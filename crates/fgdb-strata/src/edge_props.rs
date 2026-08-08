//! The block-hosted edge property patch (`fgdb-yqor`, the first increment of
//! `fgdb-w3-properties-gou`'s edge side, in the shape ruling fgdb-2t7q 3B
//! reserved): a block-level set of property patches indexed by a per-entry
//! `prop_row_ref` locator.
//!
//! Until this module existed, an edge's properties were durable in the commit
//! stream and absent from tier D — `CreateEdge` carries them, the oracle
//! materializes them, and no block byte held them. `FGSP` is the object that
//! closes that gap: one patch per block (in this slice), holding the property
//! lists of the block's propertied entries POSITIONALLY — the block's locator
//! column says which entry owns which row, and the two objects are bound by
//! the bijection law [`validate_block_patch_consistency`] enforces.
//!
//! **THE SAME EXPOSURE NOTE AS `FGSB`/`FGSR`/`FGSV` APPLIES**: these bytes are
//! deliberately unregistered; the normative sealed property layout is
//! `fgdb-w3-properties-gou`'s, which absorbs this. The object kind carries the
//! interim numbering caveat of [`crate::DELTA_BLOCK_OBJECT_KIND`].
//!
//! **CANONICAL MEANS EXACTLY ONE BYTE STRING PER VALUE**: rows hold strictly
//! ascending property keys; values go through [`CanonicalScalar::encode`],
//! the one definition of a property value's bytes; and the locator bijection
//! (rows referenced exactly once, in entry order) means a patch cannot hold
//! an unreferenced row or serve two entries with one row.

use fgdb_delta_types::PropertyKeyId;
use fgdb_types::CanonicalScalar;
use fgdb_types::ids::{DatabaseSecurityNamespaceId, ObjectId};

/// `FGSP` — FrankenGraph Strata Property patch.
pub const PROPERTY_PATCH_MAGIC: [u8; 4] = *b"FGSP";

/// Format version. Durable formats are versioned from day one (§16.6).
pub const PROPERTY_PATCH_FORMAT_V1: u16 = 1;

/// Durable object kind for a block-hosted edge property patch — §5.1
/// logical-identity header, with the same interim-numbering caveat as
/// [`crate::DELTA_BLOCK_OBJECT_KIND`].
pub const PROPERTY_PATCH_OBJECT_KIND: u16 = 0x0303;

/// The most rows one patch can hold: the block's u8 locator addresses rows
/// `1..=255` (0 is "no properties"), so this is a FORMAT ceiling shared with
/// the locator column, not a seal policy. A block whose propertied entries
/// would exceed it seals early, exactly like the entry-count ceiling.
pub const MAX_PROPERTY_PATCH_ROWS: u32 = 255;

/// The content identity of one immutable edge property patch — the same
/// semantic type boundary as `DeltaBlockVersion` and `VertexPatchVersion`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct EdgePropertyPatchVersion(pub ObjectId);

/// One entry's property list: strictly ascending by key, values canonical.
pub type EdgePropertyRow = Vec<(PropertyKeyId, CanonicalScalar)>;

/// A block's decoded property sidecar: the locator column and the hosted
/// patch's rows, kept together because neither means anything alone.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct BlockProps {
    pub locators: Vec<u8>,
    pub rows: Vec<EdgePropertyRow>,
}

impl BlockProps {
    /// The property list of the entry at `index`, empty when the entry's
    /// locator is 0 or the block hosts no patch at all.
    pub fn props_of(&self, index: usize) -> EdgePropertyRow {
        match self.locators.get(index) {
            Some(&locator) if locator != 0 => self
                .rows
                .get(usize::from(locator) - 1)
                .cloned()
                .unwrap_or_default(),
            _ => Vec::new(),
        }
    }
}

/// Why an edge property patch could not be encoded, decoded, or trusted —
/// the same boundary set as the sibling formats.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EdgePropertyPatchError {
    /// The bytes do not begin with [`PROPERTY_PATCH_MAGIC`].
    NotAPropertyPatch,
    /// A format version this build does not implement.
    UnsupportedFormat { format: u16 },
    /// The bytes end before the declared rows do.
    Truncated { at: usize },
    /// Bytes remain after the declared rows.
    TrailingBytes { extra: usize },
    /// A row's property keys are not strictly ascending.
    NonCanonicalRow { at: usize },
    /// An EMPTY row: a propertyless entry is locator 0, never an empty row,
    /// so an empty row is an unreferenced slot that shifts every later
    /// locator — refused as non-canonical.
    EmptyRow { at: usize },
    /// More rows than the locator column can address.
    ImplausibleRowCount { declared: u32 },
    /// A property value refused canonical encoding.
    ScalarEncode {
        at: usize,
        error: fgdb_types::ScalarEncodeError,
    },
    /// A property value's bytes refused canonical decoding.
    ScalarDecode {
        at: usize,
        error: fgdb_types::ScalarDecodeError,
    },
    /// The bytes are well-formed and WRONG — not the patch asked for.
    IdentityMismatch {
        expected: ObjectId,
        actual: ObjectId,
    },
    /// The block's locator column and this patch disagree: a locator points
    /// past the rows, a row is never referenced, or references are out of
    /// entry order — the bijection is the contract that makes a positional
    /// patch trustworthy, so every violation is one refusal.
    LocatorBijectionViolation {
        entry_at: usize,
        expected: u8,
        found: u8,
    },
    /// The patch holds rows no locator references (the tail of the
    /// bijection: locators covered rows `1..=n`, the patch declared more).
    UnreferencedRows { referenced: usize, declared: usize },
}

