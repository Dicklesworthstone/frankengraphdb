//! Partition roots: the durable object that says WHICH blocks a partition is
//! made of.
//!
//! A block knows its own entries and nothing else. A root is what turns a pile of
//! content-addressed blocks into a partition with a state: an ordered, canonical,
//! content-addressed list of block identities and the sequence range each covers.
//! Publishing a new root is how a partition advances — roots are immutable, so
//! "the partition changed" is always "a new root exists", never "a root was
//! edited".
//!
//! **BLOCKS ARE NAMED BY IDENTITY, NOT BY PATH**, which is the entire reason the
//! previous slice derived one. A root that named files could be satisfied by
//! whatever happened to be at that path; a root that names identities can be
//! checked, and [`crate::read_block`] is what checks it. A reader following a root
//! proves the bytes it found are the block the root meant.
//!
//! **BLOCK ORDER IS PUBLICATION ORDER, AND RANGES MAY OVERLAP.** A later tombstone
//! restates the version it retires, including that version's old `created_at`, so
//! its truthful visibility span necessarily overlaps the creation block. The list
//! supplies the total precedence rule: for two statements of one version, the
//! later block wins. Validation therefore requires only that each block's upper
//! sequence frontier does not regress. `first_seq` remains a conservative skip
//! bound, not an ownership claim over an exclusive slice of the commit stream.

use std::borrow::Borrow;

pub(crate) mod descriptor;

use crate::BlockError;
use crate::root_segment::{
    INDEX_BYTES, INDEX_REFS, SEGMENT_BYTES, SEGMENT_REF_LEN, SEGMENT_REFS, SegmentClass,
    SegmentEntry, SegmentError, SegmentLevel, SegmentRef, encode_index, encode_segment, index_id,
    index_span, segment_id, segment_span,
};
use fgdb_types::ids::{DatabaseSecurityNamespaceId, ObjectId};
use fgdb_types::{BranchId, CanonicalScalarResolver, CommitSeq, EId, GraphId, VId};

/// `FGSR` — FrankenGraph Strata Root.
pub const ROOT_MAGIC: [u8; 4] = *b"FGSR";
/// The retired first cut of this format, refused by name so an old root reads
/// as "a version this build does not implement" rather than "not our file".
///
/// V2 is a breaking bump (§16.6 breaking-major): the header gained the vertex
/// patch count and the refs section behind it, and a V1 reader would parse a
/// V2 root's patch section as trailing garbage. No production database
/// predates V2 — the spine's databases live in per-run scratch directories —
/// so there is deliberately no V1 decode path to maintain and drift.
pub const ROOT_FORMAT_V1: u16 = 1;
/// The retired second cut: V2's header ended at the patch count, so the root
/// was the one Tier-D object with no commitment over its own logical content.
/// V3 is a breaking bump for the same reason V2 was (§16.6, no production
/// database predates it) and there is deliberately no V2 decode path.
pub const ROOT_FORMAT_V2: u16 = 2;
/// Format version, versioned from day one (§16.6). V3 closes the header with
/// `canonical_partition_digest` (fgdb-6lyc), mirroring the block's own
/// transcript commitment.
pub const ROOT_FORMAT_V3: u16 = 3;

/// Domain separator for [`root_logical_digest`] — an UNKEYED transcript digest
/// over the root's LOGICAL content (coordinate, publication, and the ordered
/// ref lists), not its frame bytes, so a re-encoding under a future layout
/// digests identically.
pub const ROOT_LOGICAL_DIGEST_DOMAIN: &[u8] = b"fgdb.strata.root-logical-digest.v1";

// Header field offsets, written out rather than computed at each use site. The
// first draft of the decoder read `published_at` at 38 (the partition field) and
// the block count at 46 — the same arithmetic slip the block decoder made, and the
// reason both layouts now name their offsets instead of adding widths inline.
const OFF_GRAPH: usize = 6;
const OFF_BRANCH: usize = OFF_GRAPH + 16;
const OFF_PARTITION: usize = OFF_BRANCH + 16;
const OFF_PUBLISHED: usize = OFF_PARTITION + 8;
const OFF_BLOCK_COUNT: usize = OFF_PUBLISHED + 8;
const OFF_PATCH_COUNT: usize = OFF_BLOCK_COUNT + 4;
const OFF_DIGEST: usize = OFF_PATCH_COUNT + 4;
/// magic + format + graph + branch + partition + published_at + block_count
/// + vertex_patch_count + canonical_partition_digest
const HEADER_LEN: usize = OFF_DIGEST + 32;
/// block_id(32) + first_seq(8) + last_seq(8) — and identically
/// patch_id(32) + first_seq(8) + last_seq(8).
const REF_LEN: usize = 32 + 8 + 8;

/// The largest number of blocks this build will read from one root: a V5
/// root, whose three-level shape bounds its own bytes. A partition holds at
/// least one block per (source, relation) family, so this also bounds a
/// one-partition graph's source vertices; 2^20 refused roadnet-ca (1.97M
/// sources) and pokec (1.6M) at load (fgdb-u9jln).
pub const MAX_ROOT_BLOCKS: u32 = 1 << 24;
/// The largest number of vertex patches this build will read from one root.
pub const MAX_ROOT_PATCHES: u32 = 1 << 24;
/// The largest number of blocks a flat (V3) or two-level (V4) root may name.
/// Their encodings grow with every reference, so they keep the original
/// ceiling: it bounds what a damaged count can make a reader allocate.
pub const MAX_FLAT_ROOT_BLOCKS: u32 = 1 << 20;
/// The vertex-patch counterpart of [`MAX_FLAT_ROOT_BLOCKS`].
pub const MAX_FLAT_ROOT_PATCHES: u32 = 1 << 20;
/// The largest canonical flat (V3) root byte string.
///
/// Storage applies this before materializing a root object of any format; a
/// segmented root's own bytes are far smaller. Keeping the byte ceiling
/// derived beside the layout prevents the block store from drifting to a much
/// larger, block-shaped allocation bound when the durable root format changes.
pub const MAX_ENCODED_ROOT_BYTES: usize =
    HEADER_LEN + (MAX_FLAT_ROOT_BLOCKS as usize + MAX_FLAT_ROOT_PATCHES as usize) * REF_LEN;

/// One block a root names, and the sequence range it covers.
///
/// **THE RANGE IS THE BLOCK'S OWN, NOT A CLAIM ABOUT COVERAGE OF THE STREAM.**
/// `first_seq` and `last_seq` bound the sequences the block's entries mention, so a
/// reader can skip a block that cannot contain anything visible at its snapshot
/// without decoding it. That is the whole performance argument for a root, and it
/// is also why the range has to be checked against the block when the block is
/// read: a root that understated a range would make a reader skip a block that
/// mattered, silently.
///
/// Cross-block retirement uses tombstone supersede. The later block repeats the
/// original `created_at` and adds `retired_at`, so overlap between block ranges is
/// expected. The ordered root, not disjoint ranges, determines which statement of
/// that version wins.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct BlockRef {
    pub block_id: ObjectId,
    pub first_seq: CommitSeq,
    pub last_seq: CommitSeq,
}

/// One vertex patch a root names, and the sequence range it covers.
///
/// Structurally a [`BlockRef`] over a different object family; kept a distinct
/// type for the same reason [`crate::vertex::VertexPatchVersion`] is distinct
/// from [`crate::DeltaBlockVersion`] — a patch reference handed to a block
/// resolver would be answered with the wrong decoder's refusal, not a type
/// error.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct PatchRef {
    pub patch_id: ObjectId,
    pub first_seq: CommitSeq,
    pub last_seq: CommitSeq,
}

/// A partition's durable membership at one published sequence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PartitionRoot {
    pub graph: GraphId,
    pub branch: BranchId,
    pub partition: u64,
    /// The commit sequence at which this root became the partition's state.
    pub published_at: CommitSeq,
    pub blocks: Vec<BlockRef>,
    /// The vertex row patches this partition is made of, under the same
    /// publication-order and frontier laws as `blocks` (fgdb-3xoi).
    pub vertex_patches: Vec<PatchRef>,
}

/// Borrow the membership already owned by a publication memo. Keeping this
/// private to Strata lets the append path reuse the canonical encoder without
/// constructing another complete pair of reference vectors.
#[derive(Clone, Copy)]
pub(crate) struct PartitionRootRef<'a> {
    pub graph: GraphId,
    pub branch: BranchId,
    pub partition: u64,
    pub published_at: CommitSeq,
    pub blocks: &'a [BlockRef],
    pub vertex_patches: &'a [PatchRef],
}

impl<'a> From<&'a PartitionRoot> for PartitionRootRef<'a> {
    fn from(root: &'a PartitionRoot) -> Self {
        Self {
            graph: root.graph,
            branch: root.branch,
            partition: root.partition,
            published_at: root.published_at,
            blocks: &root.blocks,
            vertex_patches: &root.vertex_patches,
        }
    }
}

/// The immutable birth bound to one permanently spent EId.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EdgeBirth {
    /// The edge's immutable source.
    pub src: VId,
    /// The edge's immutable relation.
    pub relation: fgdb_delta_types::RelationId,
    /// The edge's immutable destination.
    pub dst: VId,
    /// The commit that permanently spent the EId.
    pub created_at: CommitSeq,
}

/// Both incompatible births reported when durable history reuses an EId.
///
/// Boxed inside [`RootError`] because the detailed diagnostic is needed only on
/// the failure path; embedding two full identities in every result inflated the
/// surrounding store and writer error types.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EdgeIdentityConflict {
    /// The first birth admitted for the EId.
    pub expected: EdgeBirth,
    /// The incompatible later birth.
    pub found: EdgeBirth,
}

/// Why a root could not be encoded, decoded, or resolved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RootError {
    /// Authenticated descriptor metadata has an impossible or mismatched shape.
    InvalidDescriptor,
    NotARoot,
    UnsupportedFormat {
        format: u16,
    },
    Truncated {
        expected: usize,
        found: usize,
    },
    TrailingBytes {
        extra: usize,
    },
    /// A block's own range is inverted.
    InvertedRange {
        at: usize,
        first_seq: CommitSeq,
        last_seq: CommitSeq,
    },
    /// A later block's upper sequence frontier is below its predecessor's.
    ///
    /// Overlapping lower bounds are expected under tombstone supersede, but the
    /// writer consumes rows in commit order, so the greatest sequence mentioned by
    /// successive sealed blocks may stay equal and may never move backwards.
    BlockOrderRegression {
        earlier: usize,
        later: usize,
        earlier_last_seq: CommitSeq,
        later_last_seq: CommitSeq,
    },
    /// A block claims a sequence at or after the root's own publication.
    ///
    /// A root cannot have been published before the commits it names: the root is
    /// written after the blocks it points at, so a block reaching past it means
    /// either the root is stale or the range is a lie.
    BlockAfterPublication {
        at: usize,
        last_seq: CommitSeq,
        published_at: CommitSeq,
    },
    /// A block references sequence zero, which names the empty stream.
    SequenceZero {
        at: usize,
    },
    ImplausibleBlockCount {
        declared: u32,
    },
    /// The declared `canonical_partition_digest` does not match a
    /// recomputation over the decoded logical content.
    ///
    /// Content-addressing already proves these are the bytes that were
    /// written; this proves the bytes still SAY what the publisher's logical
    /// state said — an encoder that dropped or reordered a ref would keep a
    /// stable identity for the wrong content.
    DigestMismatch {
        declared: [u8; 32],
        recomputed: [u8; 32],
    },
    /// The bytes are not the root that was asked for.
    IdentityMismatch {
        expected: ObjectId,
        actual: ObjectId,
    },
    /// A block the root names did not match what the root said about it.
    ///
    /// Distinct from [`BlockError::IdentityMismatch`]: that says the BYTES are the
    /// wrong block; this says the right block disagrees with the root's claim about
    /// its range. A root that understated a range would make a reader skip a block
    /// that mattered, and nothing about the block itself would look wrong.
    BlockRangeMismatch {
        at: usize,
        declared: (CommitSeq, CommitSeq),
        actual: (CommitSeq, CommitSeq),
    },
    /// One permanently spent EId appeared with two different births.
    ///
    /// `EId` is the stable identity, not a version-family key. Its source,
    /// relation, destination, and creation sequence are therefore immutable.
    /// A later block may only restate that exact birth to add its retirement.
    EdgeIdentityMismatch {
        eid: EId,
        conflict: Box<EdgeIdentityConflict>,
    },
    /// A later statement tried to undo or retime an EId's retirement.
    ///
    /// The only lawful state change for one exact birth is live-to-retired.
    /// Identical restatements are harmless, but resurrection and a second death
    /// sequence would both make last-block-wins fabricate a different lifetime.
    EdgeRetirementMismatch {
        eid: EId,
        expected: Option<CommitSeq>,
        found: Option<CommitSeq>,
    },
    /// Reading one of the named blocks failed.
    Block {
        at: usize,
        error: BlockError,
    },
    /// A vertex patch's own range is inverted.
    PatchInvertedRange {
        at: usize,
        first_seq: CommitSeq,
        last_seq: CommitSeq,
    },
    /// A later patch's upper sequence frontier is below its predecessor's —
    /// the same publication-order witness as [`RootError::BlockOrderRegression`].
    PatchOrderRegression {
        earlier: usize,
        later: usize,
        earlier_last_seq: CommitSeq,
        later_last_seq: CommitSeq,
    },
    /// A vertex patch claims a sequence at or after the root's own publication.
    PatchAfterPublication {
        at: usize,
        last_seq: CommitSeq,
        published_at: CommitSeq,
    },
    /// A vertex patch references sequence zero, which names the empty stream.
    PatchSequenceZero {
        at: usize,
    },
    ImplausiblePatchCount {
        declared: u32,
    },
    /// The named vertex patch did not span what the root said about it —
    /// the patch counterpart of [`RootError::BlockRangeMismatch`], and refused
    /// for the same reason: an understated range makes a reader skip rows
    /// that mattered, silently.
    PatchRangeMismatch {
        at: usize,
        declared: (CommitSeq, CommitSeq),
        actual: (CommitSeq, CommitSeq),
    },
    /// One permanently spent VId appeared with two incompatible rows.
    ///
    /// `VId` is the stable identity: its birth ordinal, creation sequence,
    /// labels, and properties are immutable once published. A later patch may
    /// only restate that exact row to add its retirement. Boxed for the same
    /// reason as [`EdgeIdentityConflict`].
    VertexIdentityMismatch {
        vid: VId,
        conflict: Box<(crate::vertex::VertexRow, crate::vertex::VertexRow)>,
    },
    /// A later statement tried to undo or retime a VId's retirement.
    VertexRetirementMismatch {
        vid: VId,
        expected: Option<CommitSeq>,
        found: Option<CommitSeq>,
    },
    /// Reading one of the named vertex patches failed.
    Patch {
        at: usize,
        error: crate::vertex::VertexPatchError,
    },
    /// [`crate::compact::compact_with_props`] was handed a property column for
    /// a different number of blocks than it was asked to compact. Guessing an
    /// alignment would silently attach rows to the wrong entries.
    BlockPropsArity {
        blocks: usize,
        props: usize,
    },
    /// A retained edge property row cannot be packed for publication.
    PropertyPatch(crate::edge_props::EdgePropertyPatchError),
    /// A root names a block whose durable `partition_id` disagrees with the
    /// root's own partition (V5, fgdb-da6b) — a transplanted block, refused
    /// at admission rather than silently merged into a foreign partition.
    BlockPartitionMismatch {
        at: usize,
        root_partition: u64,
        block_partition: u64,
    },
    /// A block's declared predecessor is not its descriptor family's previous
    /// block in this root's publication order (V6, fgdb-4391). The chain IS
    /// the publication order restricted to one family — finite, acyclic, and
    /// newer-first by construction (FG-INV-03) — so a link that skips,
    /// forges, or omits is a malformed history, not an alternative one.
    BlockChainMismatch {
        at: usize,
        declared: Option<ObjectId>,
        expected: Option<ObjectId>,
    },
    /// A V4 root's segment could not be encoded, decoded or bound to the root
    /// that names it (fgdb-d5vo4).
    Segment(SegmentError),
}

