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
//!
//! **INDEX SEGMENTS (fgdb-5gzaa, format V5 roots).** A V4 root still names
//! every full segment, so its bytes grow by one 80-byte reference per 256
//! references published. An index segment names [`INDEX_REFS`] consecutive
//! full segments of one list, chunked the same way: index `j` holds segments
//! `16 * j .. 16 * (j + 1)`, only full ones exist, and the root names the
//! index segments, then the segments after the last full index, then the tail.
//!
//! An index segment is its own registered kind rather than a level of
//! [`SEGMENT_OBJECT_KIND`]: a kind whose references target itself is a DAG
//! self-edge the durable-field registry refuses. One index level is all the
//! ceilings need: 2^20 references make 4,096 segments and so at most 256 index
//! segments, which the root names directly.

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

/// `FGSI` — FrankenGraph Strata root Index segment (fgdb-5gzaa).
pub const INDEX_MAGIC: [u8; 4] = *b"FGSI";
/// Index segment format version, versioned from day one (§16.6).
pub const INDEX_FORMAT_V1: u16 = 1;
/// Segment references per index segment (owner ruling 2026-10-07 on
/// fgdb-5gzaa). A root names the segments after its last full index segment,
/// so this fanout is how many a commit can re-name. At 256 a root re-named up
/// to 255 segments (20 KiB) per list and grew exactly as V4 below 65,536
/// references; at 16 it re-names at most 15, and stored bytes per commit stay
/// flat (measured Theil-Sen slope 0.026, against 0.098 at 256 and 0.362 for
/// V4). The worst root at the 2^20 ceilings is 67,934 bytes either way.
pub const INDEX_REFS: usize = 16;
/// Durable object kind `DeltaRootIndexSegment`.
pub const INDEX_OBJECT_KIND: u16 = 0x058c;
/// Domain separator for [`index_logical_digest`].
pub const INDEX_LOGICAL_DIGEST_DOMAIN: &[u8] = b"fgdb.strata.root-index-segment-logical-digest.v1";
/// How a root or an index segment names one segment: segment_id(32) + its
/// logical digest(32) + first_seq(8) + last_seq(8).
pub const SEGMENT_REF_LEN: usize = 32 + 32 + 8 + 8;
/// The exact length of every index segment: index segments are always full.
pub const INDEX_BYTES: usize = HEADER_LEN + INDEX_REFS * SEGMENT_REF_LEN;

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

/// How a root or an index segment names one full segment (or a root names one
/// index segment): its content identity, the unkeyed logical digest its
/// namer binds, and the span its entries cover (`first_seq` the least first,
/// `last_seq` the last entry's, which is the greatest because a list's
/// frontiers never regress).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SegmentRef {
    pub segment_id: ObjectId,
    pub digest: [u8; 32],
    pub first_seq: CommitSeq,
    pub last_seq: CommitSeq,
}

/// Which of the two segment kinds an identity names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SegmentLevel {
    /// A [`RootSegment`]: 256 block or vertex-patch references.
    Leaf,
    /// A [`RootIndexSegment`]: 16 leaf segment references.
    Index,
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

/// A decoded index segment: [`INDEX_REFS`] consecutive full segments of one
/// of its partition's lists.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RootIndexSegment {
    pub graph: GraphId,
    pub branch: BranchId,
    pub partition: u64,
    pub class: SegmentClass,
    pub segments: Vec<SegmentRef>,
}