impl core::fmt::Display for EdgePropertyPatchError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NotAPropertyPatch => write!(f, "not a strata edge property patch"),
            Self::UnsupportedFormat { format } => {
                write!(f, "edge property patch format {format} is not implemented")
            }
            Self::Truncated { at } => {
                write!(
                    f,
                    "property patch bytes end inside the structure at offset {at}"
                )
            }
            Self::TrailingBytes { extra } => write!(f, "{extra} bytes after the last row"),
            Self::NonCanonicalRow { at } => {
                write!(f, "row {at} property keys must be strictly ascending")
            }
            Self::EmptyRow { at } => {
                write!(
                    f,
                    "row {at} is empty; a propertyless entry is locator 0, never a row"
                )
            }
            Self::ImplausibleRowCount { declared } => write!(
                f,
                "{declared} rows exceeds the {MAX_PROPERTY_PATCH_ROWS}-row locator ceiling"
            ),
            Self::ScalarEncode { at, error } => {
                write!(f, "row {at} property value refused encoding: {error:?}")
            }
            Self::ScalarDecode { at, error } => {
                write!(f, "row {at} property value refused decoding: {error:?}")
            }
            Self::IdentityMismatch { expected, actual } => {
                write!(f, "bytes are {actual:?}, not the requested {expected:?}")
            }
            Self::LocatorBijectionViolation {
                entry_at,
                expected,
                found,
            } => write!(
                f,
                "entry {entry_at} carries locator {found}, but the bijection requires {expected}: \
                 propertied entries reference rows 1..=n in entry order, exactly once"
            ),
            Self::UnreferencedRows {
                referenced,
                declared,
            } => write!(
                f,
                "the locators reference {referenced} rows but the patch declares {declared}; \
                 an unreferenced row is dead weight no reader can prove correct"
            ),
        }
    }
}

impl core::error::Error for EdgePropertyPatchError {}

fn validate_row(at: usize, row: &EdgePropertyRow) -> Result<(), EdgePropertyPatchError> {
    if row.is_empty() {
        return Err(EdgePropertyPatchError::EmptyRow { at });
    }
    if row.windows(2).any(|pair| pair[0].0.0 >= pair[1].0.0) {
        return Err(EdgePropertyPatchError::NonCanonicalRow { at });
    }
    Ok(())
}

/// Encode rows into one canonical patch, refusing non-canonical input.
pub fn encode_property_patch(rows: &[EdgePropertyRow]) -> Result<Vec<u8>, EdgePropertyPatchError> {
    let count = u32::try_from(rows.len())
        .map_err(|_| EdgePropertyPatchError::ImplausibleRowCount { declared: u32::MAX })?;
    if count > MAX_PROPERTY_PATCH_ROWS {
        return Err(EdgePropertyPatchError::ImplausibleRowCount { declared: count });
    }
    let mut out = Vec::new();
    out.extend_from_slice(&PROPERTY_PATCH_MAGIC);
    out.extend_from_slice(&PROPERTY_PATCH_FORMAT_V1.to_le_bytes());
    out.extend_from_slice(&count.to_le_bytes());
    for (at, row) in rows.iter().enumerate() {
        validate_row(at, row)?;
        let props = u32::try_from(row.len()).expect("row length bounded by canonical admission");
        out.extend_from_slice(&props.to_le_bytes());
        for (key, value) in row {
            let encoded = value
                .encode()
                .map_err(|error| EdgePropertyPatchError::ScalarEncode { at, error })?;
            out.extend_from_slice(&key.0.to_le_bytes());
            let len = u32::try_from(encoded.len()).expect("scalar profile bounds its encoding");
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(&encoded);
        }
    }
    Ok(out)
}

