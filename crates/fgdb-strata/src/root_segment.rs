//! Root segments (fgdb-d5vo4): the bounded, content-addressed chunks a V4
//! partition root names instead of listing every reference itself.
//!
//! A partition's reference lists are append-only between compactions, so
//! chunking them at fixed [`SEGMENT_REFS`] boundaries is a pure function of the
//! list: segment `i` holds references `256 * i .. 256 * (i + 1)`. Only FULL
//! segments exist; the remainder stays inline in the root. Live publication,
//! rebuild and reopen therefore produce identical segments, and a commit writes
//! only the segment it fills, never the ones before it. That is what makes a
//! commit's root work O(N / 256 + 256) instead of O(N) (owner ruling (b) on
//! fgdb-root-format-ruling-sitting-snbc, re-ratified on fgdb-d5vo4).
//!
//! A segment is exactly one class of reference (blocks or vertex patches) of
//! one partition coordinate. It commits to its own logical content with an
//! UNKEYED digest, which the root binds in turn, and it is content-addressed
//! under its own registered kind, so a root names each segment by identity.

use crate::root::{BlockRef, PatchRef};
use fgdb_types::ids::{DatabaseSecurityNamespaceId, ObjectId};
use fgdb_types::{BranchId, CommitSeq, GraphId};

/// `FGSG` — FrankenGraph Strata root seGment.
pub const SEGMENT_MAGIC: [u8; 4] = *b"FGSG";
/// Format version, versioned from day one (§16.6).
pub const SEGMENT_FORMAT_V1: u16 = 1;
/// References per segment. Mirrors `MAX_BLOCK_ENTRIES`, so every interior
/// Tier-D object has one bounded fanout (ruling (b): K = 256).
pub const SEGMENT_REFS: usize = 256;
/// Durable object kind `DeltaRootSegment`.
pub const SEGMENT_OBJECT_KIND: u16 = 0x058b;
/// Domain separator for [`segment_logical_digest`].
pub const SEGMENT_LOGICAL_DIGEST_DOMAIN: &[u8] = b"fgdb.strata.root-segment-logical-digest.v1";

const OFF_GRAPH: usize = 6;
const OFF_BRANCH: usize = OFF_GRAPH + 16;
const OFF_PARTITION: usize = OFF_BRANCH + 16;
const OFF_CLASS: usize = OFF_PARTITION + 8;
const OFF_DIGEST: usize = OFF_CLASS + 1;
/// magic + format + graph + branch + partition + class + digest.
const HEADER_LEN: usize = OFF_DIGEST + 32;
/// id(32) + first_seq(8) + last_seq(8), as in the root's own ref lists.
const REF_LEN: usize = 32 + 8 + 8;
/// The exact length of every segment: segments are always full.
pub const SEGMENT_BYTES: usize = HEADER_LEN + SEGMENT_REFS * REF_LEN;

/// Which of the root's two reference lists a segment chunks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum SegmentClass {
    Blocks,
    VertexPatches,
}

impl SegmentClass {
    const fn tag(self) -> u8 {
        match self {
            Self::Blocks => 0,
            Self::VertexPatches => 1,
        }
    }

    const fn from_tag(tag: u8) -> Option<Self> {
        match tag {
            0 => Some(Self::Blocks),
            1 => Some(Self::VertexPatches),
            _ => None,
        }
    }
}

/// One reference inside a segment: an object and the sequence span it covers.
/// A [`BlockRef`] and a [`PatchRef`] have this shape; the segment's class says
/// which it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SegmentEntry {
    pub id: ObjectId,
    pub first_seq: CommitSeq,
    pub last_seq: CommitSeq,
}

impl From<BlockRef> for SegmentEntry {
    fn from(block: BlockRef) -> Self {
        Self {
            id: block.block_id,
            first_seq: block.first_seq,
            last_seq: block.last_seq,
        }
    }
}

impl From<PatchRef> for SegmentEntry {
    fn from(patch: PatchRef) -> Self {
        Self {
            id: patch.patch_id,
            first_seq: patch.first_seq,
            last_seq: patch.last_seq,
        }
    }
}

impl From<SegmentEntry> for BlockRef {
    fn from(entry: SegmentEntry) -> Self {
        Self {
            block_id: entry.id,
            first_seq: entry.first_seq,
            last_seq: entry.last_seq,
        }
    }
}