impl core::fmt::Display for RootError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::InvalidDescriptor => write!(f, "invalid authenticated root descriptor"),
            Self::NotARoot => write!(f, "not a strata partition root"),
            Self::UnsupportedFormat { format } => {
                write!(f, "root format {format} is not implemented")
            }
            Self::Truncated { expected, found } => {
                write!(f, "root declares {expected} bytes, found {found}")
            }
            Self::TrailingBytes { extra } => write!(f, "{extra} bytes after the last block"),
            Self::InvertedRange {
                at,
                first_seq,
                last_seq,
            } => write!(
                f,
                "block {at} spans {first_seq:?}..{last_seq:?}, which is empty"
            ),
            Self::BlockOrderRegression {
                earlier,
                later,
                earlier_last_seq,
                later_last_seq,
            } => write!(
                f,
                "block {later} ends at {later_last_seq:?}, before block {earlier}'s \
                 publication frontier {earlier_last_seq:?}"
            ),
            Self::BlockAfterPublication {
                at,
                last_seq,
                published_at,
            } => write!(
                f,
                "block {at} reaches {last_seq:?}, past this root's publication at {published_at:?}"
            ),
            Self::SequenceZero { at } => write!(f, "block {at} references the empty stream"),
            Self::ImplausibleBlockCount { declared } => {
                write!(f, "a root naming {declared} blocks is not readable here")
            }
            Self::IdentityMismatch { expected, actual } => {
                write!(f, "these bytes are root {actual:?}, not {expected:?}")
            }
            Self::BlockRangeMismatch {
                at,
                declared,
                actual,
            } => write!(
                f,
                "block {at} spans {actual:?} but the root declares {declared:?}"
            ),
            Self::EdgeIdentityMismatch { eid, conflict } => write!(
                f,
                "{eid:?} was born as {:?}, then appeared as {:?}; edge identities are \
                 permanently spent",
                conflict.expected, conflict.found
            ),
            Self::EdgeRetirementMismatch {
                eid,
                expected,
                found,
            } => write!(
                f,
                "{eid:?} retirement changed from {expected:?} to {found:?}; retirement is \
                 irreversible"
            ),
            Self::Block { at, error } => write!(f, "block {at}: {error}"),
            Self::PatchInvertedRange {
                at,
                first_seq,
                last_seq,
            } => write!(
                f,
                "vertex patch {at} spans {first_seq:?}..{last_seq:?}, which is empty"
            ),
            Self::PatchOrderRegression {
                earlier,
                later,
                earlier_last_seq,
                later_last_seq,
            } => write!(
                f,
                "vertex patch {later} ends at {later_last_seq:?}, before patch {earlier}'s \
                 publication frontier {earlier_last_seq:?}"
            ),
            Self::PatchAfterPublication {
                at,
                last_seq,
                published_at,
            } => write!(
                f,
                "vertex patch {at} reaches {last_seq:?}, past this root's publication at \
                 {published_at:?}"
            ),
            Self::PatchSequenceZero { at } => {
                write!(f, "vertex patch {at} references the empty stream")
            }
            Self::ImplausiblePatchCount { declared } => {
                write!(
                    f,
                    "a root naming {declared} vertex patches is not readable here"
                )
            }
            Self::DigestMismatch {
                declared,
                recomputed,
            } => write!(
                f,
                "the root's declared canonical partition digest {declared:02x?} does not match \
                 the recomputation {recomputed:02x?} over its decoded content"
            ),
            Self::PatchRangeMismatch {
                at,
                declared,
                actual,
            } => write!(
                f,
                "vertex patch {at} spans {actual:?} but the root declares {declared:?}"
            ),
            Self::VertexIdentityMismatch { vid, conflict } => write!(
                f,
                "{vid:?} was published as {:?}, then appeared as {:?}; vertex identities are \
                 permanently spent",
                conflict.0, conflict.1
            ),
            Self::VertexRetirementMismatch {
                vid,
                expected,
                found,
            } => write!(
                f,
                "{vid:?} retirement changed from {expected:?} to {found:?}; retirement is \
                 irreversible"
            ),
            Self::Patch { at, error } => write!(f, "vertex patch {at}: {error}"),
            Self::BlockPropsArity { blocks, props } => write!(
                f,
                "a property column for {props} blocks cannot align with {blocks} blocks"
            ),
            Self::PropertyPatch(error) => write!(f, "edge property packing: {error}"),
            Self::BlockPartitionMismatch {
                at,
                root_partition,
                block_partition,
            } => write!(
                f,
                "block {at} durably names partition {block_partition}, but the root is \
                 partition {root_partition}'s"
            ),
            Self::BlockChainMismatch {
                at,
                declared,
                expected,
            } => write!(
                f,
                "block {at} links predecessor {declared:?}, but its family's chain \
                 expects {expected:?}"
            ),
            Self::Segment(error) => write!(f, "root segment: {error}"),
        }
    }
}

impl core::error::Error for RootError {}

/// Validate a root's structural laws without allocating its canonical encoding.
///
/// Producers call this before an invalid root can escape; encoders and decoders
/// call the same function so publication and persistence cannot drift into two
/// definitions of lawfulness.
/// Refuse a root the flat (V3) and two-level (V4) formats cannot name: their
/// decoders refuse counts past the flat ceilings, so their encoders must too.
fn within_flat_ceilings(root: PartitionRootRef<'_>) -> Result<(), RootError> {
    let declared = u32::try_from(root.blocks.len()).unwrap_or(u32::MAX);
    if declared > MAX_FLAT_ROOT_BLOCKS {
        return Err(RootError::ImplausibleBlockCount { declared });
    }
    let declared = u32::try_from(root.vertex_patches.len()).unwrap_or(u32::MAX);
    if declared > MAX_FLAT_ROOT_PATCHES {
        return Err(RootError::ImplausiblePatchCount { declared });
    }
    Ok(())
}

pub fn validate_root(root: &PartitionRoot) -> Result<(), RootError> {
    validate_root_suffix(root.into(), (0, 0))
}

/// The caller must have validated the carried prefixes at a publication no
/// later than this root's. The first new reference still compares against the
/// final carried reference: skipping that boundary would admit a regression.
pub(crate) fn validate_root_suffix(
    root: PartitionRootRef<'_>,
    carried: (usize, usize),
) -> Result<(), RootError> {
    assert!(carried.0 <= root.blocks.len());
    assert!(carried.1 <= root.vertex_patches.len());
    let declared = u32::try_from(root.blocks.len()).unwrap_or(u32::MAX);
    if declared > MAX_ROOT_BLOCKS {
        return Err(RootError::ImplausibleBlockCount { declared });
    }
    for (index, block) in root.blocks.iter().enumerate().skip(carried.0) {
        if block.first_seq.0 == 0 || block.last_seq.0 == 0 {
            return Err(RootError::SequenceZero { at: index });
        }
        if block.last_seq.0 < block.first_seq.0 {
            return Err(RootError::InvertedRange {
                at: index,
                first_seq: block.first_seq,
                last_seq: block.last_seq,
            });
        }
        if block.last_seq.0 > root.published_at.0 {
            return Err(RootError::BlockAfterPublication {
                at: index,
                last_seq: block.last_seq,
                published_at: root.published_at,
            });
        }
        if index > 0 {
            let previous = &root.blocks[index - 1];
            // Ranges summarize visibility intervals and may overlap: a tombstone
            // repeats an old creation sequence. The upper frontier is the ordering
            // witness because rows reach the writer in commit order. Equal is
            // legal when one commit forces more than one block.
            if block.last_seq.0 < previous.last_seq.0 {
                return Err(RootError::BlockOrderRegression {
                    earlier: index - 1,
                    later: index,
                    earlier_last_seq: previous.last_seq,
                    later_last_seq: block.last_seq,
                });
            }
        }
    }
    let declared_patches = u32::try_from(root.vertex_patches.len()).unwrap_or(u32::MAX);
    if declared_patches > MAX_ROOT_PATCHES {
        return Err(RootError::ImplausiblePatchCount {
            declared: declared_patches,
        });
    }
    for (index, patch) in root.vertex_patches.iter().enumerate().skip(carried.1) {
        if patch.first_seq.0 == 0 || patch.last_seq.0 == 0 {
            return Err(RootError::PatchSequenceZero { at: index });
        }
        if patch.last_seq.0 < patch.first_seq.0 {
            return Err(RootError::PatchInvertedRange {
                at: index,
                first_seq: patch.first_seq,
                last_seq: patch.last_seq,
            });
        }
        if patch.last_seq.0 > root.published_at.0 {
            return Err(RootError::PatchAfterPublication {
                at: index,
                last_seq: patch.last_seq,
                published_at: root.published_at,
            });
        }
        if index > 0 {
            let previous = &root.vertex_patches[index - 1];
            // The same frontier witness as blocks: a retirement restates the
            // row's old creation sequence, so lower bounds may overlap while
            // the upper frontier never regresses.
            if patch.last_seq.0 < previous.last_seq.0 {
                return Err(RootError::PatchOrderRegression {
                    earlier: index - 1,
                    later: index,
                    earlier_last_seq: previous.last_seq,
                    later_last_seq: patch.last_seq,
                });
            }
        }
    }
    Ok(())
}

/// The root's canonical partition digest (fgdb-6lyc): UNKEYED,
/// domain-separated, over the LOGICAL content — coordinate, publication, and
/// the ordered ref lists with their sequence bounds. Infallible: every field
/// digests, so there is no skipped-field path for a defect to hide behind.
pub fn root_logical_digest(root: &PartitionRoot) -> [u8; 32] {
    let mut hasher = fgdb_crypto::Hasher::new();
    hasher.update(ROOT_LOGICAL_DIGEST_DOMAIN);
    hasher.update(&root.graph.0.to_be_bytes());
    hasher.update(&root.branch.0.to_be_bytes());
    hasher.update(&root.partition.to_be_bytes());
    hasher.update(&root.published_at.0.to_be_bytes());
    hasher.update(&(root.blocks.len() as u32).to_be_bytes());
    for block in &root.blocks {
        hasher.update(&block.block_id.0);
        hasher.update(&block.first_seq.0.to_be_bytes());
        hasher.update(&block.last_seq.0.to_be_bytes());
    }
    hasher.update(&(root.vertex_patches.len() as u32).to_be_bytes());
    for patch in &root.vertex_patches {
        hasher.update(&patch.patch_id.0);
        hasher.update(&patch.first_seq.0.to_be_bytes());
        hasher.update(&patch.last_seq.0.to_be_bytes());
    }
    hasher.finalize().0
}

/// Encode a root canonically, refusing anything that is not.
pub fn encode_root(root: &PartitionRoot) -> Result<Vec<u8>, RootError> {
    validate_root(root)?;
    within_flat_ceilings(root.into())?;

    let mut out =
        Vec::with_capacity(HEADER_LEN + (root.blocks.len() + root.vertex_patches.len()) * REF_LEN);
    out.extend_from_slice(&ROOT_MAGIC);
    out.extend_from_slice(&ROOT_FORMAT_V3.to_be_bytes());
    out.extend_from_slice(&root.graph.0.to_be_bytes());
    out.extend_from_slice(&root.branch.0.to_be_bytes());
    out.extend_from_slice(&root.partition.to_be_bytes());
    out.extend_from_slice(&root.published_at.0.to_be_bytes());
    out.extend_from_slice(&(root.blocks.len() as u32).to_be_bytes());
    out.extend_from_slice(&(root.vertex_patches.len() as u32).to_be_bytes());
    out.extend_from_slice(&root_logical_digest(root));
    for block in &root.blocks {
        out.extend_from_slice(&block.block_id.0);
        out.extend_from_slice(&block.first_seq.0.to_be_bytes());
        out.extend_from_slice(&block.last_seq.0.to_be_bytes());
    }
    for patch in &root.vertex_patches {
        out.extend_from_slice(&patch.patch_id.0);
        out.extend_from_slice(&patch.first_seq.0.to_be_bytes());
        out.extend_from_slice(&patch.last_seq.0.to_be_bytes());
    }
    Ok(out)
}