/// Decode a patch, independently re-checking every canonical law.
pub fn decode_property_patch(bytes: &[u8]) -> Result<Vec<EdgePropertyRow>, EdgePropertyPatchError> {
    let mut at = 0usize;
    let take = |at: &mut usize, n: usize| -> Result<usize, EdgePropertyPatchError> {
        let end = at
            .checked_add(n)
            .filter(|&end| end <= bytes.len())
            .ok_or(EdgePropertyPatchError::Truncated { at: *at })?;
        let start = *at;
        *at = end;
        Ok(start)
    };
    let start = take(&mut at, 4)?;
    if bytes[start..start + 4] != PROPERTY_PATCH_MAGIC {
        return Err(EdgePropertyPatchError::NotAPropertyPatch);
    }
    let start = take(&mut at, 2)?;
    let format = u16::from_le_bytes(bytes[start..start + 2].try_into().expect("two bytes"));
    if format != PROPERTY_PATCH_FORMAT_V1 {
        return Err(EdgePropertyPatchError::UnsupportedFormat { format });
    }
    let start = take(&mut at, 4)?;
    let declared = u32::from_le_bytes(bytes[start..start + 4].try_into().expect("four bytes"));
    if declared > MAX_PROPERTY_PATCH_ROWS {
        return Err(EdgePropertyPatchError::ImplausibleRowCount { declared });
    }
    let mut rows = Vec::with_capacity(declared as usize);
    for row_at in 0..declared as usize {
        let start = take(&mut at, 4)?;
        let props = u32::from_le_bytes(bytes[start..start + 4].try_into().expect("four bytes"));
        let mut row: EdgePropertyRow =
            Vec::with_capacity(props.min(MAX_PROPERTY_PATCH_ROWS) as usize);
        for _ in 0..props {
            let start = take(&mut at, 8)?;
            let key = PropertyKeyId(u64::from_le_bytes(
                bytes[start..start + 8].try_into().expect("eight"),
            ));
            let start = take(&mut at, 4)?;
            let len = u32::from_le_bytes(bytes[start..start + 4].try_into().expect("four bytes"))
                as usize;
            let start = take(&mut at, len)?;
            // This is the closed graph-value decoder; no JWT or signature state exists here.
            // ubs:ignore -- exact false match is `CanonicalScalar::decode`, not a JWT decoder.
            let value = CanonicalScalar::decode(&bytes[start..start + len])
                .map_err(|error| EdgePropertyPatchError::ScalarDecode { at: row_at, error })?;
            row.push((key, value));
        }
        validate_row(row_at, &row)?;
        rows.push(row);
    }
    if at != bytes.len() {
        return Err(EdgePropertyPatchError::TrailingBytes {
            extra: bytes.len() - at,
        });
    }
    Ok(rows)
}

/// The §5.1 logical object identity of a patch's canonical bytes.
pub fn property_patch_id(
    k_oid: &[u8; 32],
    namespace: DatabaseSecurityNamespaceId,
    bytes: &[u8],
) -> ObjectId {
    ObjectId(
        fgdb_crypto::logical_object_id(
            k_oid,
            &namespace.0,
            &PROPERTY_PATCH_OBJECT_KIND.to_le_bytes(),
            bytes,
        )
        .0,
    )
}

/// Decode a patch that must be the one named by `expected`.
pub fn read_property_patch(
    k_oid: &[u8; 32],
    namespace: DatabaseSecurityNamespaceId,
    bytes: &[u8],
    expected: EdgePropertyPatchVersion,
) -> Result<Vec<EdgePropertyRow>, EdgePropertyPatchError> {
    let actual = property_patch_id(k_oid, namespace, bytes);
    if actual != expected.0 {
        return Err(EdgePropertyPatchError::IdentityMismatch {
            expected: expected.0,
            actual,
        });
    }
    decode_property_patch(bytes)
}

/// The locator column's own canonical law: non-zero locators are exactly
/// `1..=n` in entry order — each patch row referenced once, in the order the
/// entries appear. Returns `n`. The block decoder enforces this WITHOUT the
/// patch in hand, so a block with a scrambled locator column refuses on its
/// own bytes.
pub fn validate_locator_sequence(locators: &[u8]) -> Result<usize, EdgePropertyPatchError> {
    let mut next: u8 = 1;
    for (entry_at, &locator) in locators.iter().enumerate() {
        if locator == 0 {
            continue;
        }
        if locator != next {
            return Err(EdgePropertyPatchError::LocatorBijectionViolation {
                entry_at,
                expected: next,
                found: locator,
            });
        }
        next = next
            .checked_add(1)
            .ok_or(EdgePropertyPatchError::ImplausibleRowCount { declared: u32::MAX })?;
    }
    Ok(usize::from(next - 1))
}

/// The block↔patch bijection law: the locator sequence is lawful AND the
/// patch declares exactly the referenced row count. Admission calls this with
/// both objects in hand; either side alone can be individually lawful and
/// jointly a lie, which is why the joint half is one function.
pub fn validate_block_patch_consistency(
    locators: &[u8],
    patch_rows: usize,
) -> Result<(), EdgePropertyPatchError> {
    let referenced = validate_locator_sequence(locators)?;
    if referenced != patch_rows {
        return Err(EdgePropertyPatchError::UnreferencedRows {
            referenced,
            declared: patch_rows,
        });
    }
    Ok(())
}