impl From<SegmentEntry> for PatchRef {
    fn from(entry: SegmentEntry) -> Self {
        Self {
            patch_id: entry.id,
            first_seq: entry.first_seq,
            last_seq: entry.last_seq,
        }
    }
}

/// How a V4 root names one full segment: its content identity, the unkeyed
/// logical digest the root binds, and the span its entries cover
/// (`first_seq` the least first, `last_seq` the last entry's, which is the
/// greatest because a list's frontiers never regress).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SegmentRef {
    pub segment_id: ObjectId,
    pub digest: [u8; 32],
    pub first_seq: CommitSeq,
    pub last_seq: CommitSeq,
}

/// A decoded segment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RootSegment {
    pub graph: GraphId,
    pub branch: BranchId,
    pub partition: u64,
    pub class: SegmentClass,
    pub entries: Vec<SegmentEntry>,
}

/// Why a segment could not be encoded, decoded, or bound to its root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SegmentError {
    NotASegment,
    UnsupportedFormat {
        format: u16,
    },
    Length {
        expected: usize,
        found: usize,
    },
    UnknownClass {
        class: u8,
    },
    NotFull {
        entries: usize,
    },
    DigestMismatch {
        declared: [u8; 32],
        recomputed: [u8; 32],
    },
    IdentityMismatch {
        expected: ObjectId,
        actual: ObjectId,
    },
    /// The segment's coordinate, class, digest or span is not the one the
    /// root names for it.
    Binding {
        at: usize,
        what: &'static str,
    },
}

impl core::fmt::Display for SegmentError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NotASegment => write!(f, "not a strata root segment"),
            Self::UnsupportedFormat { format } => {
                write!(f, "root segment format {format} is not implemented")
            }
            Self::Length { expected, found } => {
                write!(f, "root segment must be {expected} bytes, found {found}")
            }
            Self::UnknownClass { class } => write!(f, "unknown root segment class {class}"),
            Self::NotFull { entries } => write!(
                f,
                "a root segment holds exactly {SEGMENT_REFS} references, not {entries}"
            ),
            Self::DigestMismatch { .. } => {
                write!(f, "root segment digest does not match its content")
            }
            Self::IdentityMismatch { expected, actual } => write!(
                f,
                "root segment identity {actual:?} is not the named {expected:?}"
            ),
            Self::Binding { at, what } => {
                write!(f, "root segment {at} does not match its root: {what}")
            }
        }
    }
}

impl core::error::Error for SegmentError {}

/// The segment's canonical logical digest: UNKEYED, domain-separated, over
/// its coordinate, class and ordered entries.
pub fn segment_logical_digest(
    graph: GraphId,
    branch: BranchId,
    partition: u64,
    class: SegmentClass,
    entries: &[SegmentEntry],
) -> [u8; 32] {
    let mut hasher = fgdb_crypto::Hasher::new();
    hasher.update(SEGMENT_LOGICAL_DIGEST_DOMAIN);
    hasher.update(&graph.0.to_be_bytes());
    hasher.update(&branch.0.to_be_bytes());
    hasher.update(&partition.to_be_bytes());
    hasher.update(&[class.tag()]);
    hasher.update(&(entries.len() as u32).to_be_bytes());
    for entry in entries {
        hasher.update(&entry.id.0);
        hasher.update(&entry.first_seq.0.to_be_bytes());
        hasher.update(&entry.last_seq.0.to_be_bytes());
    }
    hasher.finalize().0
}

/// Encode one full segment, returning its bytes and logical digest.
pub fn encode_segment(
    graph: GraphId,
    branch: BranchId,
    partition: u64,
    class: SegmentClass,
    entries: &[SegmentEntry],
) -> Result<(Vec<u8>, [u8; 32]), SegmentError> {
    if entries.len() != SEGMENT_REFS {
        return Err(SegmentError::NotFull {
            entries: entries.len(),
        });
    }
    let digest = segment_logical_digest(graph, branch, partition, class, entries);
    let mut out = Vec::with_capacity(SEGMENT_BYTES);
    out.extend_from_slice(&SEGMENT_MAGIC);
    out.extend_from_slice(&SEGMENT_FORMAT_V1.to_be_bytes());
    out.extend_from_slice(&graph.0.to_be_bytes());
    out.extend_from_slice(&branch.0.to_be_bytes());
    out.extend_from_slice(&partition.to_be_bytes());
    out.push(class.tag());
    out.extend_from_slice(&digest);
    for entry in entries {
        out.extend_from_slice(&entry.id.0);
        out.extend_from_slice(&entry.first_seq.0.to_be_bytes());
        out.extend_from_slice(&entry.last_seq.0.to_be_bytes());
    }
    Ok((out, digest))
}

