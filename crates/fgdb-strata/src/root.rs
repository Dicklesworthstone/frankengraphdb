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
//! **RANGES ARE ASCENDING AND NON-OVERLAPPING, and that is a semantic rule rather
//! than tidiness.** Two blocks claiming the same commit sequence would make a
//! merge ambiguous: a reader assembling a partition's state at that sequence would
//! have two sources for it and no rule to choose between them. Refusing the
//! overlap at publication is how that ambiguity is made unrepresentable instead of
//! resolved by accident at read time. GAPS are allowed — a partition that received
//! no commits over a stretch of the stream simply has none.
//!
//! **WHAT IS DELIBERATELY ABSENT**: merging reads ACROSS blocks. A root says what
//! the blocks are; assembling one answer out of several is the MVCC-chain slice,
//! and it has a design question this slice deliberately does not prejudge — see
//! the note on [`BlockRef`].

use crate::BlockError;
use fgdb_types::ids::{DatabaseSecurityNamespaceId, ObjectId};
use fgdb_types::{BranchId, CommitSeq, GraphId};

/// `FGSR` — FrankenGraph Strata Root.
pub const ROOT_MAGIC: [u8; 4] = *b"FGSR";
/// Format version, versioned from day one (§16.6).
pub const ROOT_FORMAT_V1: u16 = 1;

// Header field offsets, written out rather than computed at each use site. The
// first draft of the decoder read `published_at` at 38 (the partition field) and
// the block count at 46 — the same arithmetic slip the block decoder made, and the
// reason both layouts now name their offsets instead of adding widths inline.
const OFF_GRAPH: usize = 6;
const OFF_BRANCH: usize = OFF_GRAPH + 16;
const OFF_PARTITION: usize = OFF_BRANCH + 16;
const OFF_PUBLISHED: usize = OFF_PARTITION + 8;
const OFF_BLOCK_COUNT: usize = OFF_PUBLISHED + 8;
/// magic + format + graph + branch + partition + published_at + block_count
const HEADER_LEN: usize = OFF_BLOCK_COUNT + 4;
/// block_id(32) + first_seq(8) + last_seq(8)
const REF_LEN: usize = 32 + 8 + 8;

/// The largest number of blocks this build will read from one root.
pub const MAX_ROOT_BLOCKS: u32 = 1 << 20;

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
/// **THE CROSS-BLOCK RETIREMENT QUESTION IS OPEN AND NOT PREJUDGED HERE.**
/// `AdjacencyEntry` carries `created_at` and `retired_at` in one entry, which is
/// complete within a block and cannot express "an entry created in an earlier
/// block was retired later" — an immutable block cannot be edited. The MVCC-chain
/// slice must choose between tombstone entries that shadow an earlier creation and
/// blocks that carry whole version chains per key. This slice stores neither, so
/// it cannot make that choice by accident.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct BlockRef {
    pub block_id: ObjectId,
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
    /// Two blocks claim the same sequence, or the list is not ascending.
    ///
    /// Carries both positions, because "this root overlaps" is not actionable and
    /// "blocks 3 and 4 both claim sequence 12" is.
    OverlappingRanges {
        earlier: usize,
        later: usize,
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
    /// Two versions of one key are live at the same sequence.
    ///
    /// A merge cannot answer this: the history claims a key was in two states at
    /// once, so it is not a sequence of states at all. Refused rather than
    /// deduplicated — collapsing it would return a plausible answer built on an
    /// impossible one, which is the shape of wrong that is hardest to notice.
    OverlappingVersions {
        dst: fgdb_types::VId,
        as_of: CommitSeq,
    },
    /// Reading one of the named blocks failed.
    Block {
        at: usize,
        error: BlockError,
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
            Self::OverlappingRanges { earlier, later } => {
                write!(
                    f,
                    "blocks {earlier} and {later} claim overlapping sequences"
                )
            }
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
            Self::OverlappingVersions { dst, as_of } => write!(
                f,
                "two versions of {dst:?} are live at {as_of:?}; the history is not a \
                 sequence of states"
            ),
            Self::Block { at, error } => write!(f, "block {at}: {error}"),
        }
    }
}

impl core::error::Error for RootError {}

fn validate(root: &PartitionRoot) -> Result<(), RootError> {
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
            // Ascending AND disjoint in one comparison: the next block must start
            // strictly after the previous one ended. Gaps are fine; overlap is not.
            if block.first_seq.0 <= previous.last_seq.0 {
                return Err(RootError::OverlappingRanges {
                    earlier: index - 1,
                    later: index,
                });
            }
        }
    }
    Ok(())
}

/// Encode a root canonically, refusing anything that is not.
pub fn encode_root(root: &PartitionRoot) -> Result<Vec<u8>, RootError> {
    if root.blocks.len() as u64 > u64::from(MAX_ROOT_BLOCKS) {
        return Err(RootError::ImplausibleBlockCount {
            declared: MAX_ROOT_BLOCKS,
        });
    }
    validate(root)?;

    let mut out = Vec::with_capacity(HEADER_LEN + root.blocks.len() * REF_LEN);
    out.extend_from_slice(&ROOT_MAGIC);
    out.extend_from_slice(&ROOT_FORMAT_V1.to_be_bytes());
    out.extend_from_slice(&root.graph.0.to_be_bytes());
    out.extend_from_slice(&root.branch.0.to_be_bytes());
    out.extend_from_slice(&root.partition.to_be_bytes());
    out.extend_from_slice(&root.published_at.0.to_be_bytes());
    out.extend_from_slice(&(root.blocks.len() as u32).to_be_bytes());
    for block in &root.blocks {
        out.extend_from_slice(&block.block_id.0);
        out.extend_from_slice(&block.first_seq.0.to_be_bytes());
        out.extend_from_slice(&block.last_seq.0.to_be_bytes());
    }
    Ok(out)
}

