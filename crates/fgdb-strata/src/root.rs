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
use fgdb_types::ids::{DatabaseSecurityNamespaceId, ObjectId};
use fgdb_types::{BranchId, CommitSeq, EId, GraphId, VId};

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
/// Format version, versioned from day one (§16.6).
pub const ROOT_FORMAT_V2: u16 = 2;

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
/// magic + format + graph + branch + partition + published_at + block_count
/// + vertex_patch_count
const HEADER_LEN: usize = OFF_PATCH_COUNT + 4;
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

/// Encode a root canonically, refusing anything that is not.
pub fn encode_root(root: &PartitionRoot) -> Result<Vec<u8>, RootError> {
    validate_root(root)?;

    let mut out =
        Vec::with_capacity(HEADER_LEN + (root.blocks.len() + root.vertex_patches.len()) * REF_LEN);
    out.extend_from_slice(&ROOT_MAGIC);
    out.extend_from_slice(&ROOT_FORMAT_V2.to_be_bytes());
    out.extend_from_slice(&root.graph.0.to_be_bytes());
    out.extend_from_slice(&root.branch.0.to_be_bytes());
    out.extend_from_slice(&root.partition.to_be_bytes());
    out.extend_from_slice(&root.published_at.0.to_be_bytes());
    out.extend_from_slice(&(root.blocks.len() as u32).to_be_bytes());
    out.extend_from_slice(&(root.vertex_patches.len() as u32).to_be_bytes());
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
    if format != ROOT_FORMAT_V2 {
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
    Ok(root)
}

/// The content identity of an encoded root — same derivation as a block's.
pub fn root_id(k_oid: &[u8; 32], namespace: DatabaseSecurityNamespaceId, bytes: &[u8]) -> ObjectId {
    ObjectId(fgdb_crypto::logical_object_id(k_oid, &namespace.0, &[], bytes).0)
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
    let (entries, _) = collapse_edge_history(blocks)?;
    Ok(entries
        .get(&eid)
        .filter(|entry| entry.visible_at(as_of))
        .copied())
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
    let (entries, _) = collapse_edge_history(blocks)?;
    let Some(winner) = entries.get(&eid).filter(|entry| entry.visible_at(as_of)) else {
        return Ok(None);
    };
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
) -> std::collections::BTreeMap<EId, crate::edge_props::EdgePropertyRow> {
    let mut rows = std::collections::BTreeMap::new();
    for (block, props) in blocks.iter().zip(block_props) {
        for (index, entry) in block.iter().enumerate() {
            let row = props
                .as_ref()
                .map(|props| props.props_of(index))
                .unwrap_or_default();
            rows.insert(entry.eid, row);
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
    let (entries, _) = collapse_edge_history(blocks)?;
    let mut rows = winning_edge_rows(blocks, block_props);
    Ok(entries
        .into_iter()
        .filter(|(_, entry)| entry.visible_at(as_of))
        .map(|(eid, entry)| (entry, rows.remove(&eid).unwrap_or_default()))
        .collect())
}

/// Incremental proof that every block in one publication history agrees on EId
/// identity and lifecycle.
///
/// Root admission uses this without retaining decoded future blocks; merge and
/// compaction consume the same state into their canonical one-entry-per-EId map.
#[derive(Debug, Default)]
pub(crate) struct EdgeHistoryValidator {
    entries: std::collections::BTreeMap<EId, crate::AdjacencyEntry>,
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
            let found_key = (entry.src, entry.relation, entry.dst, entry.eid);
            if previous_key.is_some_and(|previous| previous >= found_key) {
                return Err(RootError::Block {
                    at: block_at,
                    error: BlockError::NonCanonicalOrder { at: entry_at },
                });
            }
            previous_key = Some(found_key);
            self.seen += 1;
            if let Some(existing) = self.entries.get(&entry.eid) {
                let expected = EdgeBirth {
                    src: existing.src,
                    relation: existing.relation,
                    dst: existing.dst,
                    created_at: existing.created_at,
                };
                let found = EdgeBirth {
                    src: entry.src,
                    relation: entry.relation,
                    dst: entry.dst,
                    created_at: entry.created_at,
                };
                if found != expected {
                    return Err(RootError::EdgeIdentityMismatch {
                        eid: entry.eid,
                        conflict: Box::new(EdgeIdentityConflict { expected, found }),
                    });
                }
                if existing.retired_at.is_some() && entry.retired_at != existing.retired_at {
                    return Err(RootError::EdgeRetirementMismatch {
                        eid: entry.eid,
                        expected: existing.retired_at,
                        found: entry.retired_at,
                    });
                }
            }
            self.entries.insert(entry.eid, *entry);
        }
        Ok(())
    }

    fn into_canonical(
        self,
    ) -> (
        std::collections::BTreeMap<EId, crate::AdjacencyEntry>,
        usize,
    ) {
        let superseded = self.seen - self.entries.len();
        (self.entries, superseded)
    }
}

/// Validate and collapse a block publication history to one statement per EId.
///
/// Later blocks may restate one exact birth to add its retirement, and the later
/// statement wins. Nothing may change the birth itself: EIds are permanently
/// spent, so treating `created_at` as a version discriminator would silently make
/// reuse legal in the durable read and compaction paths.
pub(crate) fn collapse_edge_history(
    blocks: &[Vec<crate::AdjacencyEntry>],
) -> Result<
    (
        std::collections::BTreeMap<EId, crate::AdjacencyEntry>,
        usize,
    ),
    RootError,
> {
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
) -> Result<Vec<crate::vertex::VertexRow>, RootError> {
    let rows = crate::vertex::read_patch(
        k_oid,
        namespace,
        bytes,
        crate::vertex::VertexPatchVersion(reference.patch_id),
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