/// Decode a root, re-checking every law the encoder enforces.
pub fn decode_root(bytes: &[u8]) -> Result<PartitionRoot, RootError> {
    if bytes.len() < HEADER_LEN || bytes[..4] != ROOT_MAGIC {
        return Err(RootError::NotARoot);
    }
    let format = u16::from_be_bytes([bytes[4], bytes[5]]);
    if format != ROOT_FORMAT_V3 {
        return Err(RootError::UnsupportedFormat { format });
    }
    let u128_at = |at: usize| -> u128 {
        let mut buf = [0u8; 16];
        buf.copy_from_slice(&bytes[at..at + 16]);
        u128::from_be_bytes(buf)
    };
    let u64_at = |at: usize| -> u64 {
        let mut buf = [0u8; 8];
        buf.copy_from_slice(&bytes[at..at + 8]);
        u64::from_be_bytes(buf)
    };
    let u32_at = |at: usize| -> u32 {
        u32::from_be_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
    };
    // A flat root's bytes grow with its counts: the flat ceilings bound them.
    let count = u32_at(OFF_BLOCK_COUNT);
    if count > MAX_FLAT_ROOT_BLOCKS {
        return Err(RootError::ImplausibleBlockCount { declared: count });
    }
    let patch_count = u32_at(OFF_PATCH_COUNT);
    if patch_count > MAX_FLAT_ROOT_PATCHES {
        return Err(RootError::ImplausiblePatchCount {
            declared: patch_count,
        });
    }
    let expected = HEADER_LEN + (count as usize + patch_count as usize) * REF_LEN;
    if bytes.len() < expected {
        return Err(RootError::Truncated {
            expected,
            found: bytes.len(),
        });
    }
    if bytes.len() > expected {
        return Err(RootError::TrailingBytes {
            extra: bytes.len() - expected,
        });
    }

    let mut blocks = Vec::with_capacity(count as usize);
    for index in 0..count as usize {
        let at = HEADER_LEN + index * REF_LEN;
        let mut id = [0u8; 32];
        id.copy_from_slice(&bytes[at..at + 32]);
        blocks.push(BlockRef {
            block_id: ObjectId(id),
            first_seq: CommitSeq(u64_at(at + 32)),
            last_seq: CommitSeq(u64_at(at + 40)),
        });
    }
    let patches_base = HEADER_LEN + count as usize * REF_LEN;
    let mut vertex_patches = Vec::with_capacity(patch_count as usize);
    for index in 0..patch_count as usize {
        let at = patches_base + index * REF_LEN;
        let mut id = [0u8; 32];
        id.copy_from_slice(&bytes[at..at + 32]);
        vertex_patches.push(PatchRef {
            patch_id: ObjectId(id),
            first_seq: CommitSeq(u64_at(at + 32)),
            last_seq: CommitSeq(u64_at(at + 40)),
        });
    }
    let root = PartitionRoot {
        graph: GraphId(u128_at(OFF_GRAPH)),
        branch: BranchId(u128_at(OFF_BRANCH)),
        partition: u64_at(OFF_PARTITION),
        published_at: CommitSeq(u64_at(OFF_PUBLISHED)),
        blocks,
        vertex_patches,
    };
    validate_root(&root)?;
    let mut declared = [0u8; 32];
    declared.copy_from_slice(&bytes[OFF_DIGEST..OFF_DIGEST + 32]);
    let recomputed = root_logical_digest(&root);
    if declared != recomputed {
        return Err(RootError::DigestMismatch {
            declared,
            recomputed,
        });
    }
    Ok(root)
}

/// Format V4 (fgdb-d5vo4): two-level. The header is V3's, with the TOTAL
/// block and patch counts. After it come, per list, the full
/// [`SEGMENT_REFS`]-reference segments named by [`SegmentRef`], then that
/// list's remaining tail inline. A commit therefore writes and hashes the root
/// (N / 256 segment references plus a tail under 256) and only the segment it
/// fills, not every reference ever published. Additive over V3 (§16.6
/// additive-minor): V3 roots still decode, and writers emit V4.
pub const ROOT_FORMAT_V4: u16 = 4;
/// Domain separator for the V4 logical digest, which binds each full
/// segment through that segment's own logical digest.
pub const ROOT_LOGICAL_DIGEST_DOMAIN_V4: &[u8] = b"fgdb.strata.root-logical-digest.v4";
/// The largest canonical V4 root: every full segment of both lists at their
/// ceilings, plus two tails just short of a segment.
pub const MAX_ENCODED_ROOT_V4_BYTES: usize = HEADER_LEN
    + (MAX_FLAT_ROOT_BLOCKS as usize / SEGMENT_REFS
        + MAX_FLAT_ROOT_PATCHES as usize / SEGMENT_REFS)
        * SEGMENT_REF_LEN
    + 2 * (SEGMENT_REFS - 1) * REF_LEN;

/// Format V5 (fgdb-5gzaa): three-level. The header is V3's, with the TOTAL
/// block and patch counts. After it come, per list, its full index segments
/// (each naming [`INDEX_REFS`] full segments), then the full segments after
/// the last full index, then the tail inline, all named by [`SegmentRef`] as
/// in V4. A commit writes and hashes at most 15 segment references and 255
/// tail references per list, plus one index reference per 4,096 references
/// (256 at 2^20 references, 4,096 at the 2^24 ceiling), and at most one new
/// segment and one new index segment per list. Additive over V3
/// and V4 (§16.6 additive-minor): both still decode, and writers emit V5.
pub const ROOT_FORMAT_V5: u16 = 5;
/// V6 preserves V5 membership and fanout, with authenticated payload
/// descriptors in V2 leaves and bounded inline tails. Writers may emit this
/// only after complete root publication admission.
pub const ROOT_FORMAT_V6: u16 = 6;
/// Domain separator for the V5 logical digest, which binds each full index
/// segment and each named segment through that object's own logical digest.
pub const ROOT_LOGICAL_DIGEST_DOMAIN_V5: &[u8] = b"fgdb.strata.root-logical-digest.v5";
/// References one full index segment covers.
const INDEX_COVERS: usize = SEGMENT_REFS * INDEX_REFS;
/// The most references a V5 root names in one list at a ceiling: every full
/// index segment, plus segment references and a tail each just short of
/// full. Not every term peaks at once, so this bounds the root from above.
const fn max_v5_list_bytes(ceiling: u32) -> usize {
    (ceiling as usize / INDEX_COVERS + INDEX_REFS - 1) * SEGMENT_REF_LEN
        + (SEGMENT_REFS - 1) * REF_LEN
}
/// The largest canonical V5 root, from above: 682,334 bytes at any history up
/// to the count ceilings (67,934 up to 2^20 references per list).
pub const MAX_ENCODED_ROOT_V5_BYTES: usize =
    HEADER_LEN + max_v5_list_bytes(MAX_ROOT_BLOCKS) + max_v5_list_bytes(MAX_ROOT_PATCHES);

/// The most bytes one whole root read may consume: a V4, V5 or V6 root together
/// with every object it names, or a flat V3 root, whichever ceiling is
/// largest.
///
/// Each full segment costs its own header and the root's reference to it on
/// top of the flat references it carries, so a V4 root at its count ceilings
/// reads about 1.3% more bytes than [`MAX_ENCODED_ROOT_BYTES`], and a V5 root
/// adds its index segments to that. A read budget clamped to the flat ceiling
/// would refuse a lawful root.
pub const MAX_ROOT_READ_BYTES: usize = {
    let flat_segments = (MAX_FLAT_ROOT_BLOCKS as usize / SEGMENT_REFS
        + MAX_FLAT_ROOT_PATCHES as usize / SEGMENT_REFS)
        * SEGMENT_BYTES;
    let segments = (MAX_ROOT_BLOCKS as usize / SEGMENT_REFS
        + MAX_ROOT_PATCHES as usize / SEGMENT_REFS)
        .saturating_mul(crate::root_segment::MAX_SEGMENT_BYTES);
    let v4 = MAX_ENCODED_ROOT_V4_BYTES + flat_segments;
    let described = MAX_ENCODED_ROOT_V5_BYTES
        .saturating_add(32)
        .saturating_add((SEGMENT_REFS - 1) * (descriptor::BLOCK_BYTES + descriptor::PATCH_BYTES))
        .saturating_add(segments)
        .saturating_add(
            (MAX_ROOT_BLOCKS as usize / INDEX_COVERS + MAX_ROOT_PATCHES as usize / INDEX_COVERS)
                * INDEX_BYTES,
        );
    let segmented = if described > v4 { described } else { v4 };
    if segmented > MAX_ENCODED_ROOT_BYTES {
        segmented
    } else {
        MAX_ENCODED_ROOT_BYTES
    }
};

/// The full segments sealed so far for each of one root's lists, in list
/// order: `blocks[i]` chunks references `256 * i .. 256 * (i + 1)`.
///
/// The CALLER owns validity: every cached segment must chunk the prefix of
/// the list being encoded, under the same identity keys. The store keeps the
/// cache inside its root memo and discards both whenever a new root does not
/// extend the verified prefix, as a compaction's does not. An empty cache
/// encodes from scratch.
///
/// The cache owns the rest: every segment binds its root's graph, branch and
/// partition, so a root at any other coordinate re-seals from scratch rather
/// than naming segments its reader must refuse.
///
/// The full index segments (fgdb-5gzaa) chunk the segment lists the same
/// way, `block_index[j]` naming `blocks[16 * j .. 16 * (j + 1)]`, and are
/// dropped with them whenever a list is re-sealed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SegmentCache {
    coordinate: Option<(GraphId, BranchId, u64)>,
    blocks: Vec<SegmentRef>,
    patches: Vec<SegmentRef>,
    block_index: Vec<SegmentRef>,
    patch_index: Vec<SegmentRef>,
    described: bool,
    pub(crate) descriptors: Option<descriptor::RootDescriptors>,
}

impl SegmentCache {
    pub(crate) fn attach_descriptors(&mut self, descriptors: descriptor::RootDescriptors) {
        self.described = true;
        self.descriptors = Some(descriptors);
    }
    /// Identities retained by the already-admitted root, independent of any
    /// later damage to its on-disk frame or either reference list, each with
    /// the kind of segment it names.
    pub(crate) fn object_ids(&self) -> impl Iterator<Item = (ObjectId, SegmentLevel)> + '_ {
        let leaves = self
            .blocks
            .iter()
            .chain(&self.patches)
            .map(|reference| (reference.segment_id, SegmentLevel::Leaf));
        let indexes = self
            .block_index
            .iter()
            .chain(&self.patch_index)
            .map(|reference| (reference.segment_id, SegmentLevel::Index));
        leaves.chain(indexes)
    }

    /// The cache an encoder that sealed `frame`'s segments would hold. It is
    /// valid for exactly the root assembled from this frame, once each named
    /// segment has been read and proven against its reference: the decoder
    /// derives the segment count from the list total (`count / SEGMENT_REFS`),
    /// and every segment decodes to exactly [`SEGMENT_REFS`] references, so
    /// these are the segments [`encode_root_v5`] would seal from scratch.
    ///
    /// A V5 root's frame is first flattened ([`RootFrameV5::flatten`]) once
    /// its index segments are read and proven, and its index references are
    /// passed here: every index decodes to exactly [`INDEX_REFS`] segment
    /// references, so these are also the index segments [`encode_root_v5`]
    /// would seal from scratch. A V4 frame passes none, and the first V5
    /// encode seals them.
    pub(crate) fn of_frame(
        frame: &RootFrameV4,
        block_index: Vec<SegmentRef>,
        patch_index: Vec<SegmentRef>,
    ) -> Self {
        Self {
            coordinate: Some(frame.coordinate()),
            blocks: frame.block_segments.clone(),
            patches: frame.patch_segments.clone(),
            block_index,
            patch_index,
            described: false,
            descriptors: None,
        }
    }
}

/// A V4 root's own content, before its segments are read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RootFrameV4 {
    pub graph: GraphId,
    pub branch: BranchId,
    pub partition: u64,
    pub published_at: CommitSeq,
    pub block_segments: Vec<SegmentRef>,
    pub tail_blocks: Vec<BlockRef>,
    pub patch_segments: Vec<SegmentRef>,
    pub tail_patches: Vec<PatchRef>,
}

impl RootFrameV4 {
    /// The coordinate every named segment must carry.
    pub fn coordinate(&self) -> (GraphId, BranchId, u64) {
        (self.graph, self.branch, self.partition)
    }
}

/// A V5 root's own content, before its index segments and segments are read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RootFrameV5 {
    pub graph: GraphId,
    pub branch: BranchId,
    pub partition: u64,
    pub published_at: CommitSeq,
    pub block_index: Vec<SegmentRef>,
    pub block_segments: Vec<SegmentRef>,
    pub tail_blocks: Vec<BlockRef>,
    pub patch_index: Vec<SegmentRef>,
    pub patch_segments: Vec<SegmentRef>,
    pub tail_patches: Vec<PatchRef>,
}

impl RootFrameV5 {
    /// The coordinate every named index segment and segment must carry.
    pub fn coordinate(&self) -> (GraphId, BranchId, u64) {
        (self.graph, self.branch, self.partition)
    }

    /// The V4 frame naming every full segment, once each index segment has
    /// been proven by [`crate::root_segment::read_index`] to be the one this
    /// frame names: `block_children[j]` is the segment list of
    /// `block_index[j]`. A V5 root's segments are then read, and the root
    /// assembled, exactly as a V4 root's are.
    pub fn flatten(
        self,
        block_children: Vec<Vec<SegmentRef>>,
        patch_children: Vec<Vec<SegmentRef>>,
    ) -> RootFrameV4 {
        RootFrameV4 {
            graph: self.graph,
            branch: self.branch,
            partition: self.partition,
            published_at: self.published_at,
            block_segments: block_children
                .into_iter()
                .flatten()
                .chain(self.block_segments)
                .collect(),
            tail_blocks: self.tail_blocks,
            patch_segments: patch_children
                .into_iter()
                .flatten()
                .chain(self.patch_segments)
                .collect(),
            tail_patches: self.tail_patches,
        }
    }
}

/// A decoded root frame: a V3 root is complete in itself, a V4 root still
/// needs its segments, and a V5 root its index segments and then those.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RootFrame {
    V3(PartitionRoot),
    V4(RootFrameV4),
    V5(RootFrameV5),
    V6(RootFrameV6),
}

/// V6 membership and authenticated inline descriptor tails. The store compares
/// these with fully admitted payloads; an exact live admission receipt is
/// required before they can exclude payload I/O. Root identity alone is not
/// evidence that its descriptor or cross-object history claims are true.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RootFrameV6 {
    pub membership: RootFrameV5,
    pub(crate) descriptors: descriptor::RootDescriptors,
}

/// An encoded segmented root and the segments and index segments it names
/// that the cache did not already hold. Those must be published, before the
/// root, in the same batch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EncodedRoot {
    pub bytes: Vec<u8>,
    pub new_segments: Vec<(ObjectId, Vec<u8>)>,
    pub new_index_segments: Vec<(ObjectId, Vec<u8>)>,
}

/// One list of a segmented root as its logical digest sees it: the total
/// count, the full index segments (none in V4), the full segments after
/// them, and the inline tail.
struct ListDigest<'a> {
    count: usize,
    index: &'a [SegmentRef],
    segments: &'a [SegmentRef],
    tail: &'a [SegmentEntry],
}

