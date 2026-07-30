//! The canonical byte encoding of the logical delta schema.
//!
//! This is a **durable format**, so it is versioned from day one (§16.6) and
//! every reader rejects what it does not understand rather than skipping it.
//!
//! WHY THE ENCODING LIVES HERE AND THE DIGEST DOES NOT. A template is
//! identified by the digest of these bytes, and that digest is what the commit
//! marker and the effect capsule cross-check. But hashing needs `fgdb-crypto`,
//! which sits at a higher foundation position than this crate, so the
//! dependency would run backwards. The split is the better design anyway: the
//! schema owns the answer to *what are my canonical bytes*, and the commit
//! layer owns *hash them*. One definition of the bytes, one place that can
//! disagree with it — none.
//!
//! CANONICAL MEANS EXACTLY ONE ENCODING PER VALUE. That is not tidiness; it is
//! doctrine 4. Two apply paths that produce the same effects in different
//! orders must produce the same template, or the same commit would get two
//! identities and every digest cross-check downstream would be comparing
//! coincidences. So ordering is part of the format and [`validate`] enforces
//! it: coordinate entries strictly ascending by `(graph, branch, relation)`,
//! rows strictly ascending by their own encoded bytes, and embedded set/map
//! fields strictly ascending by their durable identity key. *Strictly*, because
//! a repeat is the plan's duplicate invalidity — a normal form has already
//! grouped by coordinate and logical identity, so a second occurrence means
//! the input never went through one.
//!
//! Sorting rows by their encoded bytes rather than a hand-written key is
//! deliberate. A per-arm comparison key is a second definition of row identity
//! that can drift from the encoding; comparing the encodings cannot, and it is
//! total over every arm including ones added later.

use crate::{
    CoordinateEntry, DeltaRow, ElementId, EscrowDomainId, LabelId, LogicalDeltaTemplate,
    OperationKey, PropertyKeyId, RelationId, SchemaEpoch, ValidTimePeriod,
};
use fgdb_types::{BranchId, CanonicalScalar, EId, GraphId, ObjectId, VId};

/// Format version of the canonical delta encoding.
///
/// Present in the bytes because a durable format that cannot say which version
/// it is has no migration path: additive-minor and breaking-major (§16.6) both
/// need a reader that can tell what it is holding.
pub const DELTA_FORMAT_V1: u16 = 1;

/// Arm tags. Stable across versions — a tag is a durable value, so these are
/// assigned once and never renumbered. A gap is preferable to a reuse.
mod tag {
    pub const CREATE_VERTEX: u8 = 0x01;
    pub const CREATE_EDGE: u8 = 0x02;
    pub const DELETE_VERTEX: u8 = 0x03;
    pub const DELETE_EDGE: u8 = 0x04;
    pub const LABEL_MEMBERSHIP: u8 = 0x05;
    pub const PROPERTY: u8 = 0x06;
    pub const VALID_TIME: u8 = 0x07;
    pub const COUNTER: u8 = 0x08;
    pub const ESCROW: u8 = 0x09;
    pub const SKETCH: u8 = 0x0a;
    pub const SCHEMA: u8 = 0x0b;
    pub const CONSTRAINT: u8 = 0x0c;

    pub const ELEMENT_VERTEX: u8 = 0x01;
    pub const ELEMENT_EDGE: u8 = 0x02;

    pub const ABSENT: u8 = 0x00;
    pub const PRESENT: u8 = 0x01;
}

/// Why an encode, decode, or validation failed.
///
/// Every arm names something a reader can act on. "Malformed" as a single
/// bucket would be cheaper to write and useless to the operator holding a
/// database that will not open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CanonicalError {
    /// A scalar value could not be encoded (it exceeded a profile bound).
    Scalar,
    /// The input ended inside a value.
    Truncated,
    /// A tag this reader does not define. Never skipped: a reader that does
    /// not know an arm has not understood the record, and pretending otherwise
    /// is how a durable format silently loses data.
    UnknownTag { tag: u8 },
    /// A format version this reader does not implement.
    UnsupportedFormat { format: u16 },
    /// Bytes remained after a complete value was decoded.
    TrailingBytes { remaining: usize },
    /// A count field declared more elements than the input can hold. Checked
    /// before allocating, so a corrupt length cannot ask for gigabytes.
    ImplausibleCount { declared: usize, remaining: usize },
    /// A collection had more elements than the format's 32-bit count can name.
    CollectionTooLarge { len: usize },
    /// Coordinate entries are not strictly ascending by `(graph, branch,
    /// relation)`, or a coordinate repeats.
    NonCanonicalCoordinateOrder { index: usize },
    /// Rows within a coordinate entry are not strictly ascending by their
    /// canonical bytes, or a row repeats.
    NonCanonicalRowOrder { entry: usize, index: usize },
    /// A `CreateVertex.labels` set is not strictly ascending, or repeats a
    /// label.
    NonCanonicalLabelOrder { index: usize },
    /// A create row's property map is not strictly ascending by property key,
    /// or repeats a key.
    NonCanonicalPropertyOrder { index: usize },
    /// A delete-vertex cascade image is not strictly ascending by edge id, or
    /// repeats an edge.
    NonCanonicalRetiredEdgeOrder { index: usize },
}

