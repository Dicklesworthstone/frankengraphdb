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

use crate::BlockError;
use crate::root_segment::{
    SEGMENT_BYTES, SEGMENT_REFS, SegmentClass, SegmentEntry, SegmentError, SegmentRef,
    encode_segment, segment_id, segment_span,
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

/// The largest number of blocks this build will read from one root.
pub const MAX_ROOT_BLOCKS: u32 = 1 << 20;
/// The largest number of vertex patches this build will read from one root.
pub const MAX_ROOT_PATCHES: u32 = 1 << 20;
/// The largest canonical root byte string this format version can encode.
///
/// Storage applies this before materializing a root. Keeping the byte ceiling
/// derived beside the layout prevents the block store from drifting to a much
/// larger, block-shaped allocation bound when the durable root format changes.
pub const MAX_ENCODED_ROOT_BYTES: usize =
    HEADER_LEN + (MAX_ROOT_BLOCKS as usize + MAX_ROOT_PATCHES as usize) * REF_LEN;

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
pub fn validate_root(root: &PartitionRoot) -> Result<(), RootError> {
    let declared = u32::try_from(root.blocks.len()).unwrap_or(u32::MAX);
    if declared > MAX_ROOT_BLOCKS {
        return Err(RootError::ImplausibleBlockCount { declared });
    }
    for (index, block) in root.blocks.iter().enumerate() {
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
    for (index, patch) in root.vertex_patches.iter().enumerate() {
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
    let count = u32_at(OFF_BLOCK_COUNT);
    if count > MAX_ROOT_BLOCKS {
        return Err(RootError::ImplausibleBlockCount { declared: count });
    }
    let patch_count = u32_at(OFF_PATCH_COUNT);
    if patch_count > MAX_ROOT_PATCHES {
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
/// segment_id(32) + segment logical digest(32) + first_seq(8) + last_seq(8).
const SEGMENT_REF_LEN: usize = 32 + 32 + 8 + 8;
/// The largest canonical V4 root: every full segment of both lists at their
/// ceilings, plus two tails just short of a segment.
pub const MAX_ENCODED_ROOT_V4_BYTES: usize = HEADER_LEN
    + (MAX_ROOT_BLOCKS as usize / SEGMENT_REFS + MAX_ROOT_PATCHES as usize / SEGMENT_REFS)
        * SEGMENT_REF_LEN
    + 2 * (SEGMENT_REFS - 1) * REF_LEN;

/// The most bytes one whole root read may consume: a V4 root together with
/// every segment it names, or a flat V3 root, whichever ceiling is larger.
///
/// Each full segment costs its own header and the root's reference to it on
/// top of the flat references it carries, so a V4 root at its count ceilings
/// reads about 1.3% more bytes than [`MAX_ENCODED_ROOT_BYTES`]. A read budget
/// clamped to the flat ceiling would refuse a lawful root.
pub const MAX_ROOT_READ_BYTES: usize = {
    let segmented = MAX_ENCODED_ROOT_V4_BYTES
        + (MAX_ROOT_BLOCKS as usize / SEGMENT_REFS + MAX_ROOT_PATCHES as usize / SEGMENT_REFS)
            * SEGMENT_BYTES;
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
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SegmentCache {
    coordinate: Option<(GraphId, BranchId, u64)>,
    blocks: Vec<SegmentRef>,
    patches: Vec<SegmentRef>,
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

/// A decoded root frame: a V3 root is complete in itself, and a V4 root still
/// needs its segments.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RootFrame {
    V3(PartitionRoot),
    V4(RootFrameV4),
}

/// An encoded V4 root and the segments it names that the cache did not
/// already hold. Those must be published, before the root, in the same batch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EncodedRoot {
    pub bytes: Vec<u8>,
    pub new_segments: Vec<(ObjectId, Vec<u8>)>,
}

/// One list of a V4 root as its logical digest sees it: the total count, the
/// full segments, and the inline tail.
struct ListDigest<'a> {
    count: usize,
    segments: &'a [SegmentRef],
    tail: &'a [SegmentEntry],
}

/// The V4 logical digest: UNKEYED, domain-separated, over the coordinate,
/// publication, and per list its total count, each full segment's logical
/// digest and span, and the tail references. Each segment digest binds that
/// segment's entries, so this binds the root's complete logical content.
fn root_logical_digest_v4(
    coordinate: (GraphId, BranchId, u64),
    published_at: CommitSeq,
    lists: [ListDigest<'_>; 2],
) -> [u8; 32] {
    let (graph, branch, partition) = coordinate;
    let mut hasher = fgdb_crypto::Hasher::new();
    hasher.update(ROOT_LOGICAL_DIGEST_DOMAIN_V4);
    hasher.update(&graph.0.to_be_bytes());
    hasher.update(&branch.0.to_be_bytes());
    hasher.update(&partition.to_be_bytes());
    hasher.update(&published_at.0.to_be_bytes());
    for list in lists {
        hasher.update(&(list.count as u32).to_be_bytes());
        for segment in list.segments {
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

/// Seal every full segment of `refs` the cache does not hold yet.
fn seal_full_segments<R: Copy + Into<SegmentEntry>>(
    refs: &[R],
    cache: &mut Vec<SegmentRef>,
    coordinate: (GraphId, BranchId, u64),
    class: SegmentClass,
    identity: (&[u8; 32], DatabaseSecurityNamespaceId),
    new_segments: &mut Vec<(ObjectId, Vec<u8>)>,
) -> Result<(), RootError> {
    let full = refs.len() / SEGMENT_REFS;
    if cache.len() > full {
        // The list shrank, so it was rewritten: nothing cached still chunks it.
        cache.clear();
    }
    for index in cache.len()..full {
        let entries: Vec<SegmentEntry> = refs[index * SEGMENT_REFS..(index + 1) * SEGMENT_REFS]
            .iter()
            .map(|reference| (*reference).into())
            .collect();
        let (graph, branch, partition) = coordinate;
        let (bytes, digest) = encode_segment(graph, branch, partition, class, &entries)
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
    Ok(())
}

/// Encode a root in format V4, sealing only the full segments `cache` does
/// not already hold. See [`SegmentCache`] for the caller's validity duty.
pub fn encode_root_v4(
    root: &PartitionRoot,
    k_oid: &[u8; 32],
    namespace: DatabaseSecurityNamespaceId,
    cache: &mut SegmentCache,
) -> Result<EncodedRoot, RootError> {
    validate_root(root)?;
    let coordinate = (root.graph, root.branch, root.partition);
    if cache.coordinate != Some(coordinate) {
        *cache = SegmentCache {
            coordinate: Some(coordinate),
            ..SegmentCache::default()
        };
    }
    let mut new_segments = Vec::new();
    seal_full_segments(
        &root.blocks,
        &mut cache.blocks,
        coordinate,
        SegmentClass::Blocks,
        (k_oid, namespace),
        &mut new_segments,
    )?;
    seal_full_segments(
        &root.vertex_patches,
        &mut cache.patches,
        coordinate,
        SegmentClass::VertexPatches,
        (k_oid, namespace),
        &mut new_segments,
    )?;
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
    let digest = root_logical_digest_v4(
        coordinate,
        root.published_at,
        [
            ListDigest {
                count: root.blocks.len(),
                segments: &cache.blocks,
                tail: &tails[0],
            },
            ListDigest {
                count: root.vertex_patches.len(),
                segments: &cache.patches,
                tail: &tails[1],
            },
        ],
    );
    let mut out = Vec::with_capacity(
        HEADER_LEN
            + (cache.blocks.len() + cache.patches.len()) * SEGMENT_REF_LEN
            + (tails[0].len() + tails[1].len()) * REF_LEN,
    );
    out.extend_from_slice(&ROOT_MAGIC);
    out.extend_from_slice(&ROOT_FORMAT_V4.to_be_bytes());
    out.extend_from_slice(&root.graph.0.to_be_bytes());
    out.extend_from_slice(&root.branch.0.to_be_bytes());
    out.extend_from_slice(&root.partition.to_be_bytes());
    out.extend_from_slice(&root.published_at.0.to_be_bytes());
    out.extend_from_slice(&(root.blocks.len() as u32).to_be_bytes());
    out.extend_from_slice(&(root.vertex_patches.len() as u32).to_be_bytes());
    out.extend_from_slice(&digest);
    for (segments, tail) in [&cache.blocks, &cache.patches].into_iter().zip(&tails) {
        for segment in segments {
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
    Ok(EncodedRoot {
        bytes: out,
        new_segments,
    })
}

/// Decode a root frame of either live format. A V3 root is fully decoded; a
/// V4 frame is checked against its own digest, and its segments are the
/// caller's to read and [`assemble_root`].
pub fn decode_root_frame(bytes: &[u8]) -> Result<RootFrame, RootError> {
    if bytes.len() < HEADER_LEN || bytes[..4] != ROOT_MAGIC {
        return Err(RootError::NotARoot);
    }
    let format = u16::from_be_bytes([bytes[4], bytes[5]]);
    if format == ROOT_FORMAT_V3 {
        return decode_root(bytes).map(RootFrame::V3);
    }
    if format != ROOT_FORMAT_V4 {
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
    let bytes32_at = |at: usize| -> [u8; 32] {
        let mut buf = [0u8; 32];
        buf.copy_from_slice(&bytes[at..at + 32]);
        buf
    };
    let count = u32_at(OFF_BLOCK_COUNT);
    if count > MAX_ROOT_BLOCKS {
        return Err(RootError::ImplausibleBlockCount { declared: count });
    }
    let patch_count = u32_at(OFF_PATCH_COUNT);
    if patch_count > MAX_ROOT_PATCHES {
        return Err(RootError::ImplausiblePatchCount {
            declared: patch_count,
        });
    }
    let (count, patch_count) = (count as usize, patch_count as usize);
    let lists = [
        (count / SEGMENT_REFS, count % SEGMENT_REFS),
        (patch_count / SEGMENT_REFS, patch_count % SEGMENT_REFS),
    ];
    let expected = HEADER_LEN
        + lists
            .iter()
            .map(|(segments, tail)| segments * SEGMENT_REF_LEN + tail * REF_LEN)
            .sum::<usize>();
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
    // Each list is its full segments' references, then its inline tail.
    let list_at = |at: usize, (segments, tail): (usize, usize)| {
        let named: Vec<SegmentRef> = (0..segments)
            .map(|index| {
                let at = at + index * SEGMENT_REF_LEN;
                SegmentRef {
                    segment_id: ObjectId(bytes32_at(at)),
                    digest: bytes32_at(at + 32),
                    first_seq: CommitSeq(u64_at(at + 64)),
                    last_seq: CommitSeq(u64_at(at + 72)),
                }
            })
            .collect();
        let tail_base = at + segments * SEGMENT_REF_LEN;
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
        (named, inline)
    };
    let [blocks_shape, patches_shape] = lists;
    let (block_segments, tail_blocks) = list_at(HEADER_LEN, blocks_shape);
    let (patch_segments, tail_patches) = list_at(
        HEADER_LEN + blocks_shape.0 * SEGMENT_REF_LEN + blocks_shape.1 * REF_LEN,
        patches_shape,
    );
    let coordinate = (
        GraphId(u128_at(OFF_GRAPH)),
        BranchId(u128_at(OFF_BRANCH)),
        u64_at(OFF_PARTITION),
    );
    let published_at = CommitSeq(u64_at(OFF_PUBLISHED));
    // The digest binds counts, segment digests and spans, and the tails: a
    // frame whose segments later resolve still cannot have been edited.
    let declared = bytes32_at(OFF_DIGEST);
    let recomputed = root_logical_digest_v4(
        coordinate,
        published_at,
        [
            ListDigest {
                count,
                segments: &block_segments,
                tail: &tail_blocks,
            },
            ListDigest {
                count: patch_count,
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
    Ok(RootFrame::V4(RootFrameV4 {
        graph,
        branch,
        partition,
        published_at,
        block_segments,
        tail_blocks: tail_blocks.into_iter().map(BlockRef::from).collect(),
        patch_segments,
        tail_patches: tail_patches.into_iter().map(PatchRef::from).collect(),
    }))
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

    // An empty block spans nothing, so it cannot honour any declared range —
    // and a root naming one is describing a block that carries no information.
    let Some(actual) = span_of(&entries) else {
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
    Ok(entries)
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
    blocks: &[Vec<crate::AdjacencyEntry>],
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
    blocks: &[Vec<crate::AdjacencyEntry>],
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
    blocks: &[Vec<crate::AdjacencyEntry>],
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
    blocks: &[Vec<crate::AdjacencyEntry>],
    block_props: &[Option<crate::edge_props::BlockProps>],
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
        if let Some(index) = block.iter().position(|entry| entry == winner) {
            let props = block_props
                .get(block_at)
                .and_then(Option::as_ref)
                .map(|props| props.props_of(index))
                .unwrap_or_default();
            return Ok(Some((*winner, props)));
        }
    }
    // Unreachable for a history the collapse admitted, but never a panic on
    // a read path: answer the entry with no properties.
    Ok(Some((*winner, Vec::new())))
}

/// The row each EId's LAST statement carries, by one forward pass in
/// publication order — the same last-block-wins rule the entry collapse
/// applies, over the hosted columns instead. Shared by the whole-graph scan
/// and compaction so neither can drift to a second precedence rule.
pub(crate) fn winning_edge_rows(
    blocks: &[Vec<crate::AdjacencyEntry>],
    block_props: &[Option<crate::edge_props::BlockProps>],
) -> std::collections::BTreeMap<(EId, CommitSeq), crate::edge_props::EdgePropertyRow> {
    let mut rows = std::collections::BTreeMap::new();
    for (block, props) in blocks.iter().zip(block_props) {
        for (index, entry) in block.iter().enumerate() {
            let row = props
                .as_ref()
                .map(|props| props.props_of(index))
                .unwrap_or_default();
            rows.insert((entry.eid, entry.created_at), row);
        }
    }
    rows
}

/// Every edge with a visible version at `as_of`, each beside the row its
/// winning statement carries, in ascending EId order (fgdb-9k5w) — the
/// whole-graph scan a query layer starts from, under the identical
/// whole-history validation and precedence rules as every point lookup.
#[allow(clippy::type_complexity)]
pub fn merge_all_edges_with_props(
    blocks: &[Vec<crate::AdjacencyEntry>],
    block_props: &[Option<crate::edge_props::BlockProps>],
    as_of: CommitSeq,
) -> Result<Vec<(crate::AdjacencyEntry, crate::edge_props::EdgePropertyRow)>, RootError> {
    if blocks.len() != block_props.len() {
        return Err(RootError::BlockPropsArity {
            blocks: blocks.len(),
            props: block_props.len(),
        });
    }
    let (statements, _) = collapse_edge_history(blocks)?;
    let mut rows = winning_edge_rows(blocks, block_props);
    Ok(statements
        .into_iter()
        .filter(|(_, entry)| entry.visible_at(as_of))
        .map(|(key, entry)| (entry, rows.remove(&key).unwrap_or_default()))
        .collect())
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
    /// One row per content STATEMENT, keyed `(eid, created_at)` — the FGSV V2
    /// chain model applied to edges (fgdb-ls5b). Birth fields are immutable
    /// across an EId's whole chain; `created_at` advances only at contiguity.
    statements: std::collections::BTreeMap<(EId, CommitSeq), crate::AdjacencyEntry>,
    seen: usize,
}

impl EdgeHistoryValidator {
    /// Admit one block at its publication position.
    pub(crate) fn observe_block(
        &mut self,
        block_at: usize,
        block: &[crate::AdjacencyEntry],
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
            let key = (entry.eid, entry.created_at);
            if let Some(existing) = self.statements.get(&key) {
                // A restatement of one exact statement: birth fields must
                // byte-match, and the only lawful change is live-to-retired.
                if birth(existing) != birth(entry) {
                    return Err(RootError::EdgeIdentityMismatch {
                        eid: entry.eid,
                        conflict: Box::new(EdgeIdentityConflict {
                            expected: birth(existing),
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
            } else {
                // A NEW statement must extend its EId's chain contiguously —
                // begin exactly where the predecessor retired (a gap is a
                // resurrection, an overlap is aliasing) and carry the SAME
                // topology, since (src, relation, dst) is immutable for the
                // chain's whole life; only content versions advance
                // (fgdb-ls5b, the FGSV V2 law on edges).
                let predecessor = self
                    .statements
                    .range(..key)
                    .next_back()
                    .filter(|((eid, _), _)| *eid == entry.eid)
                    .map(|(_, existing)| existing);
                if let Some(predecessor) = predecessor
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
                let successor = self
                    .statements
                    .range((std::ops::Bound::Excluded(key), std::ops::Bound::Unbounded))
                    .next()
                    .filter(|((eid, _), _)| *eid == entry.eid)
                    .map(|(_, existing)| existing);
                if let Some(successor) = successor
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
            }
            self.statements.insert(key, *entry);
        }
        Ok(())
    }

    fn into_canonical(self) -> CollapsedEdgeHistory {
        let superseded = self.seen - self.statements.len();
        (self.statements, superseded)
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
    blocks: &[Vec<crate::AdjacencyEntry>],
) -> Result<CollapsedEdgeHistory, RootError> {
    let mut validator = EdgeHistoryValidator::default();
    for (block_at, block) in blocks.iter().enumerate() {
        validator.observe_block(block_at, block)?;
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
    /// Admit one patch at its publication position.
    pub(crate) fn observe_patch(
        &mut self,
        patch_at: usize,
        rows: &[crate::vertex::VertexRow],
    ) -> Result<(), RootError> {
        for row in rows {
            let key = (row.vid, row.created_at);
            if let Some(existing) = self.rows.get(&key) {
                // A restatement of one exact version: birth must byte-match,
                // and the only lawful change is live-to-retired.
                let mut expected_birth = existing.clone();
                let mut found_birth = row.clone();
                expected_birth.retired_at = None;
                found_birth.retired_at = None;
                if found_birth != expected_birth {
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