/// The V4 and V5 logical digest: UNKEYED, domain-separated, over the
/// coordinate, publication, and per list its total count, each full index
/// segment's and then each named segment's logical digest and span, and the
/// tail references. Each of those digests binds its object's content, so this
/// binds the root's complete logical content. A V4 list names no index
/// segments, so its digest is the V4 transcript unchanged.
fn root_logical_digest_segmented(
    domain: &[u8],
    coordinate: (GraphId, BranchId, u64),
    published_at: CommitSeq,
    lists: [ListDigest<'_>; 2],
) -> [u8; 32] {
    let (graph, branch, partition) = coordinate;
    let mut hasher = fgdb_crypto::Hasher::new();
    hasher.update(domain);
    hasher.update(&graph.0.to_be_bytes());
    hasher.update(&branch.0.to_be_bytes());
    hasher.update(&partition.to_be_bytes());
    hasher.update(&published_at.0.to_be_bytes());
    for list in lists {
        hasher.update(&(list.count as u32).to_be_bytes());
        for segment in list.index.iter().chain(list.segments) {
            hasher.update(&segment.digest);
            hasher.update(&segment.first_seq.0.to_be_bytes());
            hasher.update(&segment.last_seq.0.to_be_bytes());
        }
        for entry in list.tail {
            hasher.update(&entry.id.0);
            hasher.update(&entry.first_seq.0.to_be_bytes());
            hasher.update(&entry.last_seq.0.to_be_bytes());
        }
    }
    hasher.finalize().0
}

/// Seal every full segment of `refs` the cache does not hold yet. Returns
/// whether the cache was dropped first, which drops its index segments too.
fn seal_full_segments<R: Copy + Into<SegmentEntry>>(
    refs: &[R],
    cache: &mut Vec<SegmentRef>,
    coordinate: (GraphId, BranchId, u64),
    class: SegmentClass,
    identity: (&[u8; 32], DatabaseSecurityNamespaceId),
    new_segments: &mut Vec<(ObjectId, Vec<u8>)>,
    descriptors: Option<&descriptor::RootDescriptors>,
) -> Result<bool, RootError> {
    let full = refs.len() / SEGMENT_REFS;
    let dropped = cache.len() > full;
    if dropped {
        // The list shrank, so it was rewritten: nothing cached still chunks it.
        cache.clear();
    }
    for index in cache.len()..full {
        let entries: Vec<SegmentEntry> = refs[index * SEGMENT_REFS..(index + 1) * SEGMENT_REFS]
            .iter()
            .map(|reference| (*reference).into())
            .collect();
        let (graph, branch, partition) = coordinate;
        let (bytes, digest) = if let Some(descriptors) = descriptors {
            let mut payload = Vec::new();
            descriptors.encode_range(
                class,
                index * SEGMENT_REFS..(index + 1) * SEGMENT_REFS,
                &mut payload,
            )?;
            crate::root_segment::encode_described_segment(coordinate, class, &entries, &payload)
        } else {
            encode_segment(graph, branch, partition, class, &entries)
        }
        .map_err(RootError::Segment)?;
        let segment_id = segment_id(identity.0, identity.1, &bytes);
        let (first_seq, last_seq) = segment_span(&entries);
        cache.push(SegmentRef {
            segment_id,
            digest,
            first_seq,
            last_seq,
        });
        new_segments.push((segment_id, bytes));
    }
    Ok(dropped)
}

/// Seal every full index segment over `segments` the cache does not hold yet.
fn seal_full_index(
    segments: &[SegmentRef],
    cache: &mut Vec<SegmentRef>,
    coordinate: (GraphId, BranchId, u64),
    class: SegmentClass,
    identity: (&[u8; 32], DatabaseSecurityNamespaceId),
    new_index_segments: &mut Vec<(ObjectId, Vec<u8>)>,
) -> Result<(), RootError> {
    let full = segments.len() / INDEX_REFS;
    if cache.len() > full {
        cache.clear();
    }
    for index in cache.len()..full {
        let named = &segments[index * INDEX_REFS..(index + 1) * INDEX_REFS];
        let (graph, branch, partition) = coordinate;
        let (bytes, digest) =
            encode_index(graph, branch, partition, class, named).map_err(RootError::Segment)?;
        let segment_id = index_id(identity.0, identity.1, &bytes);
        let (first_seq, last_seq) = index_span(named);
        cache.push(SegmentRef {
            segment_id,
            digest,
            first_seq,
            last_seq,
        });
        new_index_segments.push((segment_id, bytes));
    }
    Ok(())
}

/// The two segmented layouts: V4 names every full segment, V5 names full
/// index segments first.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Segmented {
    V4,
    V5,
}

/// New segments and new index segments, in sealing order.
type Sealed = (Vec<(ObjectId, Vec<u8>)>, Vec<(ObjectId, Vec<u8>)>);

/// Seal one list's new full segments and, in V5, its new full index
/// segments. Only the objects a commit newly fills are encoded and hashed.
fn seal_list<R: Copy + Into<SegmentEntry>>(
    refs: &[R],
    (segments, index): (&mut Vec<SegmentRef>, &mut Vec<SegmentRef>),
    (coordinate, class): ((GraphId, BranchId, u64), SegmentClass),
    identity: (&[u8; 32], DatabaseSecurityNamespaceId),
    format: Segmented,
    (new_segments, new_index_segments): &mut Sealed,
    descriptors: Option<&descriptor::RootDescriptors>,
) -> Result<(), RootError> {
    if seal_full_segments(
        refs,
        segments,
        coordinate,
        class,
        identity,
        new_segments,
        descriptors,
    )? {
        index.clear();
    }
    if format == Segmented::V5 {
        seal_full_index(
            segments,
            index,
            coordinate,
            class,
            identity,
            new_index_segments,
        )?;
    }
    Ok(())
}

/// Encode a root in format V4, sealing only the full segments `cache` does
/// not already hold. See [`SegmentCache`] for the caller's validity duty.
/// Writers emit V5; V4 stays encodable so its decode path stays proven.
pub fn encode_root_v4(
    root: &PartitionRoot,
    k_oid: &[u8; 32],
    namespace: DatabaseSecurityNamespaceId,
    cache: &mut SegmentCache,
) -> Result<EncodedRoot, RootError> {
    validate_root(root)?;
    within_flat_ceilings(root.into())?;
    encode_segmented(root.into(), (k_oid, namespace), cache, Segmented::V4, None)
}

/// Encode a root in format V5, sealing only the full segments and full index
/// segments `cache` does not already hold. See [`SegmentCache`] for the
/// caller's validity duty.
pub fn encode_root_v5(
    root: &PartitionRoot,
    k_oid: &[u8; 32],
    namespace: DatabaseSecurityNamespaceId,
    cache: &mut SegmentCache,
) -> Result<EncodedRoot, RootError> {
    validate_root(root)?;
    encode_segmented(root.into(), (k_oid, namespace), cache, Segmented::V5, None)
}

/// Encode an extension of an authenticated publication memo. The store alone
/// supplies `carried`, after checking the exact prior root identity, coordinate,
/// reference counts and nonregressing publication. Everything after that prefix,
/// including both append boundaries, takes the ordinary structural checks.
#[cfg(test)]
pub(crate) fn encode_root_v5_append(
    root: PartitionRootRef<'_>,
    carried: (usize, usize),
    k_oid: &[u8; 32],
    namespace: DatabaseSecurityNamespaceId,
    cache: &mut SegmentCache,
) -> Result<EncodedRoot, RootError> {
    validate_root_suffix(root, carried)?;
    encode_segmented(root, (k_oid, namespace), cache, Segmented::V5, None)
}

pub(crate) fn encode_root_v6_append(
    root: PartitionRootRef<'_>,
    carried: (usize, usize),
    descriptors: &descriptor::RootDescriptors,
    identity: (&[u8; 32], DatabaseSecurityNamespaceId),
    cache: &mut SegmentCache,
) -> Result<EncodedRoot, RootError> {
    validate_root_suffix(root, carried)?;
    if !descriptors.matches_suffix(root.blocks, root.vertex_patches, carried) {
        return Err(RootError::InvalidDescriptor);
    }
    encode_segmented(root, identity, cache, Segmented::V5, Some(descriptors))
}

fn encode_segmented(
    root: PartitionRootRef<'_>,
    identity: (&[u8; 32], DatabaseSecurityNamespaceId),
    cache: &mut SegmentCache,
    format: Segmented,
    descriptors: Option<&descriptor::RootDescriptors>,
) -> Result<EncodedRoot, RootError> {
    let coordinate = (root.graph, root.branch, root.partition);
    if cache.coordinate != Some(coordinate) || cache.described != descriptors.is_some() {
        *cache = SegmentCache {
            coordinate: Some(coordinate),
            described: descriptors.is_some(),
            ..SegmentCache::default()
        };
    }
    let mut sealed = (Vec::new(), Vec::new());
    seal_list(
        root.blocks,
        (&mut cache.blocks, &mut cache.block_index),
        (coordinate, SegmentClass::Blocks),
        identity,
        format,
        &mut sealed,
        descriptors,
    )?;
    seal_list(
        root.vertex_patches,
        (&mut cache.patches, &mut cache.patch_index),
        (coordinate, SegmentClass::VertexPatches),
        identity,
        format,
        &mut sealed,
        descriptors,
    )?;
    let (new_segments, new_index_segments) = sealed;
    // What the root names directly, per list: V4 every full segment; V5 its
    // full index segments, then the full segments after the last of them.
    let (block_index, patch_index): (&[SegmentRef], &[SegmentRef]) = match format {
        Segmented::V4 => (&[], &[]),
        Segmented::V5 => (&cache.block_index, &cache.patch_index),
    };
    let block_segments = &cache.blocks[block_index.len() * INDEX_REFS..];
    let patch_segments = &cache.patches[patch_index.len() * INDEX_REFS..];
    let tails = [
        root.blocks[cache.blocks.len() * SEGMENT_REFS..]
            .iter()
            .map(|block| SegmentEntry::from(*block))
            .collect::<Vec<_>>(),
        root.vertex_patches[cache.patches.len() * SEGMENT_REFS..]
            .iter()
            .map(|patch| SegmentEntry::from(*patch))
            .collect::<Vec<_>>(),
    ];
    let (domain, version) = match format {
        Segmented::V4 => (ROOT_LOGICAL_DIGEST_DOMAIN_V4, ROOT_FORMAT_V4),
        Segmented::V5 => (ROOT_LOGICAL_DIGEST_DOMAIN_V5, ROOT_FORMAT_V5),
    };
    let digest = root_logical_digest_segmented(
        domain,
        coordinate,
        root.published_at,
        [
            ListDigest {
                count: root.blocks.len(),
                index: block_index,
                segments: block_segments,
                tail: &tails[0],
            },
            ListDigest {
                count: root.vertex_patches.len(),
                index: patch_index,
                segments: patch_segments,
                tail: &tails[1],
            },
        ],
    );
    let named = block_index.len() + block_segments.len() + patch_index.len() + patch_segments.len();
    let mut out = Vec::with_capacity(
        HEADER_LEN + named * SEGMENT_REF_LEN + (tails[0].len() + tails[1].len()) * REF_LEN,
    );
    out.extend_from_slice(&ROOT_MAGIC);
    out.extend_from_slice(&version.to_be_bytes());
    out.extend_from_slice(&root.graph.0.to_be_bytes());
    out.extend_from_slice(&root.branch.0.to_be_bytes());
    out.extend_from_slice(&root.partition.to_be_bytes());
    out.extend_from_slice(&root.published_at.0.to_be_bytes());
    out.extend_from_slice(&(root.blocks.len() as u32).to_be_bytes());
    out.extend_from_slice(&(root.vertex_patches.len() as u32).to_be_bytes());
    out.extend_from_slice(&digest);
    for ((index, segments), tail) in [(block_index, block_segments), (patch_index, patch_segments)]
        .into_iter()
        .zip(&tails)
    {
        for segment in index.iter().chain(segments) {
            out.extend_from_slice(&segment.segment_id.0);
            out.extend_from_slice(&segment.digest);
            out.extend_from_slice(&segment.first_seq.0.to_be_bytes());
            out.extend_from_slice(&segment.last_seq.0.to_be_bytes());
        }
        for entry in tail {
            out.extend_from_slice(&entry.id.0);
            out.extend_from_slice(&entry.first_seq.0.to_be_bytes());
            out.extend_from_slice(&entry.last_seq.0.to_be_bytes());
        }
    }
    if let Some(descriptors) = descriptors {
        let mut payload = Vec::new();
        descriptors.encode_range(
            SegmentClass::Blocks,
            cache.blocks.len() * SEGMENT_REFS..root.blocks.len(),
            &mut payload,
        )?;
        descriptors.encode_range(
            SegmentClass::VertexPatches,
            cache.patches.len() * SEGMENT_REFS..root.vertex_patches.len(),
            &mut payload,
        )?;
        let described = described_root_digest(digest, &payload);
        out[4..6].copy_from_slice(&ROOT_FORMAT_V6.to_be_bytes());
        out[OFF_DIGEST..OFF_DIGEST + 32].copy_from_slice(&described);
        out.extend_from_slice(&digest);
        out.extend_from_slice(&payload);
    }
    Ok(EncodedRoot {
        bytes: out,
        new_segments,
        new_index_segments,
    })
}