impl core::fmt::Display for CanonicalError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Scalar => write!(f, "a scalar value exceeded its profile bound"),
            Self::CollectionTooLarge { len } => {
                write!(f, "a collection of {len} elements exceeds the 32-bit count")
            }
            Self::Truncated => write!(f, "input ended inside a value"),
            Self::UnknownTag { tag } => write!(f, "unknown arm tag {tag:#04x}"),
            Self::UnsupportedFormat { format } => {
                write!(f, "unsupported delta format version {format}")
            }
            Self::TrailingBytes { remaining } => {
                write!(f, "{remaining} trailing bytes after a complete value")
            }
            Self::ImplausibleCount {
                declared,
                remaining,
            } => write!(
                f,
                "declared {declared} elements with only {remaining} bytes remaining"
            ),
            Self::NonCanonicalCoordinateOrder { index } => write!(
                f,
                "coordinate entry {index} is not strictly after its predecessor"
            ),
            Self::NonCanonicalRowOrder { entry, index } => write!(
                f,
                "row {index} of coordinate entry {entry} is not strictly after its predecessor"
            ),
            Self::NonCanonicalLabelOrder { index } => write!(
                f,
                "embedded label {index} is not strictly after its predecessor"
            ),
            Self::NonCanonicalPropertyOrder { index } => write!(
                f,
                "embedded property {index} is not strictly after its predecessor key"
            ),
            Self::NonCanonicalRetiredEdgeOrder { index } => write!(
                f,
                "retired incident edge {index} is not strictly after its predecessor"
            ),
        }
    }
}

impl core::error::Error for CanonicalError {}

// ---------------------------------------------------------------------------
// Primitives
// ---------------------------------------------------------------------------

/// Append-only canonical writer. Fixed-width big-endian throughout: a delta
/// row is small, so a varint would trade a handful of bytes for a second way
/// to encode the same number — which is exactly what "canonical" forbids.
#[derive(Debug, Default)]
struct Writer {
    out: Vec<u8>,
}

impl Writer {
    fn new() -> Self {
        Self::default()
    }

    fn u8(&mut self, value: u8) {
        self.out.push(value);
    }

    fn u16(&mut self, value: u16) {
        self.out.extend_from_slice(&value.to_be_bytes());
    }

    fn u32(&mut self, value: u32) {
        self.out.extend_from_slice(&value.to_be_bytes());
    }

    fn u64(&mut self, value: u64) {
        self.out.extend_from_slice(&value.to_be_bytes());
    }

    fn u128(&mut self, value: u128) {
        self.out.extend_from_slice(&value.to_be_bytes());
    }

    fn i64(&mut self, value: i64) {
        self.out.extend_from_slice(&value.to_be_bytes());
    }

    fn i128(&mut self, value: i128) {
        self.out.extend_from_slice(&value.to_be_bytes());
    }

    fn bytes32(&mut self, value: &[u8; 32]) {
        self.out.extend_from_slice(value);
    }

    fn count(&mut self, len: usize) -> Result<(), CanonicalError> {
        let value = u32::try_from(len).map_err(|_| CanonicalError::CollectionTooLarge { len })?;
        self.u32(value);
        Ok(())
    }

    fn oid(&mut self, value: ObjectId) {
        self.bytes32(&value.0);
    }

    fn optional_i64(&mut self, value: Option<i64>) {
        match value {
            None => self.u8(tag::ABSENT),
            Some(inner) => {
                self.u8(tag::PRESENT);
                self.i64(inner);
            }
        }
    }

    fn optional_oid(&mut self, value: Option<ObjectId>) {
        match value {
            None => self.u8(tag::ABSENT),
            Some(inner) => {
                self.u8(tag::PRESENT);
                self.oid(inner);
            }
        }
    }

    fn valid_time(&mut self, period: ValidTimePeriod) {
        self.i64(period.start_micros);
        self.optional_i64(period.end_micros);
    }

