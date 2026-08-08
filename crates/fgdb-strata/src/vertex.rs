//! The Tier-D vertex row patch: the durable form of vertex existence, labels,
//! and properties (`fgdb-3xoi`, the spine increment split out of
//! `fgdb-w3-properties-gou`).
//!
//! Until this module existed, tier D was ADJACENCY ONLY: `CreateVertex` rows
//! were committed, durable, and materialized by the oracle, but the block fold
//! ignored them, so `fgdb::Database::neighbours` was the entire read surface
//! and a written label or property could never be read back. This is the
//! durable object that closes that gap: an immutable, content-addressed patch
//! holding one partition's vertex rows, encoded canonically, refused
//! fail-closed at every boundary the block format refuses at.
//!
//! **THE SAME EXPOSURE NOTE AS `FGSB`/`FGSR` APPLIES.** These bytes are
//! deliberately unregistered: Appendix A's `w3-properties` rows own the
//! normative sealed property layout, and this patch is the Tier-D subset that
//! `fgdb-w3-properties-gou` will absorb — columnar sealed chunks, label
//! membership bitmaps, edge property chunks, and the embedding-matrix contract
//! all stay with that bead. Registering this shape today would freeze a
//! subset as the normative contract, which is the mistake the block format's
//! doc comment documents at length. The object kind below shares the interim
//! numbering caveat of [`crate::DELTA_BLOCK_OBJECT_KIND`].
//!
//! **CANONICAL MEANS EXACTLY ONE BYTE STRING PER VALUE.** Rows are strictly
//! ascending by `VId`; labels are strictly ascending; property keys are
//! strictly ascending; property values are encoded through
//! [`CanonicalScalar::encode`], the single definition of what a value's bytes
//! are. The ENCODER refuses non-canonical input rather than repairing it, and
//! the DECODER independently refuses the same shapes, so a hand-built patch
//! cannot smuggle an order the encoder would never emit.

use fgdb_delta_types::{LabelId, PropertyKeyId};
use fgdb_types::ids::{DatabaseSecurityNamespaceId, ObjectId};
use fgdb_types::{CanonicalScalar, CommitSeq, ScalarDecodeError, ScalarEncodeError, VId};

/// `FGSV` — FrankenGraph Strata Vertex patch.
pub const VERTEX_PATCH_MAGIC: [u8; 4] = *b"FGSV";

/// Format version. Durable formats are versioned from day one (§16.6):
/// additive-minor, breaking-major.
pub const VERTEX_PATCH_FORMAT_V1: u16 = 1;

/// Durable object kind for a Tier-D vertex row patch — part of the §5.1
/// logical-identity header, separate from the payload framing for the same
/// reason as [`crate::DELTA_BLOCK_OBJECT_KIND`], and carrying the same
/// interim-numbering caveat: these kinds are renumbered when `fgdb-ge6a`
/// registers the Strata formats against the live reservation table.
pub const VERTEX_PATCH_OBJECT_KIND: u16 = 0x0302;

/// More rows than this build will materialize from one patch — the same
/// role as `MAX_BLOCK_ENTRIES`: a format ceiling, not a seal policy.
pub const MAX_PATCH_ROWS: u32 = 256;

/// The content identity of one immutable vertex row patch.
///
/// Like `DeltaBlockVersion` and `PartitionRootVersion`, a semantic type
/// boundary around the ordinary §5.1 [`ObjectId`], so a patch identity cannot
/// be passed accidentally where a block or root is expected.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct VertexPatchVersion(pub ObjectId);

/// One versioned vertex row: identity, birth, visibility interval, labels,
/// properties.
///
/// The interval is HALF-OPEN, `[created_at, retired_at)`, exactly like
/// [`crate::AdjacencyEntry`] and for the same reason. `retired_at` is carried
/// by the format even though the current producer folds only creations
/// (deletes are `fgdb-w5-effects-normal-form-819`'s), so landing deletes is a
/// producer change and not a format break.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VertexRow {
    pub vid: VId,
    pub birth_ordinal: u64,
    pub created_at: CommitSeq,
    /// The sequence that retired this row, or `None` while it is live.
    pub retired_at: Option<CommitSeq>,
    /// Strictly ascending label memberships at creation.
    pub labels: Vec<LabelId>,
    /// Strictly ascending by key; values are canonical scalars.
    pub props: Vec<(PropertyKeyId, CanonicalScalar)>,
}