/// Decode a root frame of any live format. A V3 root is fully decoded; a V4
/// or V5 frame is checked against its own digest, and the objects it names
/// are the caller's to read (a V5 frame's index segments first, then
/// [`RootFrameV5::flatten`]) and [`assemble_root`].
pub fn decode_root_frame(bytes: &[u8]) -> Result<RootFrame, RootError> {
    if bytes.len() < HEADER_LEN || bytes[..4] != ROOT_MAGIC {
        return Err(RootError::NotARoot);
    }
    let format = u16::from_be_bytes([bytes[4], bytes[5]]);
    if format == ROOT_FORMAT_V6 {
        return decode_root_v6(bytes).map(RootFrame::V6);
    }
    if format == ROOT_FORMAT_V3 {
        return decode_root(bytes).map(RootFrame::V3);
    }
    let format = match format {
        ROOT_FORMAT_V4 => Segmented::V4,
        ROOT_FORMAT_V5 => Segmented::V5,
        _ => return Err(RootError::UnsupportedFormat { format }),
    };
    let u128_at = |at: usize| -> u128 {
        let mut buf = [0u8; 16];
        buf.copy_from_slice(&bytes[at..at + 16]);
        u128::from_be_bytes(buf)
    };
    let u64_at = |at: usize| -> u64 {
        let mut buf = [0u8; 8];
        buf.copy_from_slice(&bytes[at..at + 8]);
        u64::from_be_bytes(buf)
    };
    let u32_at = |at: usize| -> u32 {
        u32::from_be_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
    };
    let bytes32_at = |at: usize| -> [u8; 32] {
        let mut buf = [0u8; 32];
        buf.copy_from_slice(&bytes[at..at + 32]);
        buf
    };
    // A V4 root's reference lists grow with its counts, as a flat root's do;
    // only V5's index level keeps the root bounded past the flat ceilings.
    let (block_ceiling, patch_ceiling) = match format {
        Segmented::V4 => (MAX_FLAT_ROOT_BLOCKS, MAX_FLAT_ROOT_PATCHES),
        Segmented::V5 => (MAX_ROOT_BLOCKS, MAX_ROOT_PATCHES),
    };
    let count = u32_at(OFF_BLOCK_COUNT);
    if count > block_ceiling {
        return Err(RootError::ImplausibleBlockCount { declared: count });
    }
    let patch_count = u32_at(OFF_PATCH_COUNT);
    if patch_count > patch_ceiling {
        return Err(RootError::ImplausiblePatchCount {
            declared: patch_count,
        });
    }
    let (count, patch_count) = (count as usize, patch_count as usize);
    // Each list's shape is a pure function of its count: how many full index
    // segments, full segments after them, and tail references the root names.
    let shape = |count: usize| match format {
        Segmented::V4 => (0, count / SEGMENT_REFS, count % SEGMENT_REFS),
        Segmented::V5 => (
            count / INDEX_COVERS,
            (count / SEGMENT_REFS) % INDEX_REFS,
            count % SEGMENT_REFS,
        ),
    };
    let lists = [shape(count), shape(patch_count)];
    let list_len = |(index, segments, tail): (usize, usize, usize)| {
        (index + segments) * SEGMENT_REF_LEN + tail * REF_LEN
    };
    let expected = HEADER_LEN + lists.iter().map(|list| list_len(*list)).sum::<usize>();
    if bytes.len() < expected {
        return Err(RootError::Truncated {
            expected,
            found: bytes.len(),
        });
    }
    if bytes.len() > expected {
        return Err(RootError::TrailingBytes {
            extra: bytes.len() - expected,
        });
    }
    // Each list is its full index segments' references, its named full
    // segments' references, then its inline tail.
    let refs_at = |at: usize, count: usize| -> Vec<SegmentRef> {
        (0..count)
            .map(|index| {
                let at = at + index * SEGMENT_REF_LEN;
                SegmentRef {
                    segment_id: ObjectId(bytes32_at(at)),
                    digest: bytes32_at(at + 32),
                    first_seq: CommitSeq(u64_at(at + 64)),
                    last_seq: CommitSeq(u64_at(at + 72)),
                }
            })
            .collect()
    };
    let list_at = |at: usize, (index, segments, tail): (usize, usize, usize)| {
        let index_refs = refs_at(at, index);
        let named = refs_at(at + index * SEGMENT_REF_LEN, segments);
        let tail_base = at + (index + segments) * SEGMENT_REF_LEN;
        let inline: Vec<SegmentEntry> = (0..tail)
            .map(|index| {
                let at = tail_base + index * REF_LEN;
                SegmentEntry {
                    id: ObjectId(bytes32_at(at)),
                    first_seq: CommitSeq(u64_at(at + 32)),
                    last_seq: CommitSeq(u64_at(at + 40)),
                }
            })
            .collect();
        (index_refs, named, inline)
    };
    let [blocks_shape, patches_shape] = lists;
    let (block_index, block_segments, tail_blocks) = list_at(HEADER_LEN, blocks_shape);
    let (patch_index, patch_segments, tail_patches) =
        list_at(HEADER_LEN + list_len(blocks_shape), patches_shape);
    let coordinate = (
        GraphId(u128_at(OFF_GRAPH)),
        BranchId(u128_at(OFF_BRANCH)),
        u64_at(OFF_PARTITION),
    );
    let published_at = CommitSeq(u64_at(OFF_PUBLISHED));
    // The digest binds counts, index and segment digests and spans, and the
    // tails: a frame whose objects later resolve still cannot have been edited.
    let declared = bytes32_at(OFF_DIGEST);
    let domain = match format {
        Segmented::V4 => ROOT_LOGICAL_DIGEST_DOMAIN_V4,
        Segmented::V5 => ROOT_LOGICAL_DIGEST_DOMAIN_V5,
    };
    let recomputed = root_logical_digest_segmented(
        domain,
        coordinate,
        published_at,
        [
            ListDigest {
                count,
                index: &block_index,
                segments: &block_segments,
                tail: &tail_blocks,
            },
            ListDigest {
                count: patch_count,
                index: &patch_index,
                segments: &patch_segments,
                tail: &tail_patches,
            },
        ],
    );
    if declared != recomputed {
        return Err(RootError::DigestMismatch {
            declared,
            recomputed,
        });
    }
    let (graph, branch, partition) = coordinate;
    let tail_blocks = tail_blocks.into_iter().map(BlockRef::from).collect();
    let tail_patches = tail_patches.into_iter().map(PatchRef::from).collect();
    Ok(match format {
        Segmented::V4 => RootFrame::V4(RootFrameV4 {
            graph,
            branch,
            partition,
            published_at,
            block_segments,
            tail_blocks,
            patch_segments,
            tail_patches,
        }),
        Segmented::V5 => RootFrame::V5(RootFrameV5 {
            graph,
            branch,
            partition,
            published_at,
            block_index,
            block_segments,
            tail_blocks,
            patch_index,
            patch_segments,
            tail_patches,
        }),
    })
}

fn described_root_digest(base: [u8; 32], descriptors: &[u8]) -> [u8; 32] {
    let mut hash = fgdb_crypto::Hasher::new();
    hash.update(b"fgdb.strata.root-logical-digest.v6");
    hash.update(&base);
    hash.update(descriptors);
    hash.finalize().0
}

fn decode_root_v6(bytes: &[u8]) -> Result<RootFrameV6, RootError> {
    let count =
        |offset| u32::from_be_bytes(bytes[offset..offset + 4].try_into().expect("root header"));
    let blocks = count(OFF_BLOCK_COUNT);
    let patches = count(OFF_PATCH_COUNT);
    if blocks > MAX_ROOT_BLOCKS {
        return Err(RootError::ImplausibleBlockCount { declared: blocks });
    }
    if patches > MAX_ROOT_PATCHES {
        return Err(RootError::ImplausiblePatchCount { declared: patches });
    }
    let list_bytes = |count: u32| {
        let count = count as usize;
        (count / INDEX_COVERS + (count / SEGMENT_REFS) % INDEX_REFS) * SEGMENT_REF_LEN
            + (count % SEGMENT_REFS) * REF_LEN
    };
    let base_len = HEADER_LEN + list_bytes(blocks) + list_bytes(patches);
    let block_tail = blocks as usize % SEGMENT_REFS;
    let patch_tail = patches as usize % SEGMENT_REFS;
    let expected =
        base_len + 32 + block_tail * descriptor::BLOCK_BYTES + patch_tail * descriptor::PATCH_BYTES;
    if bytes.len() < expected {
        return Err(RootError::Truncated {
            expected,
            found: bytes.len(),
        });
    }
    if bytes.len() > expected {
        return Err(RootError::TrailingBytes {
            extra: bytes.len() - expected,
        });
    }
    let base: [u8; 32] = bytes[base_len..base_len + 32]
        .try_into()
        .expect("checked root length");
    let payload = &bytes[base_len + 32..];
    let declared = bytes[OFF_DIGEST..OFF_DIGEST + 32]
        .try_into()
        .expect("root header");
    let recomputed = described_root_digest(base, payload);
    if declared != recomputed {
        return Err(RootError::DigestMismatch {
            declared,
            recomputed,
        });
    }
    let mut original = bytes[..base_len].to_vec();
    original[4..6].copy_from_slice(&ROOT_FORMAT_V5.to_be_bytes());
    original[OFF_DIGEST..OFF_DIGEST + 32].copy_from_slice(&base);
    let RootFrame::V5(membership) = decode_root_frame(&original)? else {
        unreachable!("V5 framing")
    };
    let entries: Vec<_> = membership
        .tail_blocks
        .iter()
        .copied()
        .map(SegmentEntry::from)
        .collect();
    let block_bytes = block_tail * descriptor::BLOCK_BYTES;
    let mut descriptors =
        descriptor::decode(SegmentClass::Blocks, &entries, &payload[..block_bytes])?;
    let entries: Vec<_> = membership
        .tail_patches
        .iter()
        .copied()
        .map(SegmentEntry::from)
        .collect();
    descriptors.append(descriptor::decode(
        SegmentClass::VertexPatches,
        &entries,
        &payload[block_bytes..],
    )?);
    Ok(RootFrameV6 {
        membership,
        descriptors,
    })
}

/// Rebuild a V4 root's flat lists from its frame and the entries of its
/// segments, each already proven by [`read_segment`] to be the one the frame
/// names, then check the whole root under every V3 law.
pub fn assemble_root(
    frame: RootFrameV4,
    block_segments: Vec<Vec<SegmentEntry>>,
    patch_segments: Vec<Vec<SegmentEntry>>,
) -> Result<PartitionRoot, RootError> {
    let blocks = block_segments
        .into_iter()
        .flatten()
        .map(BlockRef::from)
        .chain(frame.tail_blocks)
        .collect();
    let vertex_patches = patch_segments
        .into_iter()
        .flatten()
        .map(PatchRef::from)
        .chain(frame.tail_patches)
        .collect();
    let root = PartitionRoot {
        graph: frame.graph,
        branch: frame.branch,
        partition: frame.partition,
        published_at: frame.published_at,
        blocks,
        vertex_patches,
    };
    validate_root(&root)?;
    Ok(root)
}

/// Durable object kind for a Tier-D partition root — registered as
/// `DeltaPartitionRoot` (fgdb-ge6a). Until registration the root was the one
/// object whose §5.1 identity used an empty header, distinct from blocks by
/// emptiness alone; the registered kind makes the separation explicit.
pub const ROOT_OBJECT_KIND: u16 = 0x057e;

/// The content identity of an encoded root — same derivation as a block's,
/// under the root's own registered kind.
pub fn root_id(k_oid: &[u8; 32], namespace: DatabaseSecurityNamespaceId, bytes: &[u8]) -> ObjectId {
    ObjectId(
        fgdb_crypto::logical_object_id(k_oid, &namespace.0, &ROOT_OBJECT_KIND.to_le_bytes(), bytes)
            .0,
    )
}

/// Decode a root that must be the one named by `expected`.
pub fn read_root(
    k_oid: &[u8; 32],
    namespace: DatabaseSecurityNamespaceId,
    bytes: &[u8],
    expected: ObjectId,
) -> Result<PartitionRoot, RootError> {
    let actual = root_id(k_oid, namespace, bytes);
    if actual != expected {
        return Err(RootError::IdentityMismatch { expected, actual });
    }
    decode_root(bytes)
}

/// Prove one loaded block against the identity and range named by a root.
///
/// Kept crate-visible so the filesystem store can retain its own I/O error while
/// sharing the exact same format proof as the source-agnostic resolver below.
/// The encoded bytes are dropped before the next block is loaded, avoiding an
/// eager second copy of the whole partition.
pub(crate) fn resolve_block_ref(
    k_oid: &[u8; 32],
    namespace: DatabaseSecurityNamespaceId,
    at: usize,
    reference: &BlockRef,
    bytes: &[u8],
) -> Result<Vec<crate::AdjacencyEntry>, RootError> {
    let entries = crate::read_block(k_oid, namespace, bytes, reference.block_id)
        .map_err(|error| RootError::Block { at, error })?;
    check_block_span(at, reference, &entries)?;
    Ok(entries)
}

/// The root's half of proving a block: the decoded entries span exactly the
/// range the root declared for it. Shared by [`resolve_block_ref`] and the
/// store's root walk, which decodes off the async task.
pub(crate) fn check_block_span(
    at: usize,
    reference: &BlockRef,
    entries: &[crate::AdjacencyEntry],
) -> Result<(), RootError> {
    // An empty block spans nothing, so it cannot honour any declared range —
    // and a root naming one is describing a block that carries no information.
    let Some(actual) = span_of(entries) else {
        return Err(RootError::BlockRangeMismatch {
            at,
            declared: (reference.first_seq, reference.last_seq),
            actual: (CommitSeq(0), CommitSeq(0)),
        });
    };
    if actual != (reference.first_seq, reference.last_seq) {
        return Err(RootError::BlockRangeMismatch {
            at,
            declared: (reference.first_seq, reference.last_seq),
            actual,
        });
    }
    Ok(())
}

/// Load every block a root names, proving each is the block the root meant AND
/// that it spans the range the root claimed.
///
/// `load` is how the caller reaches bytes for an identity — a directory, a cache,
/// a network fetch. This function does not know or care, which is what keeps the
/// format independent of any store.
///
/// **BOTH CHECKS ARE NECESSARY AND THEY CATCH DIFFERENT LIES.**
/// [`crate::read_block`] proves the bytes are the named block. The range check
/// proves the ROOT told the truth about it — a root that understated a block's
/// range would make a reader skip a block that mattered, and nothing about the
/// block itself would look wrong. Only the pair makes a root's summary trustworthy
/// enough to skip a block on.
pub fn resolve_blocks(
    k_oid: &[u8; 32],
    namespace: DatabaseSecurityNamespaceId,
    root: &PartitionRoot,
    mut load: impl FnMut(ObjectId) -> Option<Vec<u8>>,
) -> Result<Vec<Vec<crate::AdjacencyEntry>>, RootError> {
    // `PartitionRoot` is public and can be constructed without passing through
    // `decode_root`. Resolution is therefore an admission boundary of its own:
    // block order decides tombstone precedence, so loading an invalid root first
    // would make structurally impossible history available to the merge path.
    validate_root(root)?;

    let mut out = Vec::with_capacity(root.blocks.len());
    for (index, reference) in root.blocks.iter().enumerate() {
        let bytes = load(reference.block_id).ok_or(RootError::Block {
            at: index,
            error: BlockError::NotABlock,
        })?;
        out.push(resolve_block_ref(
            k_oid, namespace, index, reference, &bytes,
        )?);
    }
    Ok(out)
}

/// The lowest and highest sequence a block's entries mention, or `None` if empty.
///
/// A retirement counts: an entry created at 3 and retired at 9 makes its block
/// reach 9, because a reader deciding whether to skip that block at sequence 9
/// needs to know the retirement is in there.
pub fn span_of(entries: &[crate::AdjacencyEntry]) -> Option<(CommitSeq, CommitSeq)> {
    let mut low = u64::MAX;
    let mut high = 0u64;
    for entry in entries {
        low = low.min(entry.created_at.0);
        high = high.max(entry.created_at.0);
        if let Some(retired) = entry.retired_at {
            high = high.max(retired.0);
        }
    }
    (!entries.is_empty()).then_some((CommitSeq(low), CommitSeq(high)))
}

