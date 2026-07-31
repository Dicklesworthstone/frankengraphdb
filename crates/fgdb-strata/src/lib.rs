//! `fgdb-strata` — B2's graph-structured LSM. **This activation lands tier one's
//! durable artifact and nothing else.**
//!
//! §2 of the plan puts adjacency in three temperature tiers: versioned delta
//! blocks, sealed compressed CSR runs, archived anchors. What is here is the
//! first tier's *format*: a sorted, versioned adjacency run with an exact byte
//! layout, a fail-closed decoder, and scans that read from the encoded bytes.
//!
//! **WHY A FORMAT AND NOT A MAP, WHICH IS THE INTERESTING PART.** The obvious
//! first slice is a `BTreeMap<(VId, RelationId, VId), _>` with the tier's
//! visibility semantics on top. It would be smaller, it would pass a differential
//! against the reference oracle, and it would be the exact thing doctrine 7
//! prohibits: "no `HashMap<VId, Vec<EId>>` presented as storage", and "early code
//! may implement a subset of a final abstraction — never a substitute for it". An
//! in-memory map with the right answers is a substitute for storage; it has no
//! durable form, so nothing about it can be wrong in the way storage is wrong.
//!
//! A byte layout can be wrong in exactly those ways, which is why it is the honest
//! slice: it can be non-canonical, it can be truncated, it can decode to something
//! other than what was encoded, and it can lie about ordering. Every one of those
//! is a law below.
//!
//! **WHAT IS DELIBERATELY ABSENT**, so the gap is legible rather than implied:
//! sealing a block, compressed CSR runs, archived anchors, the stable-ID
//! directory, tier migration and its decision cards, compaction, and any
//! in-memory index over blocks. A block is written once and read back; nothing
//! here manages a set of them.
//!
//! **CANONICAL MEANS EXACTLY ONE BYTE STRING PER VALUE** (doctrine 4). Entries are
//! strictly ascending by `(src, relation, dst)` and the ENCODER refuses input that
//! is not, rather than sorting it: a caller handing over a different order is
//! describing a different intent, and quietly repairing it would let two callers
//! disagree about what they stored while both succeed. The DECODER independently
//! refuses a block whose entries are not ascending, so a hand-built block cannot
//! smuggle an order the encoder would never emit.

#![forbid(unsafe_code)]

pub mod root;

use fgdb_delta_types::RelationId;
use fgdb_types::ids::{DatabaseSecurityNamespaceId, ObjectId};
use fgdb_types::{CommitSeq, VId};

/// `FGSB` — FrankenGraph Strata Block.
pub const BLOCK_MAGIC: [u8; 4] = *b"FGSB";
/// Format version. Durable formats are versioned from day one (§16.6):
/// additive-minor, breaking-major.
pub const BLOCK_FORMAT_V1: u16 = 1;

/// Header: magic + format + entry count.
const HEADER_LEN: usize = 4 + 2 + 4;
/// src(16) + relation(8) + dst(16) + created(8) + retired(8)
const ENTRY_LEN: usize = 16 + 8 + 16 + 8 + 8;

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
    pub created_at: CommitSeq,
    /// The sequence that retired this entry, or `None` while it is live.
    pub retired_at: Option<CommitSeq>,
}

impl AdjacencyEntry {
    /// The sort key. Ordering is by identity alone — NOT by sequence — because a
    /// block is an adjacency index first: a scan for one `(src, relation)` must be
    /// able to find its entries contiguously, whatever order they were created in.
    fn key(&self) -> (VId, RelationId, VId) {
        (self.src, self.relation, self.dst)
    }

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
    UnsupportedFormat { format: u16 },
    /// The bytes end before the declared entries do.
    Truncated { expected: usize, found: usize },
    /// Bytes remain after the declared entries. Refused rather than ignored: a
    /// trailing region is either a second block someone concatenated or damage,
    /// and both are wrong to read past.
    TrailingBytes { extra: usize },
    /// Entries are not strictly ascending by `(src, relation, dst)`.
    ///
    /// Carries the position so a diagnostic can name the pair, since "this block
    /// is unsorted" is not actionable on a block with thousands of entries.
    NonCanonicalOrder { at: usize },
    /// An entry claims to have been retired at or before it was created.
    RetiredBeforeCreated {
        at: usize,
        created_at: CommitSeq,
        retired_at: CommitSeq,
    },
    /// An entry was created at sequence zero, which names the empty stream and
    /// can therefore never have created anything.
    CreatedAtZero { at: usize },
    /// More entries than this build will materialize from one block.
    ImplausibleEntryCount { declared: u32 },
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
        }
    }
}

impl core::error::Error for BlockError {}

/// The largest entry count this build will accept from a declared header.
///
/// A length prefix read from possibly-damaged bytes must be bounded before it is
/// used to size anything — the same rule the commit log's `MAX_ENTRY_BODY`
/// applies. Without it a corrupted count is an allocation request.
pub const MAX_BLOCK_ENTRIES: u32 = 1 << 24;

