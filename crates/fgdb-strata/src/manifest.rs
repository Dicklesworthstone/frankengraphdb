//! The partition manifest: the durable object `root_manifest_oid` resolves to
//! (`fgdb-63w2`, the mechanism half of `fgdb-ge6a`).
//!
//! A [`crate::root::PartitionRoot`] makes ONE partition reopenable from a
//! 32-byte identity. Nothing durable named those identities: every reopen
//! either replayed the commit stream or held a root identity in memory across
//! the close, which is not a reopen. The manifest closes that gap — it is the
//! object a database's root slot reaches, and per live `(graph, branch,
//! partition)` coordinate it names exactly one partition-root identity.
//!
//! **THE MANIFEST DOES NOT DESCRIBE PARTITION CONTENTS.** The root already
//! carries its full block and patch reference lists; duplicating any of that
//! here would create a second authority that could disagree with the first.
//! One record is one coordinate and one 32-byte identity, nothing else.
//!
//! **THIS SLICE IS A FLAT LIST, AND SAYS SO.** The ge6a analysis names the
//! growth hazard honestly: rewriting one object per publish is O(partitions),
//! and a persistent two-level shape is what keeps branch forks O(1). The
//! format is versioned from day one (§16.6); the two-level shape is a
//! breaking-major successor argued on that bead, not an ambiguity in this one.

use crate::root::PartitionRoot;
use fgdb_types::ids::{DatabaseSecurityNamespaceId, ObjectId};
use fgdb_types::{BranchId, GraphId};

/// `FGSM` — FrankenGraph Strata Manifest.
pub const MANIFEST_MAGIC: [u8; 4] = *b"FGSM";
/// Format version, versioned from day one (§16.6).
pub const MANIFEST_FORMAT_V1: u16 = 1;
/// Durable object kind for the §5.1 logical-identity header — distinct from
/// blocks (0x0301), vertex patches (0x0302), and edge property patches
/// (0x0303) for the reason each of those is distinct: equal payload bytes
/// must never alias a different kind's identity.
pub const MANIFEST_OBJECT_KIND: u16 = 0x0304;

/// The largest number of records this build will read from one manifest.
pub const MAX_MANIFEST_RECORDS: u32 = 1 << 20;

/// magic + format + record count.
const HEADER_LEN: usize = 4 + 2 + 4;
/// graph(16) + branch(16) + partition(8) + root(32).
const RECORD_LEN: usize = 16 + 16 + 8 + 32;

/// One live partition: its coordinate and the identity of its published root.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ManifestRecord {
    pub graph: GraphId,
    pub branch: BranchId,
    pub partition: u64,
    /// The published [`PartitionRoot`]'s content identity.
    pub root: crate::PartitionRootVersion,
}

/// The content identity of one immutable manifest version.
///
/// The same semantic-boundary argument as [`crate::DeltaBlockVersion`]: a
/// manifest identity handed to a block or root resolver must be a type error,
/// not a wrong decoder's refusal.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct ManifestVersion(pub ObjectId);

/// Why a manifest could not be encoded or decoded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ManifestError {
    NotAManifest,
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
    /// Records must be strictly ascending by `(graph, branch, partition)`:
    /// one coordinate, one root — a duplicate would make "the partition's
    /// state" ambiguous, which is the exact question this object answers.
    NonCanonicalOrder {
        at: usize,
    },
    /// A record names the zero identity, which is no object's name.
    ZeroRoot {
        at: usize,
    },
    ImplausibleRecordCount {
        declared: u32,
    },
    /// The bytes are well-formed and are not the manifest that was asked for.
    IdentityMismatch {
        expected: ObjectId,
        actual: ObjectId,
    },
}

impl core::fmt::Display for ManifestError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NotAManifest => write!(f, "not a strata partition manifest"),
            Self::UnsupportedFormat { format } => {
                write!(f, "manifest format {format} is not implemented")
            }
            Self::Truncated { expected, found } => {
                write!(f, "manifest declares {expected} bytes, found {found}")
            }
            Self::TrailingBytes { extra } => write!(f, "{extra} bytes after the last record"),
            Self::NonCanonicalOrder { at } => write!(
                f,
                "record {at} does not strictly follow its predecessor's coordinate"
            ),
            Self::ZeroRoot { at } => write!(f, "record {at} names the zero identity"),
            Self::ImplausibleRecordCount { declared } => {
                write!(
                    f,
                    "a manifest declaring {declared} records is not readable here"
                )
            }
            Self::IdentityMismatch { expected, actual } => write!(
                f,
                "these bytes are manifest {actual:?}, not the requested {expected:?}"
            ),
        }
    }
}

impl core::error::Error for ManifestError {}