// ---------------------------------------------------------------------------
// Merging across blocks
// ---------------------------------------------------------------------------

/// Merge the blocks of a partition and answer one adjacency at one sequence.
///
/// **THE CROSS-BLOCK MODEL IS TOMBSTONE SUPERSEDE, and this is where that choice
/// is made.** A block is immutable, so retiring an entry created in an EARLIER
/// block cannot edit that block: the later block carries an entry for the same
/// `(src, relation, dst, eid)` key whose interval states the retirement, and it
/// SUPERSEDES the earlier one. The alternative — every block carrying whole
/// version chains for the keys it touches — was rejected because it makes a write
/// read-modify-write: the writer would have to fetch each key's prior versions
/// before it could seal a block, which is exactly the ingest cost B2's LSM shape
/// exists to avoid. Tombstone supersede keeps writes append-only and moves the
/// work to the read, which is what an LSM trades.
///
/// **SUPERSEDE IS PER STABLE EDGE IDENTITY, NOT PER DESTINATION.** The first
/// implementation keyed the merge on `dst` alone and let the last block win. That
/// silently collapsed parallel EIds, so retiring one edge could erase its live
/// peer. The merge is keyed on `eid`: distinct EIds survive whatever topology
/// they share, while a later tombstone for the same immutable birth supersedes its
/// earlier live statement. Any change to that EId's topology or `created_at` is
/// identity reuse and is refused even when the two intervals do not overlap.
///
/// Among statements of one exact birth, the LATER BLOCK wins, because the root is an
/// ordered publication history whose upper sequence frontier never regresses.
/// Using the entry's own interval to decide would be a second ordering rule that
/// could disagree with the first, and two rules for one question is how they drift.
///
/// **THE SKIP RULE IS SOUND AND IS THE ROOT'S WHOLE PAYOFF**: a block whose
/// `first_seq` exceeds `as_of` cannot contribute anything visible, because every
/// entry in it was created after the snapshot. That includes its retirements — a
/// retirement after `as_of` leaves the superseded entry live at `as_of`, which is
/// what the earlier block already says. So skipping is not an optimization layered
/// on top of the answer; it produces the identical answer, and there is a law
/// asserting exactly that.
pub fn merge_neighbours(
    blocks: &[impl AsRef<[crate::AdjacencyEntry]>],
    src: fgdb_types::VId,
    relation: fgdb_delta_types::RelationId,
    as_of: CommitSeq,
) -> Result<Vec<fgdb_types::VId>, RootError> {
    // Validate the WHOLE supplied history before applying the adjacency filter.
    // Otherwise a malformed tombstone can move an EId to another source or
    // relation and evade comparison merely because this read did not ask for its
    // forged topology (fgdb-ghgt).
    let (entries, _) = collapse_edge_history(blocks)?;

    let mut destinations = std::collections::BTreeSet::<fgdb_types::VId>::new();
    for entry in entries
        .values()
        .filter(|entry| entry.src == src && entry.relation == relation)
        .filter(|entry| entry.visible_at(as_of))
    {
        destinations.insert(entry.dst);
    }
    Ok(destinations.into_iter().collect())
}

/// Merge the blocks of a partition and answer one edge at one sequence — the
/// point-lookup companion of [`merge_neighbours`], under the identical
/// whole-history validation and tombstone-supersede model.
pub fn merge_edge(
    blocks: &[impl AsRef<[crate::AdjacencyEntry]>],
    eid: EId,
    as_of: CommitSeq,
) -> Result<Option<crate::AdjacencyEntry>, RootError> {
    let (statements, _) = collapse_edge_history(blocks)?;
    Ok(visible_statement(&statements, eid, as_of).copied())
}

/// The at-most-one statement of `eid` visible at `as_of` — chain contiguity
/// (fgdb-ls5b) is what makes "at most one" a law rather than a hope.
fn visible_statement(
    statements: &std::collections::BTreeMap<(EId, CommitSeq), crate::AdjacencyEntry>,
    eid: EId,
    as_of: CommitSeq,
) -> Option<&crate::AdjacencyEntry> {
    statements
        .range((eid, CommitSeq(0))..=(eid, as_of))
        .next_back()
        .map(|(_, entry)| entry)
        .filter(|entry| entry.visible_at(as_of))
}

/// The SOURCES of every live edge arriving at `dst` over `relation` at
/// `as_of`, ascending — the reverse face of [`merge_neighbours`] (fgdb-x164).
///
/// **A DERIVED SCAN, HONESTLY.** §6.1's reverse runs are synchronously
/// maintained Tier-R materializations; none exist yet, and pretending with a
/// process-local reverse index would be the substitute doctrine 7 prohibits.
/// This face walks the same validated whole history the forward merge walks
/// and pays O(entries) for it, which is the truthful cost until the reverse
/// family lands.
pub fn merge_in_neighbours(
    blocks: &[impl AsRef<[crate::AdjacencyEntry]>],
    dst: fgdb_types::VId,
    relation: fgdb_delta_types::RelationId,
    as_of: CommitSeq,
) -> Result<Vec<fgdb_types::VId>, RootError> {
    let (statements, _) = collapse_edge_history(blocks)?;
    let mut sources = std::collections::BTreeSet::<fgdb_types::VId>::new();
    for entry in statements
        .values()
        .filter(|entry| entry.dst == dst && entry.relation == relation)
        .filter(|entry| entry.visible_at(as_of))
    {
        sources.insert(entry.src);
    }
    Ok(sources.into_iter().collect())
}

/// [`merge_edge`], answering the winning statement's PROPERTIES beside it
/// (fgdb-yqor). The properties ride the winning statement's own block — a
/// tombstone restated them, so the supersede model needs no cross-block
/// property lookup: find the LAST block carrying the winning statement and
/// read its locator there.
#[allow(clippy::type_complexity)]
pub fn merge_edge_with_props(
    blocks: &[impl AsRef<[crate::AdjacencyEntry]>],
    block_props: &[Option<impl Borrow<crate::edge_props::BlockProps>>],
    eid: EId,
    as_of: CommitSeq,
) -> Result<Option<(crate::AdjacencyEntry, crate::edge_props::EdgePropertyRow)>, RootError> {
    let (statements, _) = collapse_edge_history(blocks)?;
    let Some(winner) = visible_statement(&statements, eid, as_of) else {
        return Ok(None);
    };
    let winner = *winner;
    let winner = &winner;
    for (block_at, block) in blocks.iter().enumerate().rev() {
        if let Some(index) = block.as_ref().iter().position(|entry| entry == winner) {
            let props = block_props
                .get(block_at)
                .and_then(Option::as_ref)
                .map(|props| props.borrow().props_of(index))
                .unwrap_or_default();
            return Ok(Some((*winner, props)));
        }
    }
    // Unreachable for a history the collapse admitted, but never a panic on
    // a read path: answer the entry with no properties.
    Ok(Some((*winner, Vec::new())))
}

/// The row each statement's LAST copy carries, in publication order — the
/// same last-block-wins rule the entry collapse applies, over the hosted
/// columns instead. Shared by the whole-graph scan and compaction so neither
/// can drift to a second precedence rule.
///
/// One sorted list of `(statement, block, index)` positions, the last copy of
/// each statement kept, read through a cursor: both callers ask in ascending
/// statement order, and a row is cloned only for a statement asked about. The
/// ordered map this replaced inserted, and cloned a row for, every copy in the
/// history in publication order, which a keyed EId permutation makes random
/// key order: on a packed web-google (5.1M edges) that was about a quarter of
/// a writable open (fgdb-e8gr4).
pub(crate) struct WinningEdgeRows<'a, P> {
    /// Ascending by statement, one position per statement.
    positions: Vec<((EId, CommitSeq), usize, usize)>,
    next: usize,
    block_props: &'a [Option<P>],
}

impl<'a, P: Borrow<crate::edge_props::BlockProps>> WinningEdgeRows<'a, P> {
    pub(crate) fn new(
        blocks: &[impl AsRef<[crate::AdjacencyEntry]>],
        block_props: &'a [Option<P>],
    ) -> Self {
        let mut positions: Vec<_> = blocks
            .iter()
            .zip(block_props)
            .enumerate()
            .flat_map(|(block_at, (block, _))| {
                block
                    .as_ref()
                    .iter()
                    .enumerate()
                    .map(move |(index, entry)| ((entry.eid, entry.created_at), block_at, index))
            })
            .collect();
        // Within one statement the latest copy sorts first, so keeping the
        // first of each run keeps the last copy in publication order.
        positions.sort_unstable_by(|a, b| a.0.cmp(&b.0).then((b.1, b.2).cmp(&(a.1, a.2))));
        positions.dedup_by_key(|position| position.0);
        Self {
            positions,
            next: 0,
            block_props,
        }
    }

    /// The row the last copy of `key` carries; the empty row when no block
    /// holds `key`. Asked in ascending order the cursor only moves forward;
    /// any other order is answered by a search.
    pub(crate) fn row(&mut self, key: (EId, CommitSeq)) -> crate::edge_props::EdgePropertyRow {
        let at = if self.next == 0 || self.positions[self.next - 1].0 < key {
            while self
                .positions
                .get(self.next)
                .is_some_and(|position| position.0 < key)
            {
                self.next += 1;
            }
            self.next
        } else {
            self.positions.partition_point(|position| position.0 < key)
        };
        match self.positions.get(at) {
            Some(&(found, block_at, index)) if found == key => self.block_props[block_at]
                .as_ref()
                .map(|props| props.borrow().props_of(index))
                .unwrap_or_default(),
            _ => Default::default(),
        }
    }
}

/// Every edge with a visible version at `as_of`, each beside the row its
/// winning statement carries, in ascending EId order (fgdb-9k5w) — the
/// whole-graph scan a query layer starts from, under the identical
/// whole-history validation and precedence rules as every point lookup.
#[allow(clippy::type_complexity)]
pub fn merge_all_edges_with_props(
    blocks: &[impl AsRef<[crate::AdjacencyEntry]>],
    block_props: &[Option<impl Borrow<crate::edge_props::BlockProps>>],
    as_of: CommitSeq,
) -> Result<Vec<(crate::AdjacencyEntry, crate::edge_props::EdgePropertyRow)>, RootError> {
    if blocks.len() != block_props.len() {
        return Err(RootError::BlockPropsArity {
            blocks: blocks.len(),
            props: block_props.len(),
        });
    }
    let (statements, _) = collapse_edge_history(blocks)?;
    Ok(visible_with_props(&statements, blocks, block_props, as_of))
}

/// [`merge_all_edges_with_props`] over a history a root walk already admitted
/// (its validator's canonical collapse), instead of validating every block a
/// second time. `admitted` must be the collapse of exactly these `blocks` in
/// order; a writable open passes the walk's own (fgdb-e8gr4).
#[allow(clippy::type_complexity)]
pub fn merge_admitted_edges_with_props(
    admitted: &std::collections::BTreeMap<(EId, CommitSeq), crate::AdjacencyEntry>,
    blocks: &[impl AsRef<[crate::AdjacencyEntry]>],
    block_props: &[Option<impl Borrow<crate::edge_props::BlockProps>>],
    as_of: CommitSeq,
) -> Result<Vec<(crate::AdjacencyEntry, crate::edge_props::EdgePropertyRow)>, RootError> {
    if blocks.len() != block_props.len() {
        return Err(RootError::BlockPropsArity {
            blocks: blocks.len(),
            props: block_props.len(),
        });
    }
    Ok(visible_with_props(admitted, blocks, block_props, as_of))
}

/// The visible statements of a collapsed history at `as_of`, each beside the
/// row its winning statement carries, in ascending (EId, created_at) order.
fn visible_with_props(
    statements: &std::collections::BTreeMap<(EId, CommitSeq), crate::AdjacencyEntry>,
    blocks: &[impl AsRef<[crate::AdjacencyEntry]>],
    block_props: &[Option<impl Borrow<crate::edge_props::BlockProps>>],
    as_of: CommitSeq,
) -> Vec<(crate::AdjacencyEntry, crate::edge_props::EdgePropertyRow)> {
    let mut rows = WinningEdgeRows::new(blocks, block_props);
    statements
        .iter()
        .filter(|(_, entry)| entry.visible_at(as_of))
        .map(|(key, entry)| (*entry, rows.row(*key)))
        .collect()
}

/// Incremental proof that every block in one publication history agrees on EId
/// identity and lifecycle.
///
/// Root admission uses this without retaining decoded future blocks; merge and
/// compaction consume the same state into their canonical one-entry-per-EId map.
/// The canonical collapsed history: one row per content statement, keyed
/// `(eid, created_at)`, beside the superseded-statement count.
pub(crate) type CollapsedEdgeHistory = (
    std::collections::BTreeMap<(EId, CommitSeq), crate::AdjacencyEntry>,
    usize,
);

#[derive(Debug, Default)]
pub(crate) struct EdgeHistoryValidator {
    /// One chain of content STATEMENTS per EId — the FGSV V2 chain model
    /// applied to edges (fgdb-ls5b). Birth fields are immutable across an
    /// EId's whole chain; `created_at` advances only at contiguity. Kept in
    /// first-admission order and found through `index`, because every
    /// question admission asks is about one EId's chain: an entry costs one
    /// hashed probe instead of a descent through millions of ordered keys
    /// (fgdb-2n2a2, half of an open's main-task time). Only
    /// [`Self::statements`] needs EId order, and sorts once.
    chains: Vec<EdgeChain>,
    index: EIdIndex,
    statements: usize,
    seen: usize,
}

/// Open-addressed (linear probing) positions into the validator's chains.
///
/// The hash is a fixed mix, not seeded from ambient randomness: admission
/// stays deterministic. Engine-issued EIds are keyed-permutation values and
/// spread without help, but a host may also write EIds of its own choosing
/// (fgdb-yzt2b). Both halves are therefore mixed, so a structured `(a, b)`
/// identity does not fold into a few buckets, and a probe run is bounded: an
/// EId that cannot be placed within `PROBE_LIMIT` slots goes to an ordered
/// overflow. The worst EId distribution costs what the global ordered map
/// this replaced did, never a quadratic scan.
#[derive(Debug, Default)]
struct EIdIndex {
    /// `EMPTY` or a position in `chains`; the length is zero or a power of
    /// two, at most three-quarters full.
    slots: Vec<u32>,
    /// Positions of the EIds whose probe run was full when they arrived. A
    /// slot never empties between rebuilds, so such an EId is never found,
    /// and never shadowed, within its run.
    overflow: std::collections::BTreeMap<EId, u32>,
}

