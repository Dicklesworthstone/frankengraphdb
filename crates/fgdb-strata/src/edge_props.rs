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
use fgdb_types::ids::{DatabaseSecurityNamespaceId, ObjectId};
use fgdb_types::{
    CanonicalPropertyValue, CanonicalPropertyValueError, CanonicalScalar, CanonicalScalarResolver,
    ScalarProfileVersion,
};

/// `FGSP` — FrankenGraph Strata Property patch.
pub const PROPERTY_PATCH_MAGIC: [u8; 4] = *b"FGSP";

/// Format version. Durable formats are versioned from day one (§16.6).
pub const PROPERTY_PATCH_FORMAT_V1: u16 = 1;
/// V1 plus property values of scalar profile version 2 (fgdb-hbi8s). An
/// exact additive minor: a patch is V2 if and only if a row holds such a
/// value ([`property_patch_format_for`]), so every other patch keeps its V1
/// bytes and identity, and an older reader refuses V2 by name.
pub const PROPERTY_PATCH_FORMAT_V2: u16 = 2;
/// V2 plus List and Map property values (fgdb-k3nub), encoded as
/// [`CanonicalPropertyValue`] bytes whose scalars keep their scalar bytes.
/// Exact: a patch is V3 if and only if a row holds a collection.
pub const PROPERTY_PATCH_FORMAT_V3: u16 = 3;

/// The one format `rows` may be written in.
pub fn property_patch_format_for(rows: &[EdgePropertyRow]) -> u16 {
    let values = || rows.iter().flatten().map(|(_, value)| value);
    if values().any(CanonicalPropertyValue::is_collection) {
        return PROPERTY_PATCH_FORMAT_V3;
    }
    let profile = values()
        .map(CanonicalPropertyValue::required_scalar_profile)
        .max()
        .unwrap_or(ScalarProfileVersion::V1);
    if profile > ScalarProfileVersion::V1 {
        PROPERTY_PATCH_FORMAT_V2
    } else {
        PROPERTY_PATCH_FORMAT_V1
    }
}

/// Durable object kind for a block-hosted edge property patch — §5.1
/// logical-identity header, with the same interim-numbering caveat as
/// [`crate::DELTA_BLOCK_OBJECT_KIND`].
pub const PROPERTY_PATCH_OBJECT_KIND: u16 = 0x0580;

/// The most rows one patch can hold: the block's u8 locator addresses rows
/// `1..=255` (0 is "no properties"), so this is a FORMAT ceiling shared with
/// the locator column, not a seal policy. A block whose propertied entries
/// would exceed it seals early, exactly like the entry-count ceiling.
pub const MAX_PROPERTY_PATCH_ROWS: u32 = 255;

pub(crate) const PROPERTY_PATCH_HEADER_BYTES: u64 = 10;

/// The content identity of one immutable edge property patch — the same
/// semantic type boundary as `DeltaBlockVersion` and `VertexPatchVersion`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct EdgePropertyPatchVersion(pub ObjectId);