/// Decode a segment, re-checking its length, class and digest. Returns the
/// segment and its verified logical digest.
pub fn decode_segment(bytes: &[u8]) -> Result<(RootSegment, [u8; 32]), SegmentError> {
    if bytes.len() < HEADER_LEN || bytes[..4] != SEGMENT_MAGIC {
        return Err(SegmentError::NotASegment);
    }
    let format = u16::from_be_bytes([bytes[4], bytes[5]]);
    if format != SEGMENT_FORMAT_V1 {
        return Err(SegmentError::UnsupportedFormat { format });
    }
    if bytes.len() != SEGMENT_BYTES {
        return Err(SegmentError::Length {
            expected: SEGMENT_BYTES,
            found: bytes.len(),
        });
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
    let bytes32_at = |at: usize| -> [u8; 32] {
        let mut buf = [0u8; 32];
        buf.copy_from_slice(&bytes[at..at + 32]);
        buf
    };
    let class = SegmentClass::from_tag(bytes[OFF_CLASS]).ok_or(SegmentError::UnknownClass {
        class: bytes[OFF_CLASS],
    })?;
    let entries: Vec<SegmentEntry> = (0..SEGMENT_REFS)
        .map(|index| {
            let at = HEADER_LEN + index * REF_LEN;
            SegmentEntry {
                id: ObjectId(bytes32_at(at)),
                first_seq: CommitSeq(u64_at(at + 32)),
                last_seq: CommitSeq(u64_at(at + 40)),
            }
        })
        .collect();
    let segment = RootSegment {
        graph: GraphId(u128_at(OFF_GRAPH)),
        branch: BranchId(u128_at(OFF_BRANCH)),
        partition: u64_at(OFF_PARTITION),
        class,
        entries,
    };
    let declared = bytes32_at(OFF_DIGEST);
    let recomputed = segment_logical_digest(
        segment.graph,
        segment.branch,
        segment.partition,
        segment.class,
        &segment.entries,
    );
    if declared != recomputed {
        return Err(SegmentError::DigestMismatch {
            declared,
            recomputed,
        });
    }
    Ok((segment, recomputed))
}

/// The content identity of an encoded segment, under its registered kind.
pub fn segment_id(
    k_oid: &[u8; 32],
    namespace: DatabaseSecurityNamespaceId,
    bytes: &[u8],
) -> ObjectId {
    ObjectId(
        fgdb_crypto::logical_object_id(
            k_oid,
            &namespace.0,
            &SEGMENT_OBJECT_KIND.to_le_bytes(),
            bytes,
        )
        .0,
    )
}

/// The span a full segment's entries cover, as its root names it.
pub fn segment_span(entries: &[SegmentEntry]) -> (CommitSeq, CommitSeq) {
    let first = entries
        .iter()
        .map(|entry| entry.first_seq)
        .min()
        .unwrap_or(CommitSeq(0));
    let last = entries.last().map_or(CommitSeq(0), |entry| entry.last_seq);
    (first, last)
}

/// Decode the segment a root names as its `at`-th segment of `class`, proving
/// it is that segment: its identity, coordinate, class, digest and span must
/// all be the ones the root binds.
pub fn read_segment(
    k_oid: &[u8; 32],
    namespace: DatabaseSecurityNamespaceId,
    bytes: &[u8],
    named: &SegmentRef,
    at: usize,
    coordinate: (GraphId, BranchId, u64),
    class: SegmentClass,
) -> Result<Vec<SegmentEntry>, SegmentError> {
    let actual = segment_id(k_oid, namespace, bytes);
    if actual != named.segment_id {
        return Err(SegmentError::IdentityMismatch {
            expected: named.segment_id,
            actual,
        });
    }
    let (segment, digest) = decode_segment(bytes)?;
    if (segment.graph, segment.branch, segment.partition) != coordinate {
        return Err(SegmentError::Binding {
            at,
            what: "partition coordinate",
        });
    }
    if segment.class != class {
        return Err(SegmentError::Binding { at, what: "class" });
    }
    if digest != named.digest {
        return Err(SegmentError::Binding { at, what: "digest" });
    }
    if segment_span(&segment.entries) != (named.first_seq, named.last_seq) {
        return Err(SegmentError::Binding { at, what: "span" });
    }
    Ok(segment.entries)
}