impl EIdIndex {
    const EMPTY: u32 = u32::MAX;
    const PROBE_LIMIT: usize = 32;

    fn bucket(eid: EId, slots: usize) -> usize {
        // A folded 64x64 multiply of the two halves, each offset by an odd
        // constant so a zero half does not zero the product.
        let low = (eid.0 as u64) ^ 0x9e37_79b9_7f4a_7c15;
        let high = ((eid.0 >> 64) as u64) ^ 0xc2b2_ae3d_27d4_eb4f;
        let product = u128::from(low) * u128::from(high);
        let mixed = (product as u64) ^ ((product >> 64) as u64);
        // `slots` is a power of two: keep the high bits.
        (mixed >> (64 - slots.trailing_zeros())) as usize
    }

    /// The position of `eid`'s chain, if it has one.
    fn find(&self, eid: EId, chains: &[EdgeChain]) -> Option<usize> {
        if self.slots.is_empty() {
            return None;
        }
        let mask = self.slots.len() - 1;
        let mut at = Self::bucket(eid, self.slots.len());
        for _ in 0..Self::PROBE_LIMIT {
            match self.slots[at] {
                Self::EMPTY => return None,
                position if chains[position as usize].eid() == eid => {
                    return Some(position as usize);
                }
                _ => at = (at + 1) & mask,
            }
        }
        self.overflow.get(&eid).map(|position| *position as usize)
    }

    /// Record that `eid`, absent until now, has its chain at `position`.
    fn insert(&mut self, eid: EId, position: usize, chains: &[EdgeChain]) {
        if (position + 1) * 4 > self.slots.len() * 3 {
            let grown = (self.slots.len() * 2).max(64);
            self.slots = vec![Self::EMPTY; grown];
            self.overflow.clear();
            for (at, known) in chains.iter().enumerate().take(position) {
                self.place(known.eid(), at);
            }
        }
        self.place(eid, position);
    }

    fn place(&mut self, eid: EId, position: usize) {
        let position = u32::try_from(position)
            .ok()
            .filter(|position| *position != Self::EMPTY)
            .expect("a root names at most 2^28 entries");
        let mask = self.slots.len() - 1;
        let mut at = Self::bucket(eid, self.slots.len());
        for _ in 0..Self::PROBE_LIMIT {
            if self.slots[at] == Self::EMPTY {
                self.slots[at] = position;
                return;
            }
            at = (at + 1) & mask;
        }
        self.overflow.insert(eid, position);
    }
}

/// One EId's content statements, ascending by `created_at`. Almost every EId
/// carries exactly one, which holds no separate allocation.
#[derive(Debug)]
enum EdgeChain {
    One(crate::AdjacencyEntry),
    Many(Vec<crate::AdjacencyEntry>),
}

impl EdgeChain {
    /// The chain's EId; a chain is never empty.
    fn eid(&self) -> EId {
        self.statements()[0].eid
    }

    fn statements(&self) -> &[crate::AdjacencyEntry] {
        match self {
            Self::One(statement) => std::slice::from_ref(statement),
            Self::Many(statements) => statements,
        }
    }

    fn statements_mut(&mut self) -> &mut [crate::AdjacencyEntry] {
        match self {
            Self::One(statement) => std::slice::from_mut(statement),
            Self::Many(statements) => statements,
        }
    }

    fn insert(&mut self, at: usize, statement: crate::AdjacencyEntry) {
        match self {
            Self::One(first) => {
                let mut statements = Vec::with_capacity(2);
                statements.push(*first);
                statements.insert(at, statement);
                *self = Self::Many(statements);
            }
            Self::Many(statements) => statements.insert(at, statement),
        }
    }
}

impl EdgeHistoryValidator {
    /// Every admitted identity once, in its first-admission order. Retired
    /// identities remain spent; content versions and restatements add no IDs.
    pub(crate) fn identities(&self) -> impl ExactSizeIterator<Item = EId> + '_ {
        self.chains.iter().map(EdgeChain::eid)
    }

    /// Admit one block at its publication position.
    pub(crate) fn observe_block(
        &mut self,
        block_at: usize,
        block: &[crate::AdjacencyEntry],
    ) -> Result<(), RootError> {
        self.observe_block_positions(block_at, block, |_, _| {})
    }

    /// The same admission, reporting the location of each accepted chain's
    /// latest content statement. The empty observer above compiles out of
    /// ordinary admission; a writable reopen uses it to retain one coordinate
    /// per EId instead of sorting every historical statement a second time.
    fn observe_block_positions(
        &mut self,
        block_at: usize,
        block: &[crate::AdjacencyEntry],
        mut latest: impl FnMut(usize, usize),
    ) -> Result<(), RootError> {
        let declared = u32::try_from(block.len()).unwrap_or(u32::MAX);
        if declared > crate::MAX_BLOCK_ENTRIES {
            return Err(RootError::Block {
                at: block_at,
                error: BlockError::ImplausibleEntryCount { declared },
            });
        }
        let mut previous_key = None;
        for (entry_at, entry) in block.iter().enumerate() {
            crate::validate_entry(entry_at, entry).map_err(|error| RootError::Block {
                at: block_at,
                error,
            })?;
            let found_key = (
                entry.src,
                entry.relation,
                entry.dst,
                entry.eid,
                entry.created_at,
            );
            if previous_key.is_some_and(|previous| previous >= found_key) {
                return Err(RootError::Block {
                    at: block_at,
                    error: BlockError::NonCanonicalOrder { at: entry_at },
                });
            }
            previous_key = Some(found_key);
            self.seen += 1;
            let birth = |entry: &crate::AdjacencyEntry| EdgeBirth {
                src: entry.src,
                relation: entry.relation,
                dst: entry.dst,
                created_at: entry.created_at,
            };
            // Most EIds carry one statement: an unknown EId has no
            // restatement, predecessor or successor to check, and costs only
            // its insert.
            let Some(position) = self.index.find(entry.eid, &self.chains) else {
                let position = self.chains.len();
                self.chains.push(EdgeChain::One(*entry));
                self.index.insert(entry.eid, position, &self.chains);
                self.statements += 1;
                latest(position, entry_at);
                continue;
            };
            let chain = &mut self.chains[position];
            let statements = chain.statements();
            match statements.binary_search_by_key(&entry.created_at, |known| known.created_at) {
                Ok(at) => {
                    // A restatement of one exact statement: birth fields must
                    // byte-match, and the only lawful change is
                    // live-to-retired.
                    let existing = statements[at];
                    if birth(&existing) != birth(entry) {
                        return Err(RootError::EdgeIdentityMismatch {
                            eid: entry.eid,
                            conflict: Box::new(EdgeIdentityConflict {
                                expected: birth(&existing),
                                found: birth(entry),
                            }),
                        });
                    }
                    if existing.retired_at.is_some() && entry.retired_at != existing.retired_at {
                        return Err(RootError::EdgeRetirementMismatch {
                            eid: entry.eid,
                            expected: existing.retired_at,
                            found: entry.retired_at,
                        });
                    }
                    chain.statements_mut()[at] = *entry;
                    if at + 1 == chain.statements().len() {
                        latest(position, entry_at);
                    }
                }
                Err(at) => {
                    // A NEW statement must extend its EId's chain contiguously
                    // — begin exactly where the predecessor retired (a gap is
                    // a resurrection, an overlap is aliasing) and carry the
                    // SAME topology, since (src, relation, dst) is immutable
                    // for the chain's whole life; only content versions
                    // advance (fgdb-ls5b, the FGSV V2 law on edges).
                    if let Some(predecessor) = at.checked_sub(1).map(|before| &statements[before])
                        && (predecessor.retired_at != Some(entry.created_at)
                            || (predecessor.src, predecessor.relation, predecessor.dst)
                                != (entry.src, entry.relation, entry.dst))
                    {
                        return Err(RootError::EdgeIdentityMismatch {
                            eid: entry.eid,
                            conflict: Box::new(EdgeIdentityConflict {
                                expected: birth(predecessor),
                                found: birth(entry),
                            }),
                        });
                    }
                    if let Some(successor) = statements.get(at)
                        && (entry.retired_at != Some(successor.created_at)
                            || (successor.src, successor.relation, successor.dst)
                                != (entry.src, entry.relation, entry.dst))
                    {
                        return Err(RootError::EdgeIdentityMismatch {
                            eid: entry.eid,
                            conflict: Box::new(EdgeIdentityConflict {
                                expected: birth(entry),
                                found: birth(successor),
                            }),
                        });
                    }
                    chain.insert(at, *entry);
                    self.statements += 1;
                    if at + 1 == chain.statements().len() {
                        latest(position, entry_at);
                    }
                }
            }
        }
        Ok(())
    }

    /// The canonical collapsed history admitted so far, one row per content
    /// statement keyed `(eid, created_at)`: exactly what
    /// [`collapse_edge_history`] would rebuild from the same blocks in the
    /// same order. Built on request; admission itself never needs it.
    pub(crate) fn statements(
        &self,
    ) -> std::collections::BTreeMap<(EId, CommitSeq), crate::AdjacencyEntry> {
        // Chains are in admission order: sort them by EId once. Each chain is
        // ascending by created_at, so the pairs then arrive sorted and the
        // map is bulk-built. Each chain's EId is read once, in order, beside
        // its position: sorting references by a key read through them made
        // every comparison a random read of the chains (6 s of a pokec open,
        // 30.6M chains). One EId per chain, so the order is the same.
        // (An (EId, usize) pair pads to the 32 bytes an (EId, u32) would.)
        let mut order: Vec<(EId, usize)> = self
            .chains
            .iter()
            .enumerate()
            .map(|(at, chain)| (chain.eid(), at))
            .collect();
        order.sort_unstable();
        order
            .into_iter()
            .flat_map(|(_, at)| self.chains[at].statements())
            .map(|statement| ((statement.eid, statement.created_at), *statement))
            .collect()
    }

    fn into_canonical(self) -> CollapsedEdgeHistory {
        let superseded = self.seen - self.statements;
        (self.statements(), superseded)
    }
}

/// Latest-statement coordinates for one fully admitted root walk. This is
/// temporary reconstruction state, never a second history validator or a
/// durable reference. It costs eight bytes per EId, regardless of the number
/// of statements/restatements, and is dropped when the walk becomes publish
/// receipts. Read-only walks and ordinary publication do not allocate it.
#[derive(Debug, Default)]
pub(crate) struct LatestEdgeRows {
    positions: Vec<(u32, u32)>,
}

impl LatestEdgeRows {
    pub(crate) fn observe_block(
        &mut self,
        history: &mut EdgeHistoryValidator,
        block_at: usize,
        block: &[crate::AdjacencyEntry],
    ) -> Result<(), RootError> {
        history.observe_block_positions(block_at, block, |chain, row| {
            // A structurally admitted root holds at most MAX_ROOT_BLOCKS
            // blocks, each with MAX_BLOCK_ENTRIES rows; both fit u32.
            let position = (
                u32::try_from(block_at).expect("admitted root block index fits u32"),
                u32::try_from(row).expect("admitted block row index fits u32"),
            );
            if chain == self.positions.len() {
                self.positions.push(position);
            } else {
                self.positions[chain] = position;
            }
        })
    }

    /// Recover the live rows at the root's own publication frontier. All
    /// history has already been validated, and its greatest statement is the
    /// only possible live one there. Sorting only live EIds keeps the writer's
    /// exact canonical insertion order; each winning property row is cloned
    /// once from the last copy's own hosted column.
    #[allow(clippy::type_complexity)]
    pub(crate) fn live_edges(
        &self,
        history: &EdgeHistoryValidator,
        block_props: &[Option<impl Borrow<crate::edge_props::BlockProps>>],
        frontier: CommitSeq,
    ) -> Result<Vec<(crate::AdjacencyEntry, crate::edge_props::EdgePropertyRow)>, RootError> {
        assert_eq!(self.positions.len(), history.chains.len());
        let mut order = Vec::new();
        for (chain, statements) in history.chains.iter().enumerate() {
            let latest = statements.statements().last().expect("a chain is nonempty");
            let (block, _) = self.positions[chain];
            let last_seq = latest.retired_at.unwrap_or(latest.created_at);
            if last_seq > frontier {
                return Err(RootError::BlockAfterPublication {
                    at: block as usize,
                    last_seq,
                    published_at: frontier,
                });
            }
            if latest.visible_at(frontier) {
                order.push((latest.eid, chain));
            }
        }
        order.sort_unstable();
        Ok(order
            .into_iter()
            .map(|(_, chain)| {
                let entry = *history.chains[chain]
                    .statements()
                    .last()
                    .expect("a chain is nonempty");
                let (block, row) = self.positions[chain];
                let props = block_props[block as usize]
                    .as_ref()
                    .map(|props| props.borrow().props_of(row as usize))
                    .unwrap_or_default();
                (entry, props)
            })
            .collect())
    }
}

/// Validate and collapse a block publication history to one row per content
/// STATEMENT `(eid, created_at)` — the FGSV V2 chain model (fgdb-ls5b).
///
/// Later blocks may restate one exact statement to add its retirement, and the
/// later statement wins. A NEW `created_at` for a spent EId is lawful ONLY as
/// a contiguous content-version successor (same topology, beginning exactly
/// where the predecessor retired); anything else — a gap, an overlap, or a
/// topology change — is refused, which is what keeps EId reuse illegal now
/// that `created_at` is a version discriminator. Contiguity also guarantees
/// at most one statement per EId is visible at any sequence.
pub(crate) fn collapse_edge_history(
    blocks: &[impl AsRef<[crate::AdjacencyEntry]>],
) -> Result<CollapsedEdgeHistory, RootError> {
    let mut validator = EdgeHistoryValidator::default();
    for (block_at, block) in blocks.iter().enumerate() {
        validator.observe_block(block_at, block.as_ref())?;
    }
    Ok(validator.into_canonical())
}

/// Which of a root's blocks can contribute to a read at `as_of`.
///
/// The complement of the skip rule: a block whose `first_seq` is at or below
/// `as_of` must be read. Returned as indices into `root.blocks` so a caller can
/// load exactly those and no more — the reason a root carries ranges at all.
pub fn blocks_visible_at(root: &PartitionRoot, as_of: CommitSeq) -> Vec<usize> {
    root.blocks
        .iter()
        .enumerate()
        .filter(|(_, block)| block.first_seq.0 <= as_of.0)
        .map(|(index, _)| index)
        .collect()
}