    fn optional_valid_time(&mut self, period: Option<ValidTimePeriod>) {
        match period {
            None => self.u8(tag::ABSENT),
            Some(inner) => {
                self.u8(tag::PRESENT);
                self.valid_time(inner);
            }
        }
    }

    fn element(&mut self, elem: ElementId) {
        match elem {
            ElementId::Vertex(vid) => {
                self.u8(tag::ELEMENT_VERTEX);
                self.u128(vid.0);
            }
            ElementId::Edge(eid) => {
                self.u8(tag::ELEMENT_EDGE);
                self.u128(eid.0);
            }
        }
    }

    /// A scalar is written as its own order-preserving encoding, length
    /// prefixed. Reusing `CanonicalScalar::encode` rather than re-deriving a
    /// value encoding here means there is exactly one definition of what a
    /// property value's bytes are.
    fn scalar(&mut self, value: &CanonicalScalar) -> Result<(), CanonicalError> {
        let encoded = value.encode().map_err(|_| CanonicalError::Scalar)?;
        self.count(encoded.len())?;
        self.out.extend_from_slice(&encoded);
        Ok(())
    }

    fn optional_scalar(&mut self, value: Option<&CanonicalScalar>) -> Result<(), CanonicalError> {
        match value {
            None => {
                self.u8(tag::ABSENT);
                Ok(())
            }
            Some(inner) => {
                self.u8(tag::PRESENT);
                self.scalar(inner)
            }
        }
    }

    fn props(&mut self, props: &[(PropertyKeyId, CanonicalScalar)]) -> Result<(), CanonicalError> {
        self.count(props.len())?;
        for (key, value) in props {
            self.u64(key.0);
            self.scalar(value)?;
        }
        Ok(())
    }

    fn finish(self) -> Vec<u8> {
        self.out
    }
}