/// Encode `records` into a canonical manifest.
///
/// REFUSES rather than sorts, for the reason every encoder here does: a
/// caller handing over a different order is describing a different intent.
pub fn encode_manifest(records: &[ManifestRecord]) -> Result<Vec<u8>, ManifestError> {
    let count = u32::try_from(records.len())
        .map_err(|_| ManifestError::ImplausibleRecordCount { declared: u32::MAX })?;
    if count > MAX_MANIFEST_RECORDS {
        return Err(ManifestError::ImplausibleRecordCount { declared: count });
    }
    validate_records(records)?;
    let mut out = Vec::with_capacity(HEADER_LEN + records.len() * RECORD_LEN);
    out.extend_from_slice(&MANIFEST_MAGIC);
    out.extend_from_slice(&MANIFEST_FORMAT_V1.to_be_bytes());
    out.extend_from_slice(&count.to_be_bytes());
    for record in records {
        out.extend_from_slice(&record.graph.0.to_be_bytes());
        out.extend_from_slice(&record.branch.0.to_be_bytes());
        out.extend_from_slice(&record.partition.to_be_bytes());
        out.extend_from_slice(&record.root.0.0);
    }
    Ok(out)
}

/// Decode a manifest, independently re-checking every canonical law.
pub fn decode_manifest(bytes: &[u8]) -> Result<Vec<ManifestRecord>, ManifestError> {
    if bytes.len() < HEADER_LEN || bytes[..4] != MANIFEST_MAGIC {
        return Err(ManifestError::NotAManifest);
    }
    let format = u16::from_be_bytes([bytes[4], bytes[5]]);
    if format != MANIFEST_FORMAT_V1 {
        return Err(ManifestError::UnsupportedFormat { format });
    }
    let count = u32::from_be_bytes(bytes[6..10].try_into().expect("fixed header"));
    if count > MAX_MANIFEST_RECORDS {
        return Err(ManifestError::ImplausibleRecordCount { declared: count });
    }
    let expected = HEADER_LEN + count as usize * RECORD_LEN;
    if bytes.len() < expected {
        return Err(ManifestError::Truncated {
            expected,
            found: bytes.len(),
        });
    }
    if bytes.len() > expected {
        return Err(ManifestError::TrailingBytes {
            extra: bytes.len() - expected,
        });
    }
    let mut records = Vec::with_capacity(count as usize);
    let mut at = HEADER_LEN;
    for _ in 0..count {
        let graph = GraphId(u128::from_be_bytes(
            bytes[at..at + 16].try_into().expect("bounded record"),
        ));
        let branch = BranchId(u128::from_be_bytes(
            bytes[at + 16..at + 32].try_into().expect("bounded record"),
        ));
        let partition =
            u64::from_be_bytes(bytes[at + 32..at + 40].try_into().expect("bounded record"));
        let root: [u8; 32] = bytes[at + 40..at + 72].try_into().expect("bounded record");
        records.push(ManifestRecord {
            graph,
            branch,
            partition,
            root: crate::PartitionRootVersion(ObjectId(root)),
        });
        at += RECORD_LEN;
    }
    validate_records(&records)?;
    Ok(records)
}

fn validate_records(records: &[ManifestRecord]) -> Result<(), ManifestError> {
    for (at, record) in records.iter().enumerate() {
        if record.root.0.0 == [0u8; 32] {
            return Err(ManifestError::ZeroRoot { at });
        }
        if at > 0 {
            let previous = &records[at - 1];
            if (previous.graph, previous.branch, previous.partition)
                >= (record.graph, record.branch, record.partition)
            {
                return Err(ManifestError::NonCanonicalOrder { at });
            }
        }
    }
    Ok(())
}

/// The content identity of an encoded manifest, under its own §5.1 kind.
pub fn manifest_id(
    k_oid: &[u8; 32],
    namespace: DatabaseSecurityNamespaceId,
    bytes: &[u8],
) -> ObjectId {
    ObjectId(
        fgdb_crypto::logical_object_id(
            k_oid,
            &namespace.0,
            &MANIFEST_OBJECT_KIND.to_le_bytes(),
            bytes,
        )
        .0,
    )
}

/// Decode a manifest that must be the one named by `expected`.
pub fn read_manifest(
    k_oid: &[u8; 32],
    namespace: DatabaseSecurityNamespaceId,
    bytes: &[u8],
    expected: ManifestVersion,
) -> Result<Vec<ManifestRecord>, ManifestError> {
    let actual = manifest_id(k_oid, namespace, bytes);
    if actual != expected.0 {
        return Err(ManifestError::IdentityMismatch {
            expected: expected.0,
            actual,
        });
    }
    decode_manifest(bytes)
}

/// The record set a publish derives from its live roots — the ONE lawful
/// construction, so callers cannot disagree about ordering by building
/// records ad hoc.
pub fn records_of(
    roots: &[(PartitionRoot, crate::PartitionRootVersion)],
) -> Result<Vec<ManifestRecord>, ManifestError> {
    let mut records: Vec<ManifestRecord> = roots
        .iter()
        .map(|(root, id)| ManifestRecord {
            graph: root.graph,
            branch: root.branch,
            partition: root.partition,
            root: *id,
        })
        .collect();
    records.sort();
    validate_records(&records)?;
    Ok(records)
}