/// Prove one loaded vertex patch against the identity and range a root named —
/// the patch counterpart of [`resolve_block_ref`], catching the same two lies:
/// wrong bytes, and a root that mis-stated the range of the right bytes.
pub(crate) fn resolve_patch_ref(
    k_oid: &[u8; 32],
    namespace: DatabaseSecurityNamespaceId,
    at: usize,
    reference: &PatchRef,
    bytes: &[u8],
    resolver: Option<&dyn CanonicalScalarResolver>,
) -> Result<crate::vertex::VertexPatchRows, RootError> {
    let rows = crate::vertex::read_patch_inner(
        k_oid,
        namespace,
        bytes,
        crate::vertex::VertexPatchVersion(reference.patch_id),
        resolver,
    )
    .map_err(|error| RootError::Patch { at, error })?;

    let Some(actual) = crate::vertex::span_of_rows(&rows) else {
        return Err(RootError::PatchRangeMismatch {
            at,
            declared: (reference.first_seq, reference.last_seq),
            actual: (CommitSeq(0), CommitSeq(0)),
        });
    };
    if actual != (reference.first_seq, reference.last_seq) {
        return Err(RootError::PatchRangeMismatch {
            at,
            declared: (reference.first_seq, reference.last_seq),
            actual,
        });
    }
    Ok(rows)
}

/// Cross-patch vertex history: statements keyed by `(vid, created_at)` form
/// per-vid version CHAINS — contiguous, birth-immutable, with at most one
/// retirement change per statement (fgdb-stb6). The vertex counterpart of
/// [`EdgeHistoryValidator`], enforcing FG-INV-03's finite/newer-first
/// discipline at the identity level.
#[derive(Debug, Default)]
pub(crate) struct VertexHistoryValidator {
    rows: std::collections::BTreeMap<(VId, CommitSeq), crate::vertex::VertexRow>,
}

impl VertexHistoryValidator {
    /// The already admitted collapse, in canonical identity order. Reusing it
    /// avoids rebuilding a second map from every published vertex restatement
    /// solely to recover the current writer's live rows.
    pub(crate) fn live_rows(&self, frontier: CommitSeq) -> Vec<crate::vertex::VertexRow> {
        self.rows
            .values()
            .filter(|row| row.visible_at(frontier))
            .cloned()
            .collect()
    }

    /// Every admitted identity once, including vertices no longer live.
    pub(crate) fn identities(&self) -> impl Iterator<Item = VId> + '_ {
        let mut previous = None;
        self.rows.keys().filter_map(move |&(vid, _)| {
            let first = previous != Some(vid);
            previous = Some(vid);
            first.then_some(vid)
        })
    }

    /// Admit one patch at its publication position.
    pub(crate) fn observe_patch(
        &mut self,
        patch_at: usize,
        rows: &[crate::vertex::VertexRow],
    ) -> Result<(), RootError> {
        for row in rows {
            let key = (row.vid, row.created_at);
            if let Some(existing) = self.rows.get_mut(&key) {
                // A restatement may only add retirement. Compare the complete
                // birth by reference: cloning both property trees merely to
                // clear retirement copies large nested values during open.
                // Exhaustive destructuring makes any new row field require an
                // explicit decision here instead of silently skipping it.
                let crate::vertex::VertexRow {
                    vid,
                    birth_ordinal,
                    created_at,
                    retired_at: _,
                    labels,
                    props,
                } = row;
                if existing.vid != *vid
                    || existing.birth_ordinal != *birth_ordinal
                    || existing.created_at != *created_at
                    || existing.labels != *labels
                    || existing.props != *props
                {
                    return Err(RootError::VertexIdentityMismatch {
                        vid: row.vid,
                        conflict: Box::new((existing.clone(), row.clone())),
                    });
                }
                if existing.retired_at.is_some() && row.retired_at != existing.retired_at {
                    return Err(RootError::VertexRetirementMismatch {
                        vid: row.vid,
                        expected: existing.retired_at,
                        found: row.retired_at,
                    });
                }
                // Keep the already admitted birth allocations. Replacing the
                // entire row would deep-clone the same immutable values again.
                existing.retired_at = row.retired_at;
                continue;
            } else {
                // A NEW statement must extend its vid's chain contiguously:
                // begin exactly where the predecessor retired (a gap is a
                // resurrection, an overlap is aliasing), keep the birth
                // ordinal, and — if a later statement already exists — retire
                // exactly where that successor begins.
                let predecessor = self
                    .rows
                    .range(..key)
                    .next_back()
                    .filter(|((vid, _), _)| *vid == row.vid)
                    .map(|(_, existing)| existing);
                if let Some(predecessor) = predecessor
                    && (predecessor.retired_at != Some(row.created_at)
                        || predecessor.birth_ordinal != row.birth_ordinal)
                {
                    return Err(RootError::VertexIdentityMismatch {
                        vid: row.vid,
                        conflict: Box::new((predecessor.clone(), row.clone())),
                    });
                }
                let successor = self
                    .rows
                    .range((std::ops::Bound::Excluded(key), std::ops::Bound::Unbounded))
                    .next()
                    .filter(|((vid, _), _)| *vid == row.vid)
                    .map(|(_, existing)| existing);
                if let Some(successor) = successor
                    && (row.retired_at != Some(successor.created_at)
                        || successor.birth_ordinal != row.birth_ordinal)
                {
                    return Err(RootError::VertexIdentityMismatch {
                        vid: row.vid,
                        conflict: Box::new((row.clone(), successor.clone())),
                    });
                }
            }
            self.rows.insert(key, row.clone());
        }
        // `patch_at` names the publication position for future diagnostics;
        // the per-patch structural laws were already proven by decode.
        let _ = patch_at;
        Ok(())
    }
}

/// Which of a root's vertex patches can contribute to a read at `as_of` —
/// the patch counterpart of [`blocks_visible_at`].
pub fn patches_visible_at(root: &PartitionRoot, as_of: CommitSeq) -> Vec<usize> {
    root.vertex_patches
        .iter()
        .enumerate()
        .filter(|(_, patch)| patch.first_seq.0 <= as_of.0)
        .map(|(index, _)| index)
        .collect()
}

#[cfg(test)]
mod edge_history_tests;

#[cfg(test)]
mod append_tests;

#[cfg(test)]
mod vertex_history_tests {
    use super::{RootError, VertexHistoryValidator};
    use crate::vertex::VertexRow;
    use fgdb_delta_types::{LabelId, PropertyKeyId};
    use fgdb_types::{
        CanonicalList, CanonicalMap, CanonicalMapEntry, CanonicalPropertyValue, CanonicalScalar,
        CanonicalText, CommitSeq, VId,
    };

    fn birth() -> VertexRow {
        let nested = CanonicalPropertyValue::Map(
            CanonicalMap::try_new(vec![CanonicalMapEntry::new(
                CanonicalText::new_ucs_basic("nested").unwrap(),
                CanonicalPropertyValue::List(
                    CanonicalList::try_new(vec![
                        CanonicalScalar::ucs_basic_text(&"retained payload ".repeat(8))
                            .unwrap()
                            .into(),
                        CanonicalScalar::bytes(vec![0x5a; 256]).unwrap().into(),
                    ])
                    .unwrap(),
                ),
            )])
            .unwrap(),
        );
        let row = VertexRow {
            vid: VId((1_u128 << 96) | 17),
            birth_ordinal: 31,
            created_at: CommitSeq(3),
            retired_at: None,
            labels: vec![LabelId(5), LabelId(11)],
            props: vec![(PropertyKeyId(7), nested)],
        };
        crate::vertex::admit_row_content(&row.labels, &row.props).unwrap();
        row
    }

    #[test]
    fn retirement_restatements_keep_admitted_birth_allocations() {
        let mut history = VertexHistoryValidator::default();
        let mut row = birth();
        let key = (row.vid, row.created_at);
        history
            .observe_patch(0, core::slice::from_ref(&row))
            .unwrap();
        let labels = history.rows[&key].labels.as_ptr();
        let props = history.rows[&key].props.as_ptr();
        assert_ne!(labels, row.labels.as_ptr());
        assert_ne!(props, row.props.as_ptr());

        for patch in 1..65 {
            if patch == 32 {
                row.retired_at = Some(CommitSeq(9));
            }
            history
                .observe_patch(patch, core::slice::from_ref(&row))
                .unwrap();
            let admitted = &history.rows[&key];
            assert_eq!(admitted, &row);
            assert_eq!(admitted.labels.as_ptr(), labels);
            assert_eq!(admitted.props.as_ptr(), props);
        }
        let accepted = row.clone();
        row.labels.clear();
        row.props.clear();
        assert_eq!(
            history.rows[&key], accepted,
            "input stays independently owned"
        );
    }

    #[test]
    fn restatement_errors_preserve_exact_conflicts_and_accepted_prefix() {
        let mut retired = birth();
        retired.retired_at = Some(CommitSeq(9));
        let mutations: [fn(&mut VertexRow); 4] = [
            |row| row.birth_ordinal += 1,
            |row| row.labels.push(LabelId(13)),
            |row| row.props[0].1 = CanonicalScalar::Int(-1).into(),
            |row| {
                row.labels.clear();
                row.retired_at = None;
            },
        ];
        for mutate in mutations {
            let mut history = VertexHistoryValidator::default();
            history
                .observe_patch(0, core::slice::from_ref(&retired))
                .unwrap();
            let mut malformed = retired.clone();
            mutate(&mut malformed);
            let mut prefix = birth();
            prefix.vid = VId(retired.vid.0 + 1);
            let mut expected = history.rows.clone();
            expected.insert((prefix.vid, prefix.created_at), prefix.clone());
            assert_eq!(
                history.observe_patch(1, &[prefix, malformed.clone()]),
                Err(RootError::VertexIdentityMismatch {
                    vid: retired.vid,
                    conflict: Box::new((retired.clone(), malformed)),
                }),
            );
            assert_eq!(history.rows, expected);
        }

        for retirement in [None, Some(CommitSeq(10))] {
            let mut history = VertexHistoryValidator::default();
            history
                .observe_patch(0, core::slice::from_ref(&retired))
                .unwrap();
            let expected = history.rows.clone();
            let mut malformed = retired.clone();
            malformed.retired_at = retirement;
            assert_eq!(
                history.observe_patch(1, core::slice::from_ref(&malformed)),
                Err(RootError::VertexRetirementMismatch {
                    vid: retired.vid,
                    expected: retired.retired_at,
                    found: retirement,
                }),
            );
            assert_eq!(history.rows, expected);
        }
    }

    #[test]
    fn contiguous_vertex_versions_keep_their_distinct_births_in_both_orders() {
        let mut first = birth();
        first.retired_at = Some(CommitSeq(5));
        let mut second = first.clone();
        second.created_at = CommitSeq(5);
        second.retired_at = Some(CommitSeq(9));
        second.props[0].1 = CanonicalScalar::Int(41).into();
        let mut third = second.clone();
        third.created_at = CommitSeq(9);
        third.retired_at = None;
        third.labels.clear();
        third.props[0].1 = CanonicalScalar::Bool(true).into();
        let rows = [first, second, third];

        for order in [[0, 1, 2], [2, 1, 0], [1, 0, 2], [1, 2, 0]] {
            let mut history = VertexHistoryValidator::default();
            for (patch, index) in order.into_iter().enumerate() {
                history
                    .observe_patch(patch, core::slice::from_ref(&rows[index]))
                    .unwrap();
            }
            for row in &rows {
                history
                    .observe_patch(3, core::slice::from_ref(row))
                    .unwrap();
            }
            assert_eq!(
                history.rows,
                rows.iter()
                    .map(|row| ((row.vid, row.created_at), row.clone()))
                    .collect(),
            );
        }
    }

    #[test]
    fn admitted_vertex_reconstruction_matches_patch_history_and_keeps_spent_ids() {
        let mut first = birth();
        first.retired_at = Some(CommitSeq(5));
        let mut second = first.clone();
        second.created_at = CommitSeq(5);
        second.retired_at = Some(CommitSeq(9));
        second.labels.clear();
        second.props[0].1 = CanonicalScalar::Int(41).into();
        let mut current = second.clone();
        current.created_at = CommitSeq(9);
        current.retired_at = None;
        current.props[0].1 = CanonicalScalar::Bool(true).into();
        let mut retired = birth();
        retired.vid = VId(2);
        retired.birth_ordinal = 7;
        retired.retired_at = Some(CommitSeq(4));
        let rows = [first, second, current, retired];

        // The unchanged validator refuses a missing middle interval; an
        // eventual complete chain does not authorize a temporarily gapped
        // publication. Refusal leaves the accepted live row and ID intact.
        let mut gap = VertexHistoryValidator::default();
        gap.observe_patch(0, core::slice::from_ref(&rows[2]))
            .unwrap();
        assert!(matches!(
            gap.observe_patch(1, core::slice::from_ref(&rows[0])),
            Err(RootError::VertexIdentityMismatch { .. })
        ));
        assert_eq!(gap.live_rows(CommitSeq(12)), [rows[2].clone()]);
        assert_eq!(gap.identities().collect::<Vec<_>>(), [rows[2].vid]);

        for order in [[0, 1, 2, 3], [3, 2, 1, 0], [1, 3, 0, 2], [1, 2, 3, 0]] {
            let mut history = VertexHistoryValidator::default();
            let mut patches = Vec::new();
            for index in order.into_iter().chain([0, 1, 2, 3]) {
                let row = &rows[index];
                history
                    .observe_patch(patches.len(), core::slice::from_ref(row))
                    .unwrap();
                patches.extend(crate::vertex::pack_rows(vec![row.clone()]).unwrap());
            }
            for cut in 0..=12 {
                assert_eq!(
                    history.live_rows(CommitSeq(cut)),
                    crate::vertex::merge_all_vertices(&patches, CommitSeq(cut)),
                    "cut {cut}, publication order {order:?}"
                );
            }
            assert_eq!(
                history.identities().collect::<Vec<_>>(),
                [VId(2), rows[0].vid]
            );
            let expected = history.live_rows(CommitSeq(12));
            assert_eq!(expected, [rows[2].clone()]);
            let mut independent = history.live_rows(CommitSeq(12));
            independent[0].props.clear();
            assert_eq!(history.live_rows(CommitSeq(12)), expected);
            let mut malformed = rows[2].clone();
            malformed.birth_ordinal += 1;
            assert!(matches!(
                history.observe_patch(patches.len(), &[malformed]),
                Err(RootError::VertexIdentityMismatch { .. })
            ));
            assert_eq!(history.live_rows(CommitSeq(12)), expected);
            assert_eq!(
                history.identities().collect::<Vec<_>>(),
                [VId(2), rows[0].vid]
            );
        }
    }
}