/// Why a segment could not be encoded, decoded, or bound to its root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SegmentError {
    NotASegment,
    NotAnIndexSegment,
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
    IndexNotFull {
        segments: usize,
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
            Self::NotAnIndexSegment => write!(f, "not a strata root index segment"),
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
            Self::IndexNotFull { segments } => write!(
                f,
                "a root index segment names exactly {INDEX_REFS} segments, not {segments}"
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
    encode_header(
        &mut out,
        (SEGMENT_MAGIC, SEGMENT_FORMAT_V1),
        (graph, branch, partition),
        class,
        &digest,
    );
    for entry in entries {
        out.extend_from_slice(&entry.id.0);
        out.extend_from_slice(&entry.first_seq.0.to_be_bytes());
        out.extend_from_slice(&entry.last_seq.0.to_be_bytes());
    }
    Ok((out, digest))
}

/// The header both segment kinds share: magic, format, the partition
/// coordinate, the class, and the logical digest.
fn encode_header(
    out: &mut Vec<u8>,
    (magic, format): ([u8; 4], u16),
    (graph, branch, partition): (GraphId, BranchId, u64),
    class: SegmentClass,
    digest: &[u8; 32],
) {
    out.extend_from_slice(&magic);
    out.extend_from_slice(&format.to_be_bytes());
    out.extend_from_slice(&graph.0.to_be_bytes());
    out.extend_from_slice(&branch.0.to_be_bytes());
    out.extend_from_slice(&partition.to_be_bytes());
    out.push(class.tag());
    out.extend_from_slice(digest);
}

/// A decoded shared header: the coordinate, the class, and the declared
/// logical digest.
type Header = ((GraphId, BranchId, u64), SegmentClass, [u8; 32]);

/// Decode the shared header of a segment kind, refusing any other magic or
/// format, and any length but the kind's one fixed length.
fn decode_header(
    bytes: &[u8],
    (magic, format): ([u8; 4], u16),
    length: usize,
    not_this_kind: SegmentError,
) -> Result<Header, SegmentError> {
    if bytes.len() < HEADER_LEN || bytes[..4] != magic {
        return Err(not_this_kind);
    }
    let found = u16::from_be_bytes([bytes[4], bytes[5]]);
    if found != format {
        return Err(SegmentError::UnsupportedFormat { format: found });
    }
    if bytes.len() != length {
        return Err(SegmentError::Length {
            expected: length,
            found: bytes.len(),
        });
    }
    let class = SegmentClass::from_tag(bytes[OFF_CLASS]).ok_or(SegmentError::UnknownClass {
        class: bytes[OFF_CLASS],
    })?;
    let mut graph = [0u8; 16];
    graph.copy_from_slice(&bytes[OFF_GRAPH..OFF_GRAPH + 16]);
    let mut branch = [0u8; 16];
    branch.copy_from_slice(&bytes[OFF_BRANCH..OFF_BRANCH + 16]);
    let coordinate = (
        GraphId(u128::from_be_bytes(graph)),
        BranchId(u128::from_be_bytes(branch)),
        u64_at(bytes, OFF_PARTITION),
    );
    Ok((coordinate, class, bytes32_at(bytes, OFF_DIGEST)))
}

fn u64_at(bytes: &[u8], at: usize) -> u64 {
    let mut buf = [0u8; 8];
    buf.copy_from_slice(&bytes[at..at + 8]);
    u64::from_be_bytes(buf)
}

fn bytes32_at(bytes: &[u8], at: usize) -> [u8; 32] {
    let mut buf = [0u8; 32];
    buf.copy_from_slice(&bytes[at..at + 32]);
    buf
}

/// Decode a segment, re-checking its length, class and digest. Returns the
/// segment and its verified logical digest.
pub fn decode_segment(bytes: &[u8]) -> Result<(RootSegment, [u8; 32]), SegmentError> {
    let ((graph, branch, partition), class, declared) = decode_header(
        bytes,
        (SEGMENT_MAGIC, SEGMENT_FORMAT_V1),
        SEGMENT_BYTES,
        SegmentError::NotASegment,
    )?;
    let entries: Vec<SegmentEntry> = (0..SEGMENT_REFS)
        .map(|index| {
            let at = HEADER_LEN + index * REF_LEN;
            SegmentEntry {
                id: ObjectId(bytes32_at(bytes, at)),
                first_seq: CommitSeq(u64_at(bytes, at + 32)),
                last_seq: CommitSeq(u64_at(bytes, at + 40)),
            }
        })
        .collect();
    let segment = RootSegment {
        graph,
        branch,
        partition,
        class,
        entries,
    };
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

/// An index segment's canonical logical digest: UNKEYED, domain-separated,
/// over its coordinate, class, and each named segment's logical digest and
/// span. Each of those digests binds that segment's entries, so this binds the
/// index's whole logical content without depending on identity keys.
pub fn index_logical_digest(
    graph: GraphId,
    branch: BranchId,
    partition: u64,
    class: SegmentClass,
    segments: &[SegmentRef],
) -> [u8; 32] {
    let mut hasher = fgdb_crypto::Hasher::new();
    hasher.update(INDEX_LOGICAL_DIGEST_DOMAIN);
    hasher.update(&graph.0.to_be_bytes());
    hasher.update(&branch.0.to_be_bytes());
    hasher.update(&partition.to_be_bytes());
    hasher.update(&[class.tag()]);
    hasher.update(&(segments.len() as u32).to_be_bytes());
    for segment in segments {
        hasher.update(&segment.digest);
        hasher.update(&segment.first_seq.0.to_be_bytes());
        hasher.update(&segment.last_seq.0.to_be_bytes());
    }
    hasher.finalize().0
}

/// Encode one full index segment, returning its bytes and logical digest.
pub fn encode_index(
    graph: GraphId,
    branch: BranchId,
    partition: u64,
    class: SegmentClass,
    segments: &[SegmentRef],
) -> Result<(Vec<u8>, [u8; 32]), SegmentError> {
    if segments.len() != INDEX_REFS {
        return Err(SegmentError::IndexNotFull {
            segments: segments.len(),
        });
    }
    let digest = index_logical_digest(graph, branch, partition, class, segments);
    let mut out = Vec::with_capacity(INDEX_BYTES);
    encode_header(
        &mut out,
        (INDEX_MAGIC, INDEX_FORMAT_V1),
        (graph, branch, partition),
        class,
        &digest,
    );
    for segment in segments {
        out.extend_from_slice(&segment.segment_id.0);
        out.extend_from_slice(&segment.digest);
        out.extend_from_slice(&segment.first_seq.0.to_be_bytes());
        out.extend_from_slice(&segment.last_seq.0.to_be_bytes());
    }
    Ok((out, digest))
}

/// Decode an index segment, re-checking its length, class and digest.
/// Returns the index segment and its verified logical digest.
pub fn decode_index(bytes: &[u8]) -> Result<(RootIndexSegment, [u8; 32]), SegmentError> {
    let ((graph, branch, partition), class, declared) = decode_header(
        bytes,
        (INDEX_MAGIC, INDEX_FORMAT_V1),
        INDEX_BYTES,
        SegmentError::NotAnIndexSegment,
    )?;
    let segments: Vec<SegmentRef> = (0..INDEX_REFS)
        .map(|index| {
            let at = HEADER_LEN + index * SEGMENT_REF_LEN;
            SegmentRef {
                segment_id: ObjectId(bytes32_at(bytes, at)),
                digest: bytes32_at(bytes, at + 32),
                first_seq: CommitSeq(u64_at(bytes, at + 64)),
                last_seq: CommitSeq(u64_at(bytes, at + 72)),
            }
        })
        .collect();
    let recomputed = index_logical_digest(graph, branch, partition, class, &segments);
    if declared != recomputed {
        return Err(SegmentError::DigestMismatch {
            declared,
            recomputed,
        });
    }
    Ok((
        RootIndexSegment {
            graph,
            branch,
            partition,
            class,
            segments,
        },
        recomputed,
    ))
}

/// The content identity of an encoded index segment, under its registered
/// kind.
pub fn index_id(
    k_oid: &[u8; 32],
    namespace: DatabaseSecurityNamespaceId,
    bytes: &[u8],
) -> ObjectId {
    ObjectId(
        fgdb_crypto::logical_object_id(
            k_oid,
            &namespace.0,
            &INDEX_OBJECT_KIND.to_le_bytes(),
            bytes,
        )
        .0,
    )
}

/// The span a full index segment covers, as its root names it: the least
/// first sequence of its segments, and the last segment's last sequence.
pub fn index_span(segments: &[SegmentRef]) -> (CommitSeq, CommitSeq) {
    let first = segments
        .iter()
        .map(|segment| segment.first_seq)
        .min()
        .unwrap_or(CommitSeq(0));
    let last = segments
        .last()
        .map_or(CommitSeq(0), |segment| segment.last_seq);
    (first, last)
}

/// Decode the index segment a root names as its `at`-th index of `class`,
/// proving it is that index segment: its identity, coordinate, class, digest
/// and span must all be the ones the root binds. Returns the segment
/// references it names, each still to be read with [`read_segment`].
pub fn read_index(
    k_oid: &[u8; 32],
    namespace: DatabaseSecurityNamespaceId,
    bytes: &[u8],
    named: &SegmentRef,
    at: usize,
    coordinate: (GraphId, BranchId, u64),
    class: SegmentClass,
) -> Result<Vec<SegmentRef>, SegmentError> {
    let actual = index_id(k_oid, namespace, bytes);
    if actual != named.segment_id {
        return Err(SegmentError::IdentityMismatch {
            expected: named.segment_id,
            actual,
        });
    }
    let (index, digest) = decode_index(bytes)?;
    if (index.graph, index.branch, index.partition) != coordinate {
        return Err(SegmentError::Binding {
            at,
            what: "index partition coordinate",
        });
    }
    if index.class != class {
        return Err(SegmentError::Binding {
            at,
            what: "index class",
        });
    }
    if digest != named.digest {
        return Err(SegmentError::Binding {
            at,
            what: "index digest",
        });
    }
    if index_span(&index.segments) != (named.first_seq, named.last_seq) {
        return Err(SegmentError::Binding {
            at,
            what: "index span",
        });
    }
    Ok(index.segments)
}