/// Encode `entries` into a canonical block.
///
/// REFUSES rather than sorts. A caller whose entries are out of order or repeated
/// is describing something other than what a block means, and quietly repairing it
/// would let two callers store different intents and both be told they succeeded.
pub fn encode_block(entries: &[AdjacencyEntry]) -> Result<Vec<u8>, BlockError> {
    if entries.len() as u64 > u64::from(MAX_BLOCK_ENTRIES) {
        return Err(BlockError::ImplausibleEntryCount {
            declared: MAX_BLOCK_ENTRIES,
        });
    }
    for (index, entry) in entries.iter().enumerate() {
        validate_entry(index, entry)?;
        if index > 0 && entries[index - 1].key() >= entry.key() {
            return Err(BlockError::NonCanonicalOrder { at: index });
        }
    }

    let mut out = Vec::with_capacity(HEADER_LEN + entries.len() * ENTRY_LEN);
    out.extend_from_slice(&BLOCK_MAGIC);
    out.extend_from_slice(&BLOCK_FORMAT_V1.to_be_bytes());
    out.extend_from_slice(&(entries.len() as u32).to_be_bytes());
    for entry in entries {
        out.extend_from_slice(&entry.src.0.to_be_bytes());
        out.extend_from_slice(&entry.relation.0.to_be_bytes());
        out.extend_from_slice(&entry.dst.0.to_be_bytes());
        out.extend_from_slice(&entry.created_at.0.to_be_bytes());
        // Zero encodes "live". Unambiguous because sequence zero is the empty
        // stream and can never retire anything — the same fact `CreatedAtZero`
        // refuses on the other side. A presence flag would give `None` two
        // spellings and break canonicality.
        out.extend_from_slice(&entry.retired_at.map_or(0, |r| r.0).to_be_bytes());
    }
    Ok(out)
}

fn validate_entry(index: usize, entry: &AdjacencyEntry) -> Result<(), BlockError> {
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

/// Read a block's header, returning the declared entry count.
fn read_header(bytes: &[u8]) -> Result<u32, BlockError> {
    if bytes.len() < HEADER_LEN || bytes[..4] != BLOCK_MAGIC {
        return Err(BlockError::NotABlock);
    }
    let format = u16::from_be_bytes([bytes[4], bytes[5]]);
    if format != BLOCK_FORMAT_V1 {
        return Err(BlockError::UnsupportedFormat { format });
    }
    let count = u32::from_be_bytes([bytes[6], bytes[7], bytes[8], bytes[9]]);
    if count > MAX_BLOCK_ENTRIES {
        return Err(BlockError::ImplausibleEntryCount { declared: count });
    }
    let expected = HEADER_LEN + count as usize * ENTRY_LEN;
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
    Ok(count)
}

fn read_entry(bytes: &[u8], index: usize) -> AdjacencyEntry {
    let at = HEADER_LEN + index * ENTRY_LEN;
    let u128_at = |off: usize| -> u128 {
        let mut buf = [0u8; 16];
        buf.copy_from_slice(&bytes[at + off..at + off + 16]);
        u128::from_be_bytes(buf)
    };
    let u64_at = |off: usize| -> u64 {
        let mut buf = [0u8; 8];
        buf.copy_from_slice(&bytes[at + off..at + off + 8]);
        u64::from_be_bytes(buf)
    };
    // Offsets: src 0..16, relation 16..24, dst 24..40, created 40..48,
    // retired 48..56. Written out because the first version of this function had
    // created_at at 32 — it treated `dst` as eight bytes rather than sixteen, and
    // the round-trip law caught it on the first run.
    let retired = u64_at(48);
    AdjacencyEntry {
        src: VId(u128_at(0)),
        relation: RelationId(u64_at(16)),
        dst: VId(u128_at(24)),
        created_at: CommitSeq(u64_at(40)),
        retired_at: (retired != 0).then_some(CommitSeq(retired)),
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
    let count = read_header(bytes)? as usize;
    let mut out = Vec::with_capacity(count);
    for index in 0..count {
        let entry = read_entry(bytes, index);
        validate_entry(index, &entry)?;
        if let Some(previous) = out.last()
            && AdjacencyEntry::key(previous) >= entry.key()
        {
            return Err(BlockError::NonCanonicalOrder { at: index });
        }
        out.push(entry);
    }
    Ok(out)
}

/// The neighbours of `src` over `relation` visible at `as_of`, ascending.
///
/// **Reads from the ENCODED bytes.** It walks entries in place and materializes
/// only the destinations it returns, rather than decoding the block and filtering
/// — which is the whole point of having a format: a scan for one adjacency must
/// not cost the whole block. The layout supports it because entries are sorted by
/// `(src, relation, dst)`, so one adjacency's entries are contiguous.
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
    let count = read_header(bytes)? as usize;
    let mut out = Vec::new();
    let mut previous: Option<(VId, RelationId, VId)> = None;
    for index in 0..count {
        let entry = read_entry(bytes, index);
        validate_entry(index, &entry)?;
        if let Some(prev) = previous
            && prev >= entry.key()
        {
            return Err(BlockError::NonCanonicalOrder { at: index });
        }
        previous = Some(entry.key());

        if entry.src == src && entry.relation == relation && entry.visible_at(as_of) {
            out.push(entry.dst);
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
/// key and security namespace rather than globally guessable.
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
    // Empty canonical header, payload is the block: the same shape
    // `IdentifiedObject::new` uses for a capsule's plaintext. The block's own
    // magic already separates it from any other payload with equal bytes.
    ObjectId(fgdb_crypto::logical_object_id(k_oid, &namespace.0, &[], bytes).0)
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