/// Decode a root, re-checking every law the encoder enforces.
pub fn decode_root(bytes: &[u8]) -> Result<PartitionRoot, RootError> {
    if bytes.len() < HEADER_LEN || bytes[..4] != ROOT_MAGIC {
        return Err(RootError::NotARoot);
    }
    let format = u16::from_be_bytes([bytes[4], bytes[5]]);
    if format != ROOT_FORMAT_V1 {
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
    let count = u32::from_be_bytes([
        bytes[OFF_BLOCK_COUNT],
        bytes[OFF_BLOCK_COUNT + 1],
        bytes[OFF_BLOCK_COUNT + 2],
        bytes[OFF_BLOCK_COUNT + 3],
    ]);
    if count > MAX_ROOT_BLOCKS {
        return Err(RootError::ImplausibleBlockCount { declared: count });
    }
    let expected = HEADER_LEN + count as usize * REF_LEN;
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
    let root = PartitionRoot {
        graph: GraphId(u128_at(OFF_GRAPH)),
        branch: BranchId(u128_at(OFF_BRANCH)),
        partition: u64_at(OFF_PARTITION),
        published_at: CommitSeq(u64_at(OFF_PUBLISHED)),
        blocks,
    };
    validate(&root)?;
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
    let mut out = Vec::with_capacity(root.blocks.len());
    for (index, reference) in root.blocks.iter().enumerate() {
        let bytes = load(reference.block_id).ok_or(RootError::Block {
            at: index,
            error: BlockError::NotABlock,
        })?;
        let entries = crate::read_block(k_oid, namespace, &bytes, reference.block_id)
            .map_err(|error| RootError::Block { at: index, error })?;

        // An empty block spans nothing, so it cannot honour any declared range —
        // and a root naming one is describing a block that carries no information.
        let Some(actual) = span_of(&entries) else {
            return Err(RootError::BlockRangeMismatch {
                at: index,
                declared: (reference.first_seq, reference.last_seq),
                actual: (CommitSeq(0), CommitSeq(0)),
            });
        };
        if actual != (reference.first_seq, reference.last_seq) {
            return Err(RootError::BlockRangeMismatch {
                at: index,
                declared: (reference.first_seq, reference.last_seq),
                actual,
            });
        }
        out.push(entries);
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
/// `(src, relation, dst)` key whose interval states the retirement, and it
/// SUPERSEDES the earlier one. The alternative — every block carrying whole
/// version chains for the keys it touches — was rejected because it makes a write
/// read-modify-write: the writer would have to fetch each key's prior versions
/// before it could seal a block, which is exactly the ingest cost B2's LSM shape
/// exists to avoid. Tombstone supersede keeps writes append-only and moves the
/// work to the read, which is what an LSM trades.
///
/// **SUPERSEDE IS PER VERSION, NOT PER KEY, and getting that wrong loses history.**
/// The first implementation keyed the merge on `dst` alone and let the last block
/// win outright. It passes every retirement law and is WRONG: once a key is
/// retired and re-created, the newer version replaces the older one entirely, so a
/// read AS OF a sequence when the older version was live returns nothing. MVCC
/// time-travel is the whole of B1, and a storage tier that cannot answer an old
/// snapshot has silently dropped it. The merge is therefore keyed on
/// `(dst, created_at)` — a VERSION — and selection among versions is by interval
/// containment. Only entries describing the same version supersede, which is
/// exactly the cross-block retirement case.
///
/// Among entries for one version, the LATER BLOCK wins, because the root already
/// establishes a total order over blocks with disjoint ranges. Using the entry's
/// own interval to decide would be a second ordering rule that could disagree with
/// the first, and two rules for one question is how they drift.
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
    // Keyed by (dst, created_at) — the VERSION, not the key. Two entries for one
    // dst with different creations are two different versions and must BOTH
    // survive the merge; only entries describing the same version supersede.
    let mut versions: std::collections::BTreeMap<(fgdb_types::VId, u64), crate::AdjacencyEntry> =
        std::collections::BTreeMap::new();
    for block in blocks {
        for entry in block {
            if entry.src != src || entry.relation != relation {
                continue;
            }
            versions.insert((entry.dst, entry.created_at.0), *entry);
        }
    }

    let mut out: Vec<fgdb_types::VId> = Vec::new();
    for entry in versions.values().filter(|e| e.visible_at(as_of)) {
        // TWO LIVE VERSIONS OF ONE KEY AT ONE SEQUENCE IS A CORRUPT MERGE, not a
        // duplicate to quietly collapse. It means the stream retired a version
        // and created its successor with overlapping intervals, so the history is
        // not a sequence of states — and a reader that deduplicated would return a
        // plausible answer built on an impossible one.
        if out.last() == Some(&entry.dst) {
            return Err(RootError::OverlappingVersions {
                dst: entry.dst,
                as_of,
            });
        }
        out.push(entry.dst);
    }
    Ok(out)
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