impl VertexRow {
    /// Is this row visible to a reader at `as_of`?
    ///
    /// `created_at <= as_of < retired_at` — the tier's one visibility rule,
    /// shared verbatim with [`crate::AdjacencyEntry::visible_at`].
    pub fn visible_at(&self, as_of: CommitSeq) -> bool {
        self.created_at.0 <= as_of.0 && self.retired_at.is_none_or(|r| as_of.0 < r.0)
    }
}

/// Why a vertex patch could not be encoded, decoded, or trusted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VertexPatchError {
    /// The bytes do not begin with [`VERTEX_PATCH_MAGIC`].
    NotAVertexPatch,
    /// A format version this build does not implement.
    UnsupportedFormat { format: u16 },
    /// The bytes end before the declared rows do.
    Truncated { at: usize },
    /// Bytes remain after the declared rows — a concatenation or damage,
    /// both wrong to read past.
    TrailingBytes { extra: usize },
    /// Rows are not strictly ascending by `VId`.
    NonCanonicalOrder { at: usize },
    /// A row was created at sequence zero, which names the empty stream.
    CreatedAtZero { at: usize },
    /// A row claims to have been retired at or before it was created.
    RetiredBeforeCreated {
        at: usize,
        created_at: CommitSeq,
        retired_at: CommitSeq,
    },
    /// A row's labels are not strictly ascending.
    NonCanonicalLabels { at: usize },
    /// A row's property keys are not strictly ascending.
    NonCanonicalProps { at: usize },
    /// More rows than this build will materialize from one patch.
    ImplausibleRowCount { declared: u32 },
    /// A property value refused canonical encoding.
    ScalarEncode { at: usize, error: ScalarEncodeError },
    /// A property value's bytes refused canonical decoding.
    ScalarDecode { at: usize, error: ScalarDecodeError },
    /// The bytes are well-formed and WRONG — not the patch that was asked
    /// for. Kept distinct exactly as in `BlockError::IdentityMismatch`.
    IdentityMismatch {
        expected: ObjectId,
        actual: ObjectId,
    },
}

impl core::fmt::Display for VertexPatchError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NotAVertexPatch => write!(f, "not a strata vertex patch"),
            Self::UnsupportedFormat { format } => {
                write!(f, "vertex patch format {format} is not implemented")
            }
            Self::Truncated { at } => {
                write!(
                    f,
                    "vertex patch bytes end inside the structure at offset {at}"
                )
            }
            Self::TrailingBytes { extra } => write!(f, "{extra} bytes after the last row"),
            Self::NonCanonicalOrder { at } => {
                write!(
                    f,
                    "rows must be strictly ascending by vid; violated at row {at}"
                )
            }
            Self::CreatedAtZero { at } => {
                write!(f, "row {at} was created at sequence zero")
            }
            Self::RetiredBeforeCreated {
                at,
                created_at,
                retired_at,
            } => write!(
                f,
                "row {at} retired at {retired_at:?} at or before its creation {created_at:?}"
            ),
            Self::NonCanonicalLabels { at } => {
                write!(f, "row {at} labels must be strictly ascending")
            }
            Self::NonCanonicalProps { at } => {
                write!(f, "row {at} property keys must be strictly ascending")
            }
            Self::ImplausibleRowCount { declared } => {
                write!(
                    f,
                    "{declared} rows exceeds the {MAX_PATCH_ROWS}-row format ceiling"
                )
            }
            Self::ScalarEncode { at, error } => {
                write!(f, "row {at} property value refused encoding: {error:?}")
            }
            Self::ScalarDecode { at, error } => {
                write!(f, "row {at} property value refused decoding: {error:?}")
            }
            Self::IdentityMismatch { expected, actual } => {
                write!(f, "bytes are {actual:?}, not the requested {expected:?}")
            }
        }
    }
}

impl core::error::Error for VertexPatchError {}

/// The full canonical-shape check for one row, shared by encode, decode, and
/// the writer's fold-time admission.
pub(crate) fn validate_patch_row(at: usize, row: &VertexRow) -> Result<(), VertexPatchError> {
    if row.created_at.0 == 0 {
        return Err(VertexPatchError::CreatedAtZero { at });
    }
    if let Some(retired_at) = row.retired_at
        && retired_at.0 <= row.created_at.0
    {
        return Err(VertexPatchError::RetiredBeforeCreated {
            at,
            created_at: row.created_at,
            retired_at,
        });
    }
    if row.labels.windows(2).any(|pair| pair[0].0 >= pair[1].0) {
        return Err(VertexPatchError::NonCanonicalLabels { at });
    }
    if row.props.windows(2).any(|pair| pair[0].0.0 >= pair[1].0.0) {
        return Err(VertexPatchError::NonCanonicalProps { at });
    }
    Ok(())
}