/// Bounds-checked canonical reader. Every accessor returns `Result`, so a
/// truncated input can never produce a partially-populated row.
#[derive(Debug)]
struct Reader<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    fn remaining(&self) -> usize {
        self.bytes.len() - self.position
    }

    fn is_exhausted(&self) -> bool {
        self.position == self.bytes.len()
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], CanonicalError> {
        let end = self
            .position
            .checked_add(len)
            .ok_or(CanonicalError::Truncated)?;
        let slice = self
            .bytes
            .get(self.position..end)
            .ok_or(CanonicalError::Truncated)?;
        self.position = end;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8, CanonicalError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, CanonicalError> {
        let slice = self.take(2)?;
        Ok(u16::from_be_bytes([slice[0], slice[1]]))
    }

    fn u32(&mut self) -> Result<u32, CanonicalError> {
        let slice = self.take(4)?;
        Ok(u32::from_be_bytes([slice[0], slice[1], slice[2], slice[3]]))
    }

    fn u64(&mut self) -> Result<u64, CanonicalError> {
        let mut value = [0u8; 8];
        value.copy_from_slice(self.take(8)?);
        Ok(u64::from_be_bytes(value))
    }

    fn u128(&mut self) -> Result<u128, CanonicalError> {
        let mut value = [0u8; 16];
        value.copy_from_slice(self.take(16)?);
        Ok(u128::from_be_bytes(value))
    }

    fn i64(&mut self) -> Result<i64, CanonicalError> {
        let mut value = [0u8; 8];
        value.copy_from_slice(self.take(8)?);
        Ok(i64::from_be_bytes(value))
    }

    fn i128(&mut self) -> Result<i128, CanonicalError> {
        let mut value = [0u8; 16];
        value.copy_from_slice(self.take(16)?);
        Ok(i128::from_be_bytes(value))
    }

    fn bytes32(&mut self) -> Result<[u8; 32], CanonicalError> {
        let mut value = [0u8; 32];
        value.copy_from_slice(self.take(32)?);
        Ok(value)
    }

    fn oid(&mut self) -> Result<ObjectId, CanonicalError> {
        Ok(ObjectId(self.bytes32()?))
    }

    /// A collection count, checked against what the input can actually hold
    /// BEFORE any allocation. A corrupt length must not be able to ask for a
    /// gigabyte of `Vec` capacity on the way to failing.
    fn count(&mut self, min_element_bytes: usize) -> Result<usize, CanonicalError> {
        let declared = self.u32()? as usize;
        let floor = declared.saturating_mul(min_element_bytes.max(1));
        if floor > self.remaining() {
            return Err(CanonicalError::ImplausibleCount {
                declared,
                remaining: self.remaining(),
            });
        }
        Ok(declared)
    }

    fn presence(&mut self) -> Result<bool, CanonicalError> {
        match self.u8()? {
            tag::ABSENT => Ok(false),
            tag::PRESENT => Ok(true),
            tag => Err(CanonicalError::UnknownTag { tag }),
        }
    }

    fn optional_i64(&mut self) -> Result<Option<i64>, CanonicalError> {
        if self.presence()? {
            Ok(Some(self.i64()?))
        } else {
            Ok(None)
        }
    }

    fn optional_oid(&mut self) -> Result<Option<ObjectId>, CanonicalError> {
        if self.presence()? {
            Ok(Some(self.oid()?))
        } else {
            Ok(None)
        }
    }

    fn valid_time(&mut self) -> Result<ValidTimePeriod, CanonicalError> {
        Ok(ValidTimePeriod {
            start_micros: self.i64()?,
            end_micros: self.optional_i64()?,
        })
    }

    fn optional_valid_time(&mut self) -> Result<Option<ValidTimePeriod>, CanonicalError> {
        if self.presence()? {
            Ok(Some(self.valid_time()?))
        } else {
            Ok(None)
        }
    }

    fn element(&mut self) -> Result<ElementId, CanonicalError> {
        match self.u8()? {
            tag::ELEMENT_VERTEX => Ok(ElementId::Vertex(VId(self.u128()?))),
            tag::ELEMENT_EDGE => Ok(ElementId::Edge(EId(self.u128()?))),
            tag => Err(CanonicalError::UnknownTag { tag }),
        }
    }

    fn scalar(&mut self) -> Result<CanonicalScalar, CanonicalError> {
        let len = self.count(1)?;
        let encoded = self.take(len)?;
        // This is the closed graph-value decoder; no JWT or signature state exists here.
        // ubs:ignore -- exact false match is `CanonicalScalar::decode`, not a JWT decoder.
        CanonicalScalar::decode(encoded).map_err(|_| CanonicalError::Scalar)
    }

    fn optional_scalar(&mut self) -> Result<Option<CanonicalScalar>, CanonicalError> {
        if self.presence()? {
            Ok(Some(self.scalar()?))
        } else {
            Ok(None)
        }
    }

    fn props(&mut self) -> Result<Vec<(PropertyKeyId, CanonicalScalar)>, CanonicalError> {
        // 8 bytes of key + 4 of length is the floor for one property.
        let count = self.count(12)?;
        let mut props = Vec::with_capacity(count);
        for _ in 0..count {
            let key = PropertyKeyId(self.u64()?);
            props.push((key, self.scalar()?));
        }
        Ok(props)
    }
}

// ---------------------------------------------------------------------------
// Rows
// ---------------------------------------------------------------------------

fn first_non_strict_index<T: Ord>(values: &[T]) -> Option<usize> {
    values
        .windows(2)
        .position(|pair| pair[0] >= pair[1])
        .map(|index| index + 1)
}

impl DeltaRow {
    /// Normalize set/map fields produced by a builder.
    ///
    /// Duplicates deliberately remain adjacent for validation to reject; this
    /// method never invents the semantic policy of collapsing two effects.
    fn canonicalize_embedded_collections(&mut self) {
        match self {
            DeltaRow::CreateVertex { labels, props, .. } => {
                labels.sort_unstable();
                props.sort_by_key(|(key, _)| *key);
            }
            DeltaRow::CreateEdge { props, .. } => {
                props.sort_by_key(|(key, _)| *key);
            }
            DeltaRow::DeleteVertex {
                sorted_retired_incident_edges,
                ..
            } => sorted_retired_incident_edges.sort_unstable(),
            _ => {}
        }
    }