/// One entry's property list: strictly ascending by key, values canonical
/// scalars, Lists and Maps (fgdb-k3nub).
pub type EdgePropertyRow = Vec<(PropertyKeyId, CanonicalPropertyValue)>;

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
        self.props_ref(index).to_vec()
    }

    /// [`Self::props_of`] borrowed from the decoded patch, for a caller that
    /// only reads the row.
    pub fn props_ref(&self, index: usize) -> &[(PropertyKeyId, CanonicalPropertyValue)] {
        match self.locators.get(index) {
            Some(&locator) if locator != 0 => self
                .rows
                .get(usize::from(locator) - 1)
                .map_or(&[], Vec::as_slice),
            _ => &[],
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
    /// A supported format that is not the one the rows require
    /// ([`property_patch_format_for`]): V1 over a profile-2 value, V2 over
    /// rows V1 encodes, V1 or V2 over a collection, or V3 over scalar-only
    /// rows.
    FormatMismatch { format: u16, required: u16 },
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
    /// A single row cannot fit the store's current materialization budget.
    /// This is resource admission, not a durable-format maximum.
    RowExceedsStorageLimit { bytes: u64, limit: u64 },
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
    /// A List or Map property value refused canonical encoding or decoding.
    PropertyValue {
        at: usize,
        error: CanonicalPropertyValueError,
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
            Self::FormatMismatch { format, required } => write!(
                f,
                "edge property patch format {format} declared over rows that require format \
                 {required}"
            ),
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
            Self::RowExceedsStorageLimit { bytes, limit } => {
                write!(
                    f,
                    "edge property row needs {bytes} stored bytes; admission limit is {limit}"
                )
            }
            Self::ScalarDecode { at, error } => {
                write!(f, "row {at} property value refused decoding: {error:?}")
            }
            Self::PropertyValue { at, error } => {
                write!(f, "row {at} collection property value: {error}")
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

/// Canonically measure a row once for both sealing and compaction. A
/// propertyless entry uses no patch row. Indivisible rows must fit alone.
/// Returns the row bytes excluding the shared patch header. Write preparation
/// uses the same admission before its effects can enter Chronicle.
pub fn admitted_row_bytes(row: &EdgePropertyRow) -> Result<u64, EdgePropertyPatchError> {
    if row.is_empty() {
        return Ok(0);
    }
    let bytes = encode_property_patch(core::slice::from_ref(row))?.len() as u64;
    let limit = crate::store::MAX_STORED_OBJECT_BYTES;
    if bytes > limit {
        return Err(EdgePropertyPatchError::RowExceedsStorageLimit { bytes, limit });
    }
    Ok(bytes - PROPERTY_PATCH_HEADER_BYTES)
}

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
    out.extend_from_slice(&property_patch_format_for(rows).to_le_bytes());
    out.extend_from_slice(&count.to_le_bytes());
    for (at, row) in rows.iter().enumerate() {
        validate_row(at, row)?;
        let props = u32::try_from(row.len()).expect("row length bounded by canonical admission");
        out.extend_from_slice(&props.to_le_bytes());
        for (key, value) in row {
            // A scalar keeps its scalar bytes and error; a collection is its
            // CanonicalPropertyValue encoding.
            let encoded = match value.as_scalar() {
                Some(scalar) => scalar
                    .encode()
                    .map_err(|error| EdgePropertyPatchError::ScalarEncode { at, error })?,
                None => value
                    .encode()
                    .map_err(|error| EdgePropertyPatchError::PropertyValue { at, error })?,
            };
            out.extend_from_slice(&key.0.to_le_bytes());
            let len = u32::try_from(encoded.len()).expect("value bounds limit its encoding");
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(&encoded);
        }
    }
    Ok(out)
}

/// Decode a patch, independently re-checking every canonical law.
pub fn decode_property_patch(bytes: &[u8]) -> Result<Vec<EdgePropertyRow>, EdgePropertyPatchError> {
    decode_property_patch_inner(bytes, None)
}

/// Decode a patch using explicit pinned artifacts for artifact-bound scalars.
pub fn decode_property_patch_with_resolver(
    bytes: &[u8],
    resolver: &dyn CanonicalScalarResolver,
) -> Result<Vec<EdgePropertyRow>, EdgePropertyPatchError> {
    decode_property_patch_inner(bytes, Some(resolver))
}

fn decode_property_patch_inner(
    bytes: &[u8],
    resolver: Option<&dyn CanonicalScalarResolver>,
) -> Result<Vec<EdgePropertyRow>, EdgePropertyPatchError> {
    decode_property_patch_fields(bytes, resolver, None)
}

// A selection is legal only behind the already-admitted read below. All public
// decoders pass None and independently validate every value and format profile.
// Selected reads still parse every row, including skipped rows' lengths and key
// order, but retain only the requested row rather than a partial patch image.
fn decode_property_patch_fields(
    bytes: &[u8],
    resolver: Option<&dyn CanonicalScalarResolver>,
    selected: Option<usize>,
) -> Result<Vec<EdgePropertyRow>, EdgePropertyPatchError> {
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
    if !matches!(
        format,
        PROPERTY_PATCH_FORMAT_V1 | PROPERTY_PATCH_FORMAT_V2 | PROPERTY_PATCH_FORMAT_V3
    ) {
        return Err(EdgePropertyPatchError::UnsupportedFormat { format });
    }
    let start = take(&mut at, 4)?;
    let declared = u32::from_le_bytes(bytes[start..start + 4].try_into().expect("four bytes"));
    if declared > MAX_PROPERTY_PATCH_ROWS {
        return Err(EdgePropertyPatchError::ImplausibleRowCount { declared });
    }
    let capacity = selected.map_or(declared as usize, |row| {
        usize::from(row < declared as usize)
    });
    let mut rows = Vec::with_capacity(capacity);
    for row_at in 0..declared as usize {
        let start = take(&mut at, 4)?;
        let props = u32::from_le_bytes(bytes[start..start + 4].try_into().expect("four bytes"));
        let decode = selected.is_none_or(|selected| selected == row_at);
        let mut row: EdgePropertyRow = Vec::with_capacity(if decode {
            props.min(MAX_PROPERTY_PATCH_ROWS) as usize
        } else {
            0
        });
        let mut previous_key = None;
        for _ in 0..props {
            let start = take(&mut at, 8)?;
            let key = PropertyKeyId(u64::from_le_bytes(
                bytes[start..start + 8].try_into().expect("eight"),
            ));
            let start = take(&mut at, 4)?;
            let len = u32::from_le_bytes(bytes[start..start + 4].try_into().expect("four bytes"))
                as usize;
            let start = take(&mut at, len)?;
            if !decode {
                if previous_key.is_some_and(|previous| previous >= key.0) {
                    return Err(EdgePropertyPatchError::NonCanonicalRow { at: row_at });
                }
                previous_key = Some(key.0);
                continue;
            }
            let encoded = &bytes[start..start + len];
            let value = if CanonicalPropertyValue::is_collection_encoding(encoded) {
                match resolver {
                    Some(resolver) => {
                        CanonicalPropertyValue::decode_with_resolver(encoded, resolver)
                    }
                    None => CanonicalPropertyValue::decode_stored(encoded),
                }
                .map_err(|error| EdgePropertyPatchError::PropertyValue { at: row_at, error })?
            } else {
                // This is the closed graph-value decoder; no JWT or signature state exists here.
                // ubs:ignore -- exact false match is `CanonicalScalar::decode`, not a JWT decoder.
                match resolver {
                    Some(resolver) => CanonicalScalar::decode_with_resolver(encoded, resolver),
                    None => CanonicalScalar::decode_stored(encoded),
                }
                .map(CanonicalPropertyValue::Scalar)
                .map_err(|error| EdgePropertyPatchError::ScalarDecode { at: row_at, error })?
            };
            row.push((key, value));
        }
        if decode {
            validate_row(row_at, &row)?;
            rows.push(row);
        } else if props == 0 {
            return Err(EdgePropertyPatchError::EmptyRow { at: row_at });
        }
    }
    if at != bytes.len() {
        return Err(EdgePropertyPatchError::TrailingBytes {
            extra: bytes.len() - at,
        });
    }
    // The value decoder admits every tag this build knows under every
    // header, so the header is checked against what the rows hold.
    // An admitted selection cannot infer the complete patch's required format
    // from one row. Initial admission already proved that exact format, and the
    // keyed identity plus current extent checksum bind all skipped payloads.
    if selected.is_none() {
        let required = property_patch_format_for(&rows);
        if format != required {
            return Err(EdgePropertyPatchError::FormatMismatch { format, required });
        }
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
    read_property_patch_inner(k_oid, namespace, bytes, expected, None)
}

/// Verify a patch's identity and decode its scalars against explicit pinned artifacts.
pub fn read_property_patch_with_resolver(
    k_oid: &[u8; 32],
    namespace: DatabaseSecurityNamespaceId,
    bytes: &[u8],
    expected: EdgePropertyPatchVersion,
    resolver: &dyn CanonicalScalarResolver,
) -> Result<Vec<EdgePropertyRow>, EdgePropertyPatchError> {
    read_property_patch_inner(k_oid, namespace, bytes, expected, Some(resolver))
}

pub(crate) fn read_property_patch_inner(
    k_oid: &[u8; 32],
    namespace: DatabaseSecurityNamespaceId,
    bytes: &[u8],
    expected: EdgePropertyPatchVersion,
    resolver: Option<&dyn CanonicalScalarResolver>,
) -> Result<Vec<EdgePropertyRow>, EdgePropertyPatchError> {
    let actual = property_patch_id(k_oid, namespace, bytes);
    if actual != expected.0 {
        return Err(EdgePropertyPatchError::IdentityMismatch {
            expected: expected.0,
            actual,
        });
    }
    decode_property_patch_inner(bytes, resolver)
}

/// Read one positional property row from an already admitted immutable patch.
///
/// The buffered reader's complete initial admission must have validated every
/// value, the exact format profile and the block/patch locator bijection. Its
/// current fault must also match the admitted extent checksum. Recheck the
/// complete keyed identity and every row's framing and property-key order, but
/// decode values only for the requested zero-based row. Unselected rows are
/// never returned with empty properties that could be mistaken for absence.
/// An out-of-range selection returns None only after complete framing checks.
pub(crate) fn read_admitted_property_row(
    k_oid: &[u8; 32],
    namespace: DatabaseSecurityNamespaceId,
    bytes: &[u8],
    expected: EdgePropertyPatchVersion,
    resolver: Option<&dyn CanonicalScalarResolver>,
    row: usize,
) -> Result<Option<EdgePropertyRow>, EdgePropertyPatchError> {
    let actual = property_patch_id(k_oid, namespace, bytes);
    if actual != expected.0 {
        return Err(EdgePropertyPatchError::IdentityMismatch {
            expected: expected.0,
            actual,
        });
    }
    Ok(decode_property_patch_fields(bytes, resolver, Some(row))?.pop())
}

/// The locator column's own canonical law: non-zero locators are exactly
/// `1..=n` in entry order — each patch row referenced once, in the order the
/// entries appear. Returns `n`. The block decoder enforces this WITHOUT the
/// patch in hand, so a block with a scrambled locator column refuses on its
/// own bytes.
pub fn validate_locator_sequence(locators: &[u8]) -> Result<usize, EdgePropertyPatchError> {
    // `next` is wider than the locator byte: a lawful full column is
    // `1..=255`, and the increment after 255 must not wrap or refuse
    // (fgdb-hc04). A 256th non-zero locator cannot inhabit a u8 and is
    // the format ceiling, not a bijection scramble.
    let mut next: u16 = 1;
    for (entry_at, &locator) in locators.iter().enumerate() {
        if locator == 0 {
            continue;
        }
        if u32::from(next) > MAX_PROPERTY_PATCH_ROWS {
            return Err(EdgePropertyPatchError::ImplausibleRowCount {
                declared: u32::from(next),
            });
        }
        if u16::from(locator) != next {
            return Err(EdgePropertyPatchError::LocatorBijectionViolation {
                entry_at,
                expected: u8::try_from(next).expect("next is 1..=255 here"),
                found: locator,
            });
        }
        next += 1;
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

#[cfg(test)]
mod selected_row_tests {
    use super::*;
    use fgdb_types::{
        CanonicalList, CanonicalLocalTime, CanonicalMap, CanonicalMapEntry, CanonicalText,
        CanonicalTime, CanonicalTimestamp, CollationResolver, CollationResolverError,
        NonBinaryTextBinding, TzdbResolver,
    };
    use std::cell::Cell;

    const KEY: [u8; 32] = [0x38; 32];
    const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x97; 32]);

    fn select(bytes: &[u8], row: usize) -> Result<Option<EdgePropertyRow>, EdgePropertyPatchError> {
        read_admitted_property_row(
            &KEY,
            NS,
            bytes,
            EdgePropertyPatchVersion(property_patch_id(&KEY, NS, bytes)),
            None,
            row,
        )
    }

    fn values() -> [CanonicalPropertyValue; 3] {
        [
            CanonicalScalar::ucs_basic_text("selected edge")
                .unwrap()
                .into(),
            CanonicalScalar::Time(
                CanonicalTime::new(
                    CanonicalLocalTime::from_hms_nano(17, 23, 41, 987_654_321).unwrap(),
                    -18_000,
                )
                .unwrap(),
            )
            .into(),
            CanonicalPropertyValue::Map(
                CanonicalMap::try_new(vec![CanonicalMapEntry::new(
                    CanonicalText::new_ucs_basic("nested").unwrap(),
                    CanonicalPropertyValue::List(
                        CanonicalList::try_new(vec![
                            CanonicalScalar::Int(i64::MIN).into(),
                            CanonicalScalar::bytes(vec![0x57; 31]).unwrap().into(),
                        ])
                        .unwrap(),
                    ),
                )])
                .unwrap(),
            ),
        ]
    }

    #[test]
    fn selected_rows_equal_full_decode_for_every_locator_and_format() {
        for (profile, value) in values().into_iter().enumerate() {
            for count in [0, 1, 2, 17, MAX_PROPERTY_PATCH_ROWS as usize] {
                let rows: Vec<_> = (0..count)
                    .map(|at| {
                        vec![
                            (
                                PropertyKeyId(3),
                                CanonicalScalar::Int(at as i64 - 113).into(),
                            ),
                            (PropertyKeyId(u64::MAX), value.clone()),
                        ]
                    })
                    .collect();
                let bytes = encode_property_patch(&rows).unwrap();
                let complete = decode_property_patch(&bytes).unwrap();
                assert_eq!(complete, rows);
                if count != 0 {
                    assert_eq!(property_patch_format_for(&rows), profile as u16 + 1);
                }
                for at in 0..count {
                    assert_eq!(select(&bytes, at).unwrap().as_ref(), rows.get(at));
                }
                for at in [count, count.saturating_add(1), usize::MAX] {
                    assert!(select(&bytes, at).unwrap().is_none());
                }
            }
        }
    }

    #[test]
    fn selected_rows_still_require_complete_framing_and_canonical_keys() {
        let rows = vec![
            vec![(PropertyKeyId(5), CanonicalScalar::Int(-7).into())],
            vec![
                (PropertyKeyId(11), CanonicalScalar::Bool(true).into()),
                (PropertyKeyId(19), CanonicalScalar::Int(29).into()),
            ],
            vec![(PropertyKeyId(31), CanonicalScalar::Null.into())],
        ];
        let bytes = encode_property_patch(&rows).unwrap();
        for selection in [0, 1, 2, 3, usize::MAX] {
            for cut in 0..bytes.len() {
                assert!(
                    matches!(
                        select(&bytes[..cut], selection),
                        Err(EdgePropertyPatchError::Truncated { .. })
                    ),
                    "selection {selection}, strict prefix {cut}"
                );
            }
            let mut extra = bytes.clone();
            extra.push(0);
            assert_eq!(
                select(&extra, selection),
                Err(EdgePropertyPatchError::TrailingBytes { extra: 1 })
            );
        }

        // These bytes are deliberately self-identified malformed inputs to the
        // parser, not admitted patches. They prove framing/key checks remain
        // present even when the selected row is earlier, later, or absent.
        let first_bytes = encode_property_patch(&rows[..1]).unwrap().len() - 10;
        let second = 10 + first_bytes;
        let second_key = second + 4 + 8 + 4 + CanonicalScalar::Bool(true).encode().unwrap().len();
        let mut duplicate = bytes.clone();
        duplicate[second_key..second_key + 8].copy_from_slice(&11_u64.to_le_bytes());
        let mut descending = duplicate.clone();
        descending[second_key..second_key + 8].copy_from_slice(&7_u64.to_le_bytes());
        for selection in [0, 1, 2, usize::MAX] {
            for malformed in [&duplicate, &descending] {
                assert_eq!(
                    select(malformed, selection),
                    Err(EdgePropertyPatchError::NonCanonicalRow { at: 1 })
                );
            }
        }
        let mut empty = bytes[..second].to_vec();
        empty.extend_from_slice(&0_u32.to_le_bytes());
        let third = encode_property_patch(&rows[2..]).unwrap();
        empty.extend_from_slice(&third[10..]);
        for selection in [0, 1, 2, usize::MAX] {
            assert_eq!(
                select(&empty, selection),
                Err(EdgePropertyPatchError::EmptyRow { at: 1 })
            );
        }
    }

    #[test]
    fn a_selected_row_authenticates_unselected_bytes_and_all_identity_coordinates() {
        let rows: Vec<_> = values()
            .into_iter()
            .map(|value| vec![(PropertyKeyId(13), value)])
            .collect();
        let bytes = encode_property_patch(&rows).unwrap();
        let expected = EdgePropertyPatchVersion(property_patch_id(&KEY, NS, &bytes));
        for at in 0..bytes.len() {
            let mut changed = bytes.clone();
            changed[at] ^= 1;
            assert!(matches!(
                read_admitted_property_row(&KEY, NS, &changed, expected, None, 0),
                Err(EdgePropertyPatchError::IdentityMismatch { .. })
            ));
        }
        for (key, namespace) in [
            ([0x39; 32], NS),
            (KEY, DatabaseSecurityNamespaceId([0x98; 32])),
        ] {
            assert!(matches!(
                read_admitted_property_row(&key, namespace, &bytes, expected, None, 0),
                Err(EdgePropertyPatchError::IdentityMismatch { .. })
            ));
        }
        let mut format = bytes.clone();
        format[4..6].copy_from_slice(&99_u16.to_le_bytes());
        assert_eq!(
            select(&format, usize::MAX),
            Err(EdgePropertyPatchError::UnsupportedFormat { format: 99 })
        );
        let mut count = bytes;
        count[6..10].copy_from_slice(&256_u32.to_le_bytes());
        assert_eq!(
            select(&count, usize::MAX),
            Err(EdgePropertyPatchError::ImplausibleRowCount { declared: 256 })
        );
    }

    #[test]
    fn complete_admission_still_checks_every_value_and_the_exact_format_profile() {
        for value in values() {
            let rows = vec![
                vec![(PropertyKeyId(3), CanonicalScalar::Int(17).into())],
                vec![(PropertyKeyId(7), value)],
            ];
            let bytes = encode_property_patch(&rows).unwrap();
            let required = property_patch_format_for(&rows);
            for format in [
                PROPERTY_PATCH_FORMAT_V1,
                PROPERTY_PATCH_FORMAT_V2,
                PROPERTY_PATCH_FORMAT_V3,
            ] {
                if format == required {
                    continue;
                }
                let mut mismatched = bytes.clone();
                mismatched[4..6].copy_from_slice(&format.to_le_bytes());
                let id = EdgePropertyPatchVersion(property_patch_id(&KEY, NS, &mismatched));
                assert_eq!(
                    read_property_patch(&KEY, NS, &mismatched, id),
                    Err(EdgePropertyPatchError::FormatMismatch { format, required })
                );
            }
            // The second row is unrelated to a point selecting row zero, but
            // initial full admission must still reject its invalid value tag.
            let second = encode_property_patch(&rows[..1]).unwrap().len();
            let mut malformed = bytes;
            malformed[second + 4 + 8 + 4] = 0xff;
            let id = EdgePropertyPatchVersion(property_patch_id(&KEY, NS, &malformed));
            assert!(matches!(
                read_property_patch(&KEY, NS, &malformed, id),
                Err(EdgePropertyPatchError::ScalarDecode { at: 1, .. })
            ));
        }
    }

    #[derive(Default)]
    struct Resolver {
        calls: Cell<usize>,
        reject: Cell<bool>,
    }

    impl CollationResolver for Resolver {
        fn artifact_available(&self, _: &ObjectId) -> bool {
            false
        }

        fn canonical_sort_key_len(
            &self,
            _: &NonBinaryTextBinding,
            _: &str,
        ) -> Result<usize, CollationResolverError> {
            Err(CollationResolverError::new(1))
        }

        fn write_canonical_sort_key(
            &self,
            _: &NonBinaryTextBinding,
            _: &str,
            _: &mut [u8],
        ) -> Result<usize, CollationResolverError> {
            Err(CollationResolverError::new(1))
        }

        fn canonical_sort_key_matches(
            &self,
            _: &NonBinaryTextBinding,
            _: &str,
            _: &[u8],
        ) -> Result<bool, CollationResolverError> {
            Err(CollationResolverError::new(1))
        }
    }

    impl TzdbResolver for Resolver {
        fn contains_tzdb(&self, oid: &ObjectId) -> bool {
            self.calls.set(self.calls.get() + 1);
            !self.reject.get() && *oid == ObjectId([9; 32])
        }

        fn canonical_utc_offset_seconds(
            &self,
            oid: &ObjectId,
            zone: &str,
            instant: i128,
        ) -> Option<i32> {
            self.calls.set(self.calls.get() + 1);
            (!self.reject.get()
                && *oid == ObjectId([9; 32])
                && zone == "America/New_York"
                && instant == 1_735_689_600_123_456_789)
                .then_some(-18_000)
        }
    }

    #[test]
    fn selected_values_use_the_resolver_but_unselected_values_are_not_decoded() {
        let resolver = Resolver::default();
        let timestamp = CanonicalScalar::Timestamp(
            CanonicalTimestamp::zoned(
                1_735_689_600_123_456_789,
                -18_000,
                "America/New_York",
                ObjectId([9; 32]),
                &resolver,
            )
            .unwrap(),
        );
        let rows = vec![
            vec![(PropertyKeyId(3), timestamp.clone().into())],
            vec![(PropertyKeyId(7), CanonicalScalar::Int(101).into())],
            vec![(
                PropertyKeyId(11),
                CanonicalPropertyValue::List(
                    CanonicalList::try_new(vec![timestamp.into()]).unwrap(),
                ),
            )],
        ];
        let bytes = encode_property_patch(&rows).unwrap();
        let id = EdgePropertyPatchVersion(property_patch_id(&KEY, NS, &bytes));
        assert_eq!(
            decode_property_patch_with_resolver(&bytes, &resolver).unwrap(),
            rows
        );
        // Stored timestamps can be decoded without supplying artifacts, but
        // an explicitly supplied resolver must still validate selected values.
        assert_eq!(decode_property_patch(&bytes).unwrap(), rows);
        resolver.calls.set(0);
        resolver.reject.set(true);
        for selection in [1, rows.len(), usize::MAX] {
            assert_eq!(
                read_admitted_property_row(&KEY, NS, &bytes, id, None, selection)
                    .unwrap()
                    .as_ref(),
                rows.get(selection)
            );
            assert_eq!(
                read_admitted_property_row(&KEY, NS, &bytes, id, Some(&resolver), selection)
                    .unwrap()
                    .as_ref(),
                rows.get(selection)
            );
        }
        assert_eq!(
            resolver.calls.get(),
            0,
            "unrelated artifacts are not resolved"
        );
        for selection in [0, 2] {
            resolver.reject.set(true);
            assert!(
                read_admitted_property_row(&KEY, NS, &bytes, id, Some(&resolver), selection)
                    .is_err()
            );
            resolver.reject.set(false);
            let before = resolver.calls.get();
            assert_eq!(
                read_admitted_property_row(&KEY, NS, &bytes, id, Some(&resolver), selection)
                    .unwrap()
                    .as_ref(),
                rows.get(selection)
            );
            assert!(resolver.calls.get() > before);
        }
    }
}