/// Encode rows into one canonical patch. Refuses non-canonical input rather
/// than sorting it, for the reason the block encoder gives: a caller handing
/// over a different order is describing a different intent.
pub fn encode_patch(rows: &[VertexRow]) -> Result<Vec<u8>, VertexPatchError> {
    let count = u32::try_from(rows.len())
        .map_err(|_| VertexPatchError::ImplausibleRowCount { declared: u32::MAX })?;
    if count > MAX_PATCH_ROWS {
        return Err(VertexPatchError::ImplausibleRowCount { declared: count });
    }
    let mut out = Vec::new();
    out.extend_from_slice(&VERTEX_PATCH_MAGIC);
    out.extend_from_slice(&VERTEX_PATCH_FORMAT_V1.to_le_bytes());
    out.extend_from_slice(&count.to_le_bytes());
    let mut previous: Option<VId> = None;
    for (at, row) in rows.iter().enumerate() {
        if let Some(previous) = previous
            && previous >= row.vid
        {
            return Err(VertexPatchError::NonCanonicalOrder { at });
        }
        validate_patch_row(at, row)?;
        out.extend_from_slice(&row.vid.0.to_le_bytes());
        out.extend_from_slice(&row.birth_ordinal.to_le_bytes());
        out.extend_from_slice(&row.created_at.0.to_le_bytes());
        out.extend_from_slice(&row.retired_at.map_or(0, |r| r.0).to_le_bytes());
        let labels =
            u32::try_from(row.labels.len()).expect("label count bounded by canonical admission");
        out.extend_from_slice(&labels.to_le_bytes());
        for label in &row.labels {
            out.extend_from_slice(&label.0.to_le_bytes());
        }
        let props =
            u32::try_from(row.props.len()).expect("prop count bounded by canonical admission");
        out.extend_from_slice(&props.to_le_bytes());
        for (key, value) in &row.props {
            let encoded = value
                .encode()
                .map_err(|error| VertexPatchError::ScalarEncode { at, error })?;
            out.extend_from_slice(&key.0.to_le_bytes());
            let len = u32::try_from(encoded.len()).expect("scalar profile bounds its encoding");
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(&encoded);
        }
        previous = Some(row.vid);
    }
    Ok(out)
}

/// A little-endian cursor that refuses to read past the end, so every
/// truncation is a typed refusal at a named offset rather than a panic.
struct Cursor<'bytes> {
    bytes: &'bytes [u8],
    at: usize,
}

impl<'bytes> Cursor<'bytes> {
    fn take<const N: usize>(&mut self) -> Result<[u8; N], VertexPatchError> {
        let end = self
            .at
            .checked_add(N)
            .filter(|&end| end <= self.bytes.len())
            .ok_or(VertexPatchError::Truncated { at: self.at })?;
        let mut out = [0u8; N];
        out.copy_from_slice(&self.bytes[self.at..end]);
        self.at = end;
        Ok(out)
    }

    fn take_slice(&mut self, len: usize) -> Result<&'bytes [u8], VertexPatchError> {
        let end = self
            .at
            .checked_add(len)
            .filter(|&end| end <= self.bytes.len())
            .ok_or(VertexPatchError::Truncated { at: self.at })?;
        let out = &self.bytes[self.at..end];
        self.at = end;
        Ok(out)
    }

    fn u16(&mut self) -> Result<u16, VertexPatchError> {
        self.take().map(u16::from_le_bytes)
    }

    fn u32(&mut self) -> Result<u32, VertexPatchError> {
        self.take().map(u32::from_le_bytes)
    }

    fn u64(&mut self) -> Result<u64, VertexPatchError> {
        self.take().map(u64::from_le_bytes)
    }

    fn u128(&mut self) -> Result<u128, VertexPatchError> {
        self.take().map(u128::from_le_bytes)
    }
}