    /// Enforce the one-encoding law for every embedded set/map.
    fn validate_embedded_collections(&self) -> Result<(), CanonicalError> {
        match self {
            DeltaRow::CreateVertex { labels, props, .. } => {
                if let Some(index) = first_non_strict_index(labels) {
                    return Err(CanonicalError::NonCanonicalLabelOrder { index });
                }
                if let Some(index) = props
                    .windows(2)
                    .position(|pair| pair[0].0 >= pair[1].0)
                    .map(|index| index + 1)
                {
                    return Err(CanonicalError::NonCanonicalPropertyOrder { index });
                }
            }
            DeltaRow::CreateEdge { props, .. } => {
                if let Some(index) = props
                    .windows(2)
                    .position(|pair| pair[0].0 >= pair[1].0)
                    .map(|index| index + 1)
                {
                    return Err(CanonicalError::NonCanonicalPropertyOrder { index });
                }
            }
            DeltaRow::DeleteVertex {
                sorted_retired_incident_edges,
                ..
            } => {
                if let Some(index) = first_non_strict_index(sorted_retired_incident_edges) {
                    return Err(CanonicalError::NonCanonicalRetiredEdgeOrder { index });
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// This row's canonical bytes.
    ///
    /// Also its sort key: [`validate`] orders rows by these bytes, so the
    /// encoding and the ordering can never disagree about row identity.
    pub fn canonical_bytes(&self) -> Result<Vec<u8>, CanonicalError> {
        self.validate_embedded_collections()?;
        let mut w = Writer::new();
        match self {
            DeltaRow::CreateVertex {
                vid,
                birth_ordinal,
                labels,
                props,
                valid_time,
            } => {
                w.u8(tag::CREATE_VERTEX);
                w.u128(vid.0);
                w.u64(*birth_ordinal);
                w.count(labels.len())?;
                for label in labels {
                    w.u64(label.0);
                }
                w.props(props)?;
                w.optional_valid_time(*valid_time);
            }
            DeltaRow::CreateEdge {
                eid,
                birth_ordinal,
                src,
                relation,
                dst,
                canonical_key,
                props,
                valid_time,
            } => {
                w.u8(tag::CREATE_EDGE);
                w.u128(eid.0);
                w.u64(*birth_ordinal);
                w.u128(src.0);
                w.u64(relation.0);
                w.u128(dst.0);
                w.optional_scalar(canonical_key.as_ref())?;
                w.props(props)?;
                w.optional_valid_time(*valid_time);
            }
            DeltaRow::DeleteVertex {
                vid,
                before_version,
                sorted_retired_incident_edges,
            } => {
                w.u8(tag::DELETE_VERTEX);
                w.u128(vid.0);
                w.oid(*before_version);
                w.count(sorted_retired_incident_edges.len())?;
                for edge in sorted_retired_incident_edges {
                    w.u128(edge.0);
                }
            }
            DeltaRow::DeleteEdge {
                eid,
                before_version,
            } => {
                w.u8(tag::DELETE_EDGE);
                w.u128(eid.0);
                w.oid(*before_version);
            }
            DeltaRow::LabelMembership {
                vid,
                label,
                before,
                after,
            } => {
                w.u8(tag::LABEL_MEMBERSHIP);
                w.u128(vid.0);
                w.u64(label.0);
                w.u8(u8::from(*before));
                w.u8(u8::from(*after));
            }
            DeltaRow::Property {
                elem,
                property,
                before,
                after,
            } => {
                w.u8(tag::PROPERTY);
                w.element(*elem);
                w.u64(property.0);
                w.optional_scalar(before.as_ref())?;
                w.optional_scalar(after.as_ref())?;
            }
            DeltaRow::ValidTime {
                elem,
                contract_id,
                before,
                after,
            } => {
                w.u8(tag::VALID_TIME);
                w.element(*elem);
                w.oid(*contract_id);
                w.optional_valid_time(*before);
                w.optional_valid_time(*after);
            }
            DeltaRow::Counter {
                operation_key,
                elem,
                property,
                algebra_profile,
                delta,
                before,
                after,
            } => {
                w.u8(tag::COUNTER);
                w.bytes32(&operation_key.0);
                w.element(*elem);
                w.u64(property.0);
                w.oid(*algebra_profile);
                w.i128(*delta);
                w.i128(*before);
                w.i128(*after);
            }
            DeltaRow::Escrow {
                domain_id,
                epoch,
                operation_key,
                subject,
                subject_property,
                delta,
                before_value,
                after_value,
            } => {
                w.u8(tag::ESCROW);
                w.u128(domain_id.0);
                w.u64(*epoch);
                w.bytes32(&operation_key.0);
                w.element(*subject);
                match subject_property {
                    None => w.u8(tag::ABSENT),
                    Some(key) => {
                        w.u8(tag::PRESENT);
                        w.u64(key.0);
                    }
                }
                w.i128(*delta);
                w.i128(*before_value);
                w.i128(*after_value);
            }
            DeltaRow::Sketch {
                operation_key,
                sketch_profile_oid,
                before_state_digest,
                after_state_oid,
            } => {
                w.u8(tag::SKETCH);
                w.bytes32(&operation_key.0);
                w.oid(*sketch_profile_oid);
                w.bytes32(before_state_digest);
                w.oid(*after_state_oid);
            }
            DeltaRow::Schema {
                transition_oid,
                before_epoch,
                after_epoch,
            } => {
                w.u8(tag::SCHEMA);
                w.oid(*transition_oid);
                w.u64(before_epoch.0);
                w.u64(after_epoch.0);
            }
            DeltaRow::Constraint {
                before_schema_root,
                after_schema_root,
                before_constraint_root,
                after_constraint_root,
            } => {
                w.u8(tag::CONSTRAINT);
                w.oid(*before_schema_root);
                w.oid(*after_schema_root);
                w.oid(*before_constraint_root);
                w.oid(*after_constraint_root);
            }
        }
        Ok(w.finish())
    }

    /// Decode one row, requiring the input to be exactly one row.
    pub fn decode_canonical(bytes: &[u8]) -> Result<Self, CanonicalError> {
        let mut r = Reader::new(bytes);
        let row = Self::read(&mut r)?;
        if !r.is_exhausted() {
            return Err(CanonicalError::TrailingBytes {
                remaining: r.remaining(),
            });
        }
        row.validate_embedded_collections()?;
        Ok(row)
    }

    fn read(r: &mut Reader<'_>) -> Result<Self, CanonicalError> {
        Ok(match r.u8()? {
            tag::CREATE_VERTEX => {
                let vid = VId(r.u128()?);
                let birth_ordinal = r.u64()?;
                let label_count = r.count(8)?;
                let mut labels = Vec::with_capacity(label_count);
                for _ in 0..label_count {
                    labels.push(LabelId(r.u64()?));
                }
                DeltaRow::CreateVertex {
                    vid,
                    birth_ordinal,
                    labels,
                    props: r.props()?,
                    valid_time: r.optional_valid_time()?,
                }
            }
            tag::CREATE_EDGE => DeltaRow::CreateEdge {
                eid: EId(r.u128()?),
                birth_ordinal: r.u64()?,
                src: VId(r.u128()?),
                relation: RelationId(r.u64()?),
                dst: VId(r.u128()?),
                canonical_key: r.optional_scalar()?,
                props: r.props()?,
                valid_time: r.optional_valid_time()?,
            },
            tag::DELETE_VERTEX => {
                let vid = VId(r.u128()?);
                let before_version = r.oid()?;
                let edge_count = r.count(16)?;
                let mut sorted_retired_incident_edges = Vec::with_capacity(edge_count);
                for _ in 0..edge_count {
                    sorted_retired_incident_edges.push(EId(r.u128()?));
                }
                DeltaRow::DeleteVertex {
                    vid,
                    before_version,
                    sorted_retired_incident_edges,
                }
            }
            tag::DELETE_EDGE => DeltaRow::DeleteEdge {
                eid: EId(r.u128()?),
                before_version: r.oid()?,
            },
            tag::LABEL_MEMBERSHIP => DeltaRow::LabelMembership {
                vid: VId(r.u128()?),
                label: LabelId(r.u64()?),
                before: read_bool(r)?,
                after: read_bool(r)?,
            },
            tag::PROPERTY => DeltaRow::Property {
                elem: r.element()?,
                property: PropertyKeyId(r.u64()?),
                before: r.optional_scalar()?,
                after: r.optional_scalar()?,
            },
            tag::VALID_TIME => DeltaRow::ValidTime {
                elem: r.element()?,
                contract_id: r.oid()?,
                before: r.optional_valid_time()?,
                after: r.optional_valid_time()?,
            },
            tag::COUNTER => DeltaRow::Counter {
                operation_key: OperationKey(r.bytes32()?),
                elem: r.element()?,
                property: PropertyKeyId(r.u64()?),
                algebra_profile: r.oid()?,
                delta: r.i128()?,
                before: r.i128()?,
                after: r.i128()?,
            },
            tag::ESCROW => DeltaRow::Escrow {
                domain_id: EscrowDomainId(r.u128()?),
                epoch: r.u64()?,
                operation_key: OperationKey(r.bytes32()?),
                subject: r.element()?,
                subject_property: if r.presence()? {
                    Some(PropertyKeyId(r.u64()?))
                } else {
                    None
                },
                delta: r.i128()?,
                before_value: r.i128()?,
                after_value: r.i128()?,
            },
            tag::SKETCH => DeltaRow::Sketch {
                operation_key: OperationKey(r.bytes32()?),
                sketch_profile_oid: r.oid()?,
                before_state_digest: r.bytes32()?,
                after_state_oid: r.oid()?,
            },
            tag::SCHEMA => DeltaRow::Schema {
                transition_oid: r.oid()?,
                before_epoch: SchemaEpoch(r.u64()?),
                after_epoch: SchemaEpoch(r.u64()?),
            },
            tag::CONSTRAINT => DeltaRow::Constraint {
                before_schema_root: r.oid()?,
                after_schema_root: r.oid()?,
                before_constraint_root: r.oid()?,
                after_constraint_root: r.oid()?,
            },
            tag => return Err(CanonicalError::UnknownTag { tag }),
        })
    }
}

/// A boolean is one byte and only two byte values are legal. Accepting any
/// nonzero as `true` would give `true` 255 encodings — the opposite of
/// canonical, and a way for two byte streams to mean the same template.
fn read_bool(r: &mut Reader<'_>) -> Result<bool, CanonicalError> {
    match r.u8()? {
        0 => Ok(false),
        1 => Ok(true),
        tag => Err(CanonicalError::UnknownTag { tag }),
    }
}

// ---------------------------------------------------------------------------
// Coordinate entries and templates
// ---------------------------------------------------------------------------

impl CoordinateEntry {
    /// The `(graph, branch, relation)` triple this entry is keyed by. Two
    /// entries sharing it are the plan's "duplicate coordinates/relations".
    pub fn coordinate(&self) -> (GraphId, BranchId, RelationId) {
        (self.graph, self.branch, self.relation)
    }

    fn write(&self, w: &mut Writer) -> Result<(), CanonicalError> {
        w.u128(self.graph.0);
        w.u128(self.branch.0);
        w.u64(self.relation.0);
        w.u64(self.schema_epoch.0);
        w.optional_oid(self.schema_transition);
        w.count(self.rows.len())?;
        for row in &self.rows {
            let encoded = row.canonical_bytes()?;
            w.count(encoded.len())?;
            w.out.extend_from_slice(&encoded);
        }
        Ok(())
    }

    fn read(r: &mut Reader<'_>) -> Result<Self, CanonicalError> {
        let graph = GraphId(r.u128()?);
        let branch = BranchId(r.u128()?);
        let relation = RelationId(r.u64()?);
        let schema_epoch = SchemaEpoch(r.u64()?);
        let schema_transition = r.optional_oid()?;
        // 4 bytes of length + 1 tag is the floor for one row.
        let row_count = r.count(5)?;
        let mut rows = Vec::with_capacity(row_count);
        for _ in 0..row_count {
            let len = r.count(1)?;
            let encoded = r.take(len)?;
            rows.push(DeltaRow::decode_canonical(encoded)?);
        }
        Ok(CoordinateEntry {
            graph,
            branch,
            relation,
            schema_epoch,
            schema_transition,
            rows,
        })
    }
}

impl LogicalDeltaTemplate {
    /// The template's canonical bytes — the transcript its digest is taken
    /// over, and the bytes a capsule durably carries.
    pub fn canonical_bytes(&self) -> Result<Vec<u8>, CanonicalError> {
        let mut w = Writer::new();
        w.u16(self.format);
        w.oid(self.intent_semantics_oid);
        w.bytes32(&self.source_intent_root_digest);
        w.count(self.coordinate_entries.len())?;
        for entry in &self.coordinate_entries {
            entry.write(&mut w)?;
        }
        Ok(w.finish())
    }

    /// Decode a template and VALIDATE it. There is deliberately no decode that
    /// skips validation: a template that decodes but is not canonical has no
    /// stable digest, so admitting one would let the same logical change
    /// present two identities.
    pub fn decode_canonical(bytes: &[u8]) -> Result<Self, CanonicalError> {
        let mut r = Reader::new(bytes);
        let format = r.u16()?;
        if format != DELTA_FORMAT_V1 {
            return Err(CanonicalError::UnsupportedFormat { format });
        }
        let intent_semantics_oid = r.oid()?;
        let source_intent_root_digest = r.bytes32()?;
        // 16+16+8+8+1+4 is the floor for one coordinate entry.
        let entry_count = r.count(53)?;
        let mut coordinate_entries = Vec::with_capacity(entry_count);
        for _ in 0..entry_count {
            coordinate_entries.push(CoordinateEntry::read(&mut r)?);
        }
        if !r.is_exhausted() {
            return Err(CanonicalError::TrailingBytes {
                remaining: r.remaining(),
            });
        }
        let template = LogicalDeltaTemplate {
            format,
            intent_semantics_oid,
            source_intent_root_digest,
            coordinate_entries,
        };
        template.validate()?;
        Ok(template)
    }

    /// Check every canonicality law. Called on decode, and available to a
    /// builder that wants to fail before it produces a digest nobody can
    /// reproduce.
    pub fn validate(&self) -> Result<(), CanonicalError> {
        if self.format != DELTA_FORMAT_V1 {
            return Err(CanonicalError::UnsupportedFormat {
                format: self.format,
            });
        }
        for index in 1..self.coordinate_entries.len() {
            let previous = self.coordinate_entries[index - 1].coordinate();
            let current = self.coordinate_entries[index].coordinate();
            // Strictly ascending: equal is the duplicate-coordinate invalidity,
            // descending is the noncanonical-order one.
            if previous >= current {
                return Err(CanonicalError::NonCanonicalCoordinateOrder { index });
            }
        }
        for (entry_index, entry) in self.coordinate_entries.iter().enumerate() {
            let mut previous: Option<Vec<u8>> = None;
            for (row_index, row) in entry.rows.iter().enumerate() {
                let encoded = row.canonical_bytes()?;
                if let Some(prior) = &previous
                    && prior.as_slice() >= encoded.as_slice()
                {
                    return Err(CanonicalError::NonCanonicalRowOrder {
                        entry: entry_index,
                        index: row_index,
                    });
                }
                previous = Some(encoded);
            }
        }
        Ok(())
    }
}

/// Put coordinate entries and their rows into canonical order.
///
/// Separate from [`LogicalDeltaTemplate::validate`] on purpose. Sorting is
/// what a *builder* does with effects it just produced; validating is what a
/// *reader* does with bytes it was handed. Folding the two together would mean
/// a reader silently repairing a noncanonical template into a different digest
/// than the writer computed — the exact disagreement the canonical form exists
/// to prevent.
///
/// Returns an error if any row cannot be encoded, since ordering is defined by
/// the encoding. On error, `entries` is left unchanged: callers must never
/// observe a half-canonicalized slice whose earlier rows were reordered or
/// whose failing row collection was consumed.
pub fn canonicalize(entries: &mut [CoordinateEntry]) -> Result<(), CanonicalError> {
    let canonical_rows: Vec<Vec<DeltaRow>> = entries
        .iter()
        .map(|entry| -> Result<Vec<DeltaRow>, CanonicalError> {
            // Work on clones until every row in every entry has encoded. The
            // public API takes borrowed caller state, so publishing one entry
            // before a later error would make `Err` destructive.
            let mut encoded: Vec<(Vec<u8>, DeltaRow)> = entry
                .rows
                .iter()
                .cloned()
                .map(|mut row| {
                    row.canonicalize_embedded_collections();
                    row.canonical_bytes().map(|bytes| (bytes, row))
                })
                .collect::<Result<_, _>>()?;

            // Encode once, sort the pairs: re-encoding inside a comparator
            // would make sorting quadratic in encoding cost for no benefit.
            encoded.sort_by(|left, right| left.0.cmp(&right.0));
            Ok(encoded.into_iter().map(|(_, row)| row).collect())
        })
        .collect::<Result<_, _>>()?;

    // This is the commit point. No fallible operation remains, so every entry
    // is published together and coordinate sorting cannot expose partial work.
    for (entry, rows) in entries.iter_mut().zip(canonical_rows) {
        entry.rows = rows;
    }
    entries.sort_by_key(|entry| entry.coordinate());
    Ok(())
}

impl crate::LogicalDeltaBatch {
    /// The batch's canonical bytes — the transcript its `batch_digest` is taken
    /// over, and therefore what makes that digest an idempotency key two
    /// implementations could ever agree on.
    ///
    /// The digest itself is NOT stored here, for the same reason the template's
    /// is not: hashing needs `fgdb-crypto`, which sits at a higher foundation
    /// position than this crate. A field holding a caller-supplied digest that
    /// nothing in this crate could verify would be a field free to lie, which
    /// is worse than not having it.
    ///
    /// Provenance is included: the source template digest and the marker
    /// identity are part of the transcript, so two batches with identical
    /// payloads from different commits are different batches.
    pub fn canonical_bytes(&self) -> Result<Vec<u8>, CanonicalError> {
        let mut w = Writer::new();
        w.u16(self.format());
        w.bytes32(self.source_template_digest());
        let marker = self.commit_marker_identity();
        w.oid(marker.marker_oid);
        w.u64(marker.commit_seq.0);
        w.u64(self.commit_seq().0);
        w.u64(self.frontier().0);
        w.count(self.coordinate_entries().len())?;
        for entry in self.coordinate_entries() {
            entry.write(&mut w)?;
        }
        Ok(w.finish())
    }
}