/// Decode a patch, independently re-checking every canonical law the encoder
/// enforces.
pub fn decode_patch(bytes: &[u8]) -> Result<Vec<VertexRow>, VertexPatchError> {
    let mut cursor = Cursor { bytes, at: 0 };
    if cursor.take::<4>()? != VERTEX_PATCH_MAGIC {
        return Err(VertexPatchError::NotAVertexPatch);
    }
    let format = cursor.u16()?;
    if format != VERTEX_PATCH_FORMAT_V1 {
        return Err(VertexPatchError::UnsupportedFormat { format });
    }
    let declared = cursor.u32()?;
    if declared > MAX_PATCH_ROWS {
        return Err(VertexPatchError::ImplausibleRowCount { declared });
    }
    let mut rows = Vec::with_capacity(declared as usize);
    let mut previous: Option<VId> = None;
    for at in 0..declared as usize {
        let vid = VId(cursor.u128()?);
        if let Some(previous) = previous
            && previous >= vid
        {
            return Err(VertexPatchError::NonCanonicalOrder { at });
        }
        let birth_ordinal = cursor.u64()?;
        let created_at = CommitSeq(cursor.u64()?);
        let retired_raw = cursor.u64()?;
        let retired_at = (retired_raw != 0).then_some(CommitSeq(retired_raw));
        let label_count = cursor.u32()?;
        let mut labels = Vec::with_capacity(label_count.min(MAX_PATCH_ROWS) as usize);
        for _ in 0..label_count {
            labels.push(LabelId(cursor.u64()?));
        }
        let prop_count = cursor.u32()?;
        let mut props = Vec::with_capacity(prop_count.min(MAX_PATCH_ROWS) as usize);
        for _ in 0..prop_count {
            let key = PropertyKeyId(cursor.u64()?);
            let len = cursor.u32()? as usize;
            let encoded = cursor.take_slice(len)?;
            // This is the closed graph-value decoder; no JWT or signature state exists here.
            // ubs:ignore -- exact false match is `CanonicalScalar::decode`, not a JWT decoder.
            let value = CanonicalScalar::decode(encoded)
                .map_err(|error| VertexPatchError::ScalarDecode { at, error })?;
            props.push((key, value));
        }
        let row = VertexRow {
            vid,
            birth_ordinal,
            created_at,
            retired_at,
            labels,
            props,
        };
        validate_patch_row(at, &row)?;
        rows.push(row);
        previous = Some(vid);
    }
    if cursor.at != bytes.len() {
        return Err(VertexPatchError::TrailingBytes {
            extra: bytes.len() - cursor.at,
        });
    }
    Ok(rows)
}

/// The §5.1 logical object identity of a patch's canonical bytes, namespaced
/// by [`VERTEX_PATCH_OBJECT_KIND`] exactly as [`crate::block_id`] namespaces
/// blocks.
pub fn vertex_patch_id(
    k_oid: &[u8; 32],
    namespace: DatabaseSecurityNamespaceId,
    bytes: &[u8],
) -> ObjectId {
    ObjectId(
        fgdb_crypto::logical_object_id(
            k_oid,
            &namespace.0,
            &VERTEX_PATCH_OBJECT_KIND.to_le_bytes(),
            bytes,
        )
        .0,
    )
}

/// The sequence range a patch's rows mention — the vertex counterpart of
/// [`crate::root::span_of`], with the identical low/high rule so a root's
/// claimed range means the same thing for both object families.
pub fn span_of_rows(rows: &[VertexRow]) -> Option<(CommitSeq, CommitSeq)> {
    let mut low = u64::MAX;
    let mut high = 0u64;
    for row in rows {
        low = low.min(row.created_at.0);
        high = high.max(row.created_at.0);
        if let Some(retired) = row.retired_at {
            high = high.max(retired.0);
        }
    }
    (!rows.is_empty()).then_some((CommitSeq(low), CommitSeq(high)))
}

/// Merge the vertex patches of a partition and answer one vertex at one
/// sequence — the vertex counterpart of [`crate::root::merge_neighbours`].
///
/// The cross-patch model is the same last-statement-wins supersede as blocks:
/// patches arrive in publication order, a later patch may restate a row to
/// add its retirement, and the later statement is the truth. The winning row
/// is then filtered by the tier's one visibility rule.
pub fn merge_vertex(
    patches: &[Vec<VertexRow>],
    vid: VId,
    as_of: CommitSeq,
) -> Option<VertexRow> {
    let mut winner: Option<&VertexRow> = None;
    for rows in patches {
        for row in rows {
            if row.vid == vid {
                winner = Some(row);
            }
        }
    }
    winner.filter(|row| row.visible_at(as_of)).cloned()
}

/// Decode a patch that must be the one named by `expected`.
pub fn read_patch(
    k_oid: &[u8; 32],
    namespace: DatabaseSecurityNamespaceId,
    bytes: &[u8],
    expected: VertexPatchVersion,
) -> Result<Vec<VertexRow>, VertexPatchError> {
    let actual = vertex_patch_id(k_oid, namespace, bytes);
    if actual != expected.0 {
        return Err(VertexPatchError::IdentityMismatch {
            expected: expected.0,
            actual,
        });
    }
    decode_patch(bytes)
}
