//! Canonical property-value cells and correlated projection rows.
//!
//! Source scalars are borrowed while a candidate key is tested. DISTINCT
//! copies a new value row once; ALL copies each retained matching occurrence.
//! Every copy follows its logical scratch reservations.

use super::{BindingSlot, MAX_PATTERN_VERTICES};
use crate::GlaExecutionEvent;
use crate::algebra_exec::ProjectedRows;
use fgdb_delta_types::PropertyKeyId;
use fgdb_types::{CanonicalPropertyValue, CanonicalScalar, EId, VId};
use std::borrow::Borrow;
use std::cmp::Ordering;

const CANONICAL_GRAPH_ROW_DOMAIN: &[u8] = b"fgdb:graph-row:v1\0";
const CANONICAL_GRAPH_VALUE_DOMAIN: &[u8] = b"fgdb:graph-value:v1\0";

/// A canonical graph-row refusal. Diagnostics contain structure and scalar
/// error kinds, never graph identities, map keys or text payloads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GraphValueDecodeError {
    InvalidDomain,
    Truncated,
    LengthOverflow,
    TrailingBytes,
    ColumnLimit { declared: u64 },
    UnknownTag(u8),
    DepthLimit,
    NodeLimit,
    InvalidMapKey,
    NonCanonicalMapKeys,
    AllocationFailed,
    Scalar(fgdb_types::ScalarDecodeError),
}

impl core::fmt::Display for GraphValueDecodeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::InvalidDomain => f.write_str("invalid canonical graph row/value domain"),
            Self::Truncated => f.write_str("truncated canonical graph row/value frame"),
            Self::LengthOverflow => f.write_str("canonical graph row/value length overflow"),
            Self::TrailingBytes => f.write_str("trailing canonical graph row/value bytes"),
            Self::ColumnLimit { declared } => {
                write!(f, "canonical graph row has too many columns: {declared}")
            }
            Self::UnknownTag(tag) => write!(f, "unknown canonical graph value tag {tag}"),
            Self::DepthLimit => f.write_str("canonical graph value nesting limit exceeded"),
            Self::NodeLimit => f.write_str("canonical graph value node limit exceeded"),
            Self::InvalidMapKey => f.write_str("canonical graph map key is not UTF-8"),
            Self::NonCanonicalMapKeys => {
                f.write_str("canonical graph map keys are not strictly increasing")
            }
            Self::AllocationFailed => f.write_str("canonical graph row allocation failed"),
            Self::Scalar(error) => write!(f, "invalid canonical graph scalar: {error}"),
        }
    }
}

impl std::error::Error for GraphValueDecodeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Scalar(error) => Some(error),
            _ => None,
        }
    }
}

struct CanonicalValueReader<'a> {
    bytes: &'a [u8],
}

impl<'a> CanonicalValueReader<'a> {
    fn take(&mut self, len: usize) -> Result<&'a [u8], GraphValueDecodeError> {
        let (head, tail) = self
            .bytes
            .split_at_checked(len)
            .ok_or(GraphValueDecodeError::Truncated)?;
        self.bytes = tail;
        Ok(head)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], GraphValueDecodeError> {
        let mut bytes = [0; N];
        bytes.copy_from_slice(self.take(N)?);
        Ok(bytes)
    }

    fn u64(&mut self) -> Result<u64, GraphValueDecodeError> {
        Ok(u64::from_be_bytes(self.array()?))
    }

    fn u128(&mut self) -> Result<u128, GraphValueDecodeError> {
        Ok(u128::from_be_bytes(self.array()?))
    }

    fn len(&mut self) -> Result<usize, GraphValueDecodeError> {
        usize::try_from(self.u64()?).map_err(|_| GraphValueDecodeError::LengthOverflow)
    }

    fn frame(&mut self) -> Result<Self, GraphValueDecodeError> {
        let len = self.len()?;
        Ok(Self {
            bytes: self.take(len)?,
        })
    }

    fn domain(&mut self, domain: &[u8]) -> Result<(), GraphValueDecodeError> {
        if self.take(domain.len())? != domain {
            return Err(GraphValueDecodeError::InvalidDomain);
        }
        Ok(())
    }

    fn require_items(
        &self,
        count: usize,
        minimum_bytes: usize,
    ) -> Result<(), GraphValueDecodeError> {
        // Division avoids a hostile count overflowing a multiplication.
        if count > self.bytes.len() / minimum_bytes {
            return Err(GraphValueDecodeError::Truncated);
        }
        Ok(())
    }

    fn count(&mut self, minimum_bytes: usize) -> Result<usize, GraphValueDecodeError> {
        let count = self.len()?;
        self.require_items(count, minimum_bytes)?;
        Ok(count)
    }

    fn finish(&self) -> Result<(), GraphValueDecodeError> {
        if self.bytes.is_empty() {
            Ok(())
        } else {
            Err(GraphValueDecodeError::TrailingBytes)
        }
    }
}

fn canonical_decode_vec<T>(count: usize) -> Result<Vec<T>, GraphValueDecodeError> {
    let mut values = Vec::new();
    values
        .try_reserve_exact(count)
        .map_err(|_| GraphValueDecodeError::AllocationFailed)?;
    Ok(values)
}

fn canonical_child_bounds(
    count: usize,
    depth: usize,
    remaining: &mut usize,
) -> Result<(), GraphValueDecodeError> {
    if count != 0 && depth == GraphValue::MAX_LIST_DEPTH {
        return Err(GraphValueDecodeError::DepthLimit);
    }
    if count > *remaining {
        return Err(GraphValueDecodeError::NodeLimit);
    }
    // Claim every child, including unvisited siblings, before reserving the
    // collection's vectors. Descendants may spend only the unclaimed budget.
    *remaining -= count;
    Ok(())
}

fn decode_graph_value(
    input: &mut CanonicalValueReader<'_>,
    depth: usize,
    remaining: &mut usize,
    resolver: Option<&dyn fgdb_types::CanonicalScalarResolver>,
    allow_stored_zoned: bool,
) -> Result<GraphValue, GraphValueDecodeError> {
    if depth > GraphValue::MAX_LIST_DEPTH {
        return Err(GraphValueDecodeError::DepthLimit);
    }
    // This cell was already claimed by its parent, or as the row's root.
    // A zero remaining budget still permits a leaf or empty collection.
    let mut body = input.frame()?;
    let [tag] = body.array()?;
    let value = match tag {
        0 => {
            let scalar = body.frame()?;
            let value = match resolver {
                Some(resolver) => CanonicalScalar::decode_with_resolver(scalar.bytes, resolver),
                None if allow_stored_zoned => CanonicalScalar::decode_stored(scalar.bytes),
                None => CanonicalScalar::decode(scalar.bytes),
            }
            .map_err(GraphValueDecodeError::Scalar)?;
            GraphValue::Scalar(value)
        }
        1 => GraphValue::Vertex(VId(body.u128()?)),
        2 => {
            let start = VId(body.u128()?);
            let count = body.count(32)?;
            let mut steps = canonical_decode_vec(count)?;
            for _ in 0..count {
                steps.push((EId(body.u128()?), VId(body.u128()?)));
            }
            GraphValue::Path(GraphPath::new(start, steps.into_boxed_slice()))
        }
        3 => {
            let count = body.count(16)?;
            let mut values = canonical_decode_vec(count)?;
            for _ in 0..count {
                values.push(VId(body.u128()?));
            }
            GraphValue::Vertices(values.into_boxed_slice())
        }
        4 => {
            let count = body.count(16)?;
            let mut values = canonical_decode_vec(count)?;
            for _ in 0..count {
                values.push(EId(body.u128()?));
            }
            GraphValue::Edges(values.into_boxed_slice())
        }
        5 => GraphValue::Edge(EId(body.u128()?)),
        6 => {
            let count = body.count(8 + 1)?;
            canonical_child_bounds(count, depth, remaining)?;
            let mut values = canonical_decode_vec(count)?;
            for _ in 0..count {
                values.push(decode_graph_value(
                    &mut body,
                    depth + 1,
                    remaining,
                    resolver,
                    allow_stored_zoned,
                )?);
            }
            GraphValue::List(values.into_boxed_slice())
        }
        7 => {
            let count = body.count(8 + 8 + 1)?;
            canonical_child_bounds(count, depth, remaining)?;
            let mut keys: Vec<Box<str>> = canonical_decode_vec(count)?;
            let mut values = canonical_decode_vec(count)?;
            for _ in 0..count {
                let len = body.len()?;
                let key = core::str::from_utf8(body.take(len)?)
                    .map_err(|_| GraphValueDecodeError::InvalidMapKey)?;
                if keys
                    .last()
                    .is_some_and(|previous| previous.as_bytes() >= key.as_bytes())
                {
                    return Err(GraphValueDecodeError::NonCanonicalMapKeys);
                }
                let mut owned = String::new();
                owned
                    .try_reserve_exact(key.len())
                    .map_err(|_| GraphValueDecodeError::AllocationFailed)?;
                owned.push_str(key);
                let value = decode_graph_value(
                    &mut body,
                    depth + 1,
                    remaining,
                    resolver,
                    allow_stored_zoned,
                )?;
                keys.push(owned.into_boxed_str());
                values.push(value);
            }
            GraphValue::Map {
                keys: keys.into_boxed_slice(),
                values: values.into_boxed_slice(),
            }
        }
        other => return Err(GraphValueDecodeError::UnknownTag(other)),
    };
    body.finish()?;
    Ok(value)
}

/// An owned traversal, ordered by alternating vertex and edge identities.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GraphPath {
    start: VId,
    steps: Box<[(EId, VId)]>,
}

impl GraphPath {
    /// A path value from its start vertex and (edge, vertex) steps, as a
    /// pattern binding carries it. Like a Vertex or Edge value, it is not
    /// checked against any graph.
    #[must_use]
    pub fn new(start: VId, steps: Box<[(EId, VId)]>) -> Self {
        Self { start, steps }
    }
    #[must_use]
    pub fn start(&self) -> VId {
        self.start
    }
    #[must_use]
    pub fn steps(&self) -> &[(EId, VId)] {
        &self.steps
    }
    #[must_use]
    pub fn nodes(&self) -> impl Iterator<Item = VId> + '_ {
        core::iter::once(self.start).chain(self.steps.iter().map(|(_, vertex)| *vertex))
    }
    #[must_use]
    pub fn vertices(&self) -> impl Iterator<Item = VId> + '_ {
        self.nodes()
    }
    #[must_use]
    pub fn edges(&self) -> impl ExactSizeIterator<Item = EId> + '_ {
        self.steps.iter().map(|(edge, _)| *edge)
    }
    #[must_use]
    pub fn len(&self) -> usize {
        self.steps.len()
    }
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum GraphPathFunction {
    Value,
    Length,
    Nodes,
    Edges,
    /// Identity of a captured, fixed-length relationship atom.
    Edge,
    /// Labels of a vertex, returned in canonical LabelId order as GraphValue::List([Text, ...]).
    Labels,
    /// Type of an edge, returned as GraphValue::Scalar(CanonicalScalar::Text).
    Type,
    /// Complete visible properties of a vertex or a fixed relationship.
    Properties,
}

/// Preparation-only column declarations. Names are checked before being owned
/// by a prepared pattern. The caller already resolved property key identities.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum GraphColumn<'a> {
    Vertex {
        name: &'a str,
        variable: &'a str,
    },
    Property {
        name: &'a str,
        variable: &'a str,
        key: PropertyKeyId,
    },
    EdgeProperty {
        name: &'a str,
        variable: &'a str,
        key: PropertyKeyId,
    },
    Path {
        name: &'a str,
        variable: &'a str,
        function: GraphPathFunction,
    },
    /// The list a pattern comprehension's Collect clause gathers per row
    /// (fgdb-7nwox): `clause` indexes the clause sequence.
    Collection {
        name: &'a str,
        clause: usize,
    },
}

impl<'a> GraphColumn<'a> {
    #[must_use]
    pub const fn path(name: &'a str, variable: &'a str, function: GraphPathFunction) -> Self {
        Self::Path {
            name,
            variable,
            function,
        }
    }
    #[must_use]
    pub const fn vertex(name: &'a str, variable: &'a str) -> Self {
        Self::Vertex { name, variable }
    }

    #[must_use]
    pub const fn property(name: &'a str, variable: &'a str, key: PropertyKeyId) -> Self {
        Self::Property {
            name,
            variable,
            key,
        }
    }

    #[must_use]
    pub const fn edge_property(name: &'a str, variable: &'a str, key: PropertyKeyId) -> Self {
        Self::EdgeProperty {
            name,
            variable,
            key,
        }
    }

    #[must_use]
    pub const fn collection(name: &'a str, clause: usize) -> Self {
        Self::Collection { name, clause }
    }

    #[must_use]
    pub const fn name(self) -> &'a str {
        match self {
            Self::Vertex { name, .. }
            | Self::Property { name, .. }
            | Self::EdgeProperty { name, .. }
            | Self::Path { name, .. }
            | Self::Collection { name, .. } => name,
        }
    }

    /// The binding a column reads; a collection reads no outer binding.
    #[allow(dead_code)]
    pub(super) const fn variable(self) -> &'a str {
        match self {
            Self::Vertex { variable, .. }
            | Self::Property { variable, .. }
            | Self::EdgeProperty { variable, .. }
            | Self::Path { variable, .. } => variable,
            Self::Collection { .. } => "",
        }
    }
}

impl core::fmt::Debug for GraphColumn<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("GraphColumn")
            .field("definition", &"[REDACTED]")
            .finish()
    }
}

/// Compiler-bound value expression. Column order is semantic, unlike aliases.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ValueProjection {
    Vertex {
        slot: BindingSlot,
    },
    Property {
        slot: BindingSlot,
        key: PropertyKeyId,
    },
    EdgeProperty {
        capture: u32,
        key: PropertyKeyId,
    },
    Path {
        capture: u32,
        function: GraphPathFunction,
    },
    Labels {
        slot: BindingSlot,
    },
    Type {
        capture: u32,
    },
    Properties {
        slot: BindingSlot,
    },
    EdgeProperties {
        capture: u32,
    },
    /// The list a Collect group gathered for this occurrence (fgdb-7nwox).
    Collected {
        group: u32,
    },
}

/// Scalar values retain their exact canonical type, collation and time binding.
/// Missing properties and null-extended vertices project as Scalar(Null).
/// No numeric coercion occurs. No VId is reserved as a null sentinel.
/// Canonical scalar order precedes the disjoint vertex-identity domain.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum GraphValue {
    Scalar(CanonicalScalar),
    Vertex(VId),
    Path(GraphPath),
    Vertices(Box<[VId]>),
    Edges(Box<[EId]>),
    Edge(EId),
    List(Box<[GraphValue]>),
    /// An openCypher map (fgdb-2jw3z). `keys` are unique and ascending by
    /// UTF-8 bytes, and `values[i]` belongs to `keys[i]`. Build one only
    /// through `GraphValue::map`, which enforces that; validate_bounds
    /// refuses any other shape.
    Map {
        keys: Box<[Box<str>]>,
        values: Box<[GraphValue]>,
    },
}

impl GraphValue {
    pub const MAX_LIST_DEPTH: usize = 64;
    pub const MAX_LIST_NODES: usize = 65_536;

    #[must_use]
    pub fn as_list(&self) -> Option<&[GraphValue]> {
        match self {
            Self::List(values) => Some(values),
            _ => None,
        }
    }

    /// A map from entries in any order. `None` when a key repeats: a map
    /// literal with a duplicate key is refused, never silently resolved.
    #[must_use]
    pub fn map(entries: Vec<(Box<str>, GraphValue)>) -> Option<Self> {
        let mut entries = entries;
        entries.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
        if entries.windows(2).any(|pair| pair[0].0 == pair[1].0) {
            return None;
        }
        let (keys, values): (Vec<_>, Vec<_>) = entries.into_iter().unzip();
        Some(Self::Map {
            keys: keys.into_boxed_slice(),
            values: values.into_boxed_slice(),
        })
    }

    /// The keys and their values, keys ascending.
    #[must_use]
    pub fn as_map(&self) -> Option<(&[Box<str>], &[GraphValue])> {
        match self {
            Self::Map { keys, values } => Some((keys, values)),
            _ => None,
        }
    }

    /// Bound recursive definitions before compilation or parameter admission.
    #[must_use]
    pub fn validate_bounds(&self) -> bool {
        fn visit(value: &GraphValue, depth: usize, remaining: &mut usize) -> bool {
            if depth > GraphValue::MAX_LIST_DEPTH || *remaining == 0 {
                return false;
            }
            *remaining -= 1;
            match value {
                GraphValue::List(values) => values.iter().all(|v| visit(v, depth + 1, remaining)),
                GraphValue::Map { keys, values } => {
                    keys.len() == values.len()
                        && keys
                            .windows(2)
                            .all(|pair| pair[0].as_bytes() < pair[1].as_bytes())
                        && values.iter().all(|v| visit(v, depth + 1, remaining))
                }
                _ => true,
            }
        }
        let mut remaining = Self::MAX_LIST_NODES;
        visit(self, 0, &mut remaining)
    }

    /// A stored property value as a query value (fgdb-k3nub): a scalar is
    /// itself, a List keeps its element order and a Map its text keys. `None`
    /// when the value exceeds the bounds [`Self::validate_bounds`] enforces: a
    /// stored value is admitted under its own format bounds, and a query value
    /// is never a truncated copy of one.
    #[must_use]
    pub fn from_property_value(value: &CanonicalPropertyValue) -> Option<Self> {
        fn visit(
            value: &CanonicalPropertyValue,
            depth: usize,
            remaining: &mut usize,
        ) -> Option<GraphValue> {
            if depth > GraphValue::MAX_LIST_DEPTH || *remaining == 0 {
                return None;
            }
            *remaining -= 1;
            Some(match value {
                CanonicalPropertyValue::Scalar(scalar) => GraphValue::Scalar(scalar.clone()),
                CanonicalPropertyValue::List(list) => GraphValue::List(
                    list.values()
                        .iter()
                        .map(|value| visit(value, depth + 1, remaining))
                        .collect::<Option<Vec<_>>>()?
                        .into_boxed_slice(),
                ),
                CanonicalPropertyValue::Map(map) => GraphValue::map(
                    map.entries()
                        .iter()
                        .map(|entry| {
                            Some((
                                Box::<str>::from(entry.key().as_str()),
                                visit(entry.value(), depth + 1, remaining)?,
                            ))
                        })
                        .collect::<Option<Vec<_>>>()?,
                )?,
            })
        }
        let mut remaining = Self::MAX_LIST_NODES;
        visit(value, 0, &mut remaining)
    }

    /// A stored property value as an owned query value for an executor
    /// (fgdb-k3nub). Every node is admitted as a scratch entry and every
    /// scalar payload quantum as work and scratch, all before the copy is
    /// built, as a scalar operand is admitted before it is cloned. A value
    /// beyond the query bounds raises `StoredValueBounds`; it is never NULL.
    /// Hosts must authorize the field before calling this conversion and supply
    /// their existing evaluator and signed-work control; this grants no authority.
    pub fn admit_property_value<E: From<crate::GraphIntegerError>>(
        value: &CanonicalPropertyValue,
        control: &mut impl FnMut(crate::GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Self, E> {
        Self::charge_property_value(value, control)?;
        Self::from_property_value(value).ok_or_else(|| {
            E::from(crate::GraphIntegerError {
                instruction: 0,
                kind: crate::GraphIntegerErrorKind::StoredValueBounds,
            })
        })
    }

    /// The admission charge of admit_property_value, before any copy: a
    /// scratch entry per node and per key quantum, and work and scratch per
    /// scalar payload quantum.
    pub(crate) fn charge_property_value<E>(
        value: &CanonicalPropertyValue,
        control: &mut impl FnMut(crate::GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<(), E> {
        fn charge<E>(
            value: &CanonicalPropertyValue,
            control: &mut impl FnMut(crate::GlaExecutionEvent) -> Result<(), E>,
        ) -> Result<(), E> {
            control(crate::GlaExecutionEvent::ScratchEntry)?;
            match value {
                CanonicalPropertyValue::Scalar(scalar) => {
                    crate::algebra_exec::charge_payload(scalar, &mut |event| {
                        control(event)?;
                        control(crate::GlaExecutionEvent::ScratchEntry)
                    })
                }
                CanonicalPropertyValue::List(list) => list
                    .values()
                    .iter()
                    .try_for_each(|value| charge(value, &mut *control)),
                CanonicalPropertyValue::Map(map) => map.entries().iter().try_for_each(|entry| {
                    let key = entry.key().as_str().len();
                    for _ in 0..=key.div_ceil(GRAPH_VALUE_PAYLOAD_UNIT_BYTES) {
                        control(crate::GlaExecutionEvent::ScratchEntry)?;
                    }
                    charge(entry.value(), &mut *control)
                }),
            }
        }
        if value.is_collection() {
            charge(value, control)?;
        } else if let Some(scalar) = value.as_scalar() {
            crate::algebra_exec::charge_payload(scalar, &mut |event| {
                control(event)?;
                control(crate::GlaExecutionEvent::ScratchEntry)
            })?;
        }
        Ok(())
    }

    /// [`Self::payload_units`] of the value a stored property admits to,
    /// counted in place: `None` beyond the bounds from_property_value
    /// enforces, where admission raises StoredValueBounds. Only the traversal
    /// stack allocates.
    #[must_use]
    pub(crate) fn stored_payload_units(value: &CanonicalPropertyValue) -> Option<usize> {
        let mut total = 0usize;
        let mut remaining = Self::MAX_LIST_NODES;
        let mut pending = vec![(value, 0usize)];
        while let Some((value, depth)) = pending.pop() {
            if depth > Self::MAX_LIST_DEPTH || remaining == 0 {
                return None;
            }
            remaining -= 1;
            total = total.saturating_add(1);
            match value {
                CanonicalPropertyValue::Scalar(scalar) => {
                    total = total.saturating_add(scalar_leaf_units(scalar));
                }
                CanonicalPropertyValue::List(list) => {
                    pending.extend(list.values().iter().map(|value| (value, depth + 1)));
                }
                CanonicalPropertyValue::Map(map) => {
                    for entry in map.entries() {
                        total = total.saturating_add(
                            entry
                                .key()
                                .as_str()
                                .len()
                                .div_ceil(GRAPH_VALUE_PAYLOAD_UNIT_BYTES),
                        );
                        pending.push((entry.value(), depth + 1));
                    }
                }
            }
        }
        Some(total)
    }

    /// Whether a property could hold this value (fgdb-k3nub): a scalar, or a
    /// List or Map whose members are such values within the depth bound. A
    /// graph element at any depth is not one. This allocates nothing; use
    /// [`Self::to_property_value`] for the stored value itself.
    #[must_use]
    pub(crate) fn is_stored_value(&self) -> bool {
        fn visit(value: &GraphValue, depth: usize) -> bool {
            if depth > GraphValue::MAX_LIST_DEPTH {
                return false;
            }
            match value {
                GraphValue::Scalar(_) => true,
                GraphValue::List(values) | GraphValue::Map { values, .. } => {
                    values.iter().all(|value| visit(value, depth + 1))
                }
                GraphValue::Vertex(_)
                | GraphValue::Path(_)
                | GraphValue::Vertices(_)
                | GraphValue::Edges(_)
                | GraphValue::Edge(_) => false,
            }
        }
        visit(self, 0)
    }

    /// This value as a stored property value (fgdb-k3nub). `None` for a value
    /// no property can hold: a vertex, edge, path or element list at any depth,
    /// a map key that is not UCS_BASIC text, or a collection beyond the stored
    /// format's bounds. Depth is checked before descending.
    #[must_use]
    pub fn to_property_value(&self) -> Option<CanonicalPropertyValue> {
        fn visit(value: &GraphValue, depth: usize) -> Option<CanonicalPropertyValue> {
            use fgdb_types::{CanonicalList, CanonicalMap, CanonicalMapEntry, CanonicalText};
            if depth > GraphValue::MAX_LIST_DEPTH {
                return None;
            }
            match value {
                GraphValue::Scalar(scalar) => Some(CanonicalPropertyValue::Scalar(scalar.clone())),
                GraphValue::List(values) => CanonicalList::try_new(
                    values
                        .iter()
                        .map(|value| visit(value, depth + 1))
                        .collect::<Option<Vec<_>>>()?,
                )
                .ok()
                .map(CanonicalPropertyValue::List),
                GraphValue::Map { keys, values } => CanonicalMap::try_new(
                    keys.iter()
                        .zip(values.iter())
                        .map(|(key, value)| {
                            Some(CanonicalMapEntry::new(
                                CanonicalText::new_ucs_basic(key).ok()?,
                                visit(value, depth + 1)?,
                            ))
                        })
                        .collect::<Option<Vec<_>>>()?,
                )
                .ok()
                .map(CanonicalPropertyValue::Map),
                GraphValue::Vertex(_)
                | GraphValue::Path(_)
                | GraphValue::Vertices(_)
                | GraphValue::Edges(_)
                | GraphValue::Edge(_) => None,
            }
        }
        visit(self, 0)
    }

    /// The stored value a CREATE or SET writes (fgdb-k3nub): a scalar, or a
    /// list whose elements are themselves written values. `None` for a map at
    /// any depth, and for everything [`Self::to_property_value`] refuses.
    ///
    /// Storage holds maps, but openCypher refuses a map-valued property
    /// (InvalidPropertyType). Writes therefore admit lists only, until the
    /// owner rules on stored maps.
    #[must_use]
    pub fn to_written_property(&self) -> Option<CanonicalPropertyValue> {
        fn holds_map(value: &GraphValue, depth: usize) -> bool {
            match value {
                GraphValue::Map { .. } => true,
                // Depth beyond the bound is refused by to_property_value.
                GraphValue::List(values) if depth <= GraphValue::MAX_LIST_DEPTH => {
                    values.iter().any(|value| holds_map(value, depth + 1))
                }
                _ => false,
            }
        }
        if holds_map(self, 0) {
            return None;
        }
        self.to_property_value()
    }

    /// Exact canonical size without encoding or copying any payload. The
    /// iterative walk keeps only O(nesting) frames, including for a wide list.
    /// Callers can therefore admit a complete document before allocating its
    /// canonical transcript. Structural validity remains `validate_bounds`'s
    /// responsibility, just as it is for `canonical_bytes`.
    pub fn canonical_byte_len(&self) -> Result<usize, fgdb_types::ScalarEncodeError> {
        use fgdb_types::ScalarEncodeError::EncodedSizeOverflow;

        enum Frame<'a> {
            Value(&'a GraphValue),
            Rest(&'a [GraphValue]),
        }
        let mut total = CANONICAL_GRAPH_VALUE_DOMAIN.len();
        let mut pending = vec![Frame::Value(self)];
        while let Some(frame) = pending.pop() {
            let value = match frame {
                Frame::Value(value) => value,
                Frame::Rest(values) => {
                    if let Some((first, rest)) = values.split_first() {
                        pending.push(Frame::Rest(rest));
                        pending.push(Frame::Value(first));
                    }
                    continue;
                }
            };
            // Every value has its own u64 frame length and u8 arm tag.
            total = total.checked_add(9).ok_or(EncodedSizeOverflow)?;
            let bytes = match value {
                Self::Scalar(value) => value
                    .canonical_encoded_len()?
                    .checked_add(8)
                    .ok_or(EncodedSizeOverflow)?,
                Self::Vertex(_) | Self::Edge(_) => 16,
                Self::Path(value) => value
                    .steps
                    .len()
                    .checked_mul(32)
                    .and_then(|bytes| bytes.checked_add(24))
                    .ok_or(EncodedSizeOverflow)?,
                Self::Vertices(values) => values
                    .len()
                    .checked_mul(16)
                    .and_then(|bytes| bytes.checked_add(8))
                    .ok_or(EncodedSizeOverflow)?,
                Self::Edges(values) => values
                    .len()
                    .checked_mul(16)
                    .and_then(|bytes| bytes.checked_add(8))
                    .ok_or(EncodedSizeOverflow)?,
                Self::List(values) => {
                    pending.push(Frame::Rest(values));
                    8
                }
                Self::Map { keys, values } => {
                    // Match the encoder's key/value zip even on an invalid
                    // public value. Parameter admission checks structure first.
                    let pairs = keys.len().min(values.len());
                    let mut bytes = 8usize;
                    for key in &keys[..pairs] {
                        bytes = bytes
                            .checked_add(8)
                            .and_then(|bytes| bytes.checked_add(key.len()))
                            .ok_or(EncodedSizeOverflow)?;
                    }
                    pending.push(Frame::Rest(&values[..pairs]));
                    bytes
                }
            };
            total = total.checked_add(bytes).ok_or(EncodedSizeOverflow)?;
        }
        Ok(total)
    }

    /// Self-delimiting, domain-separated identity bytes. Encoding is iterative;
    /// even externally constructed deep values never recurse on the stack.
    pub fn canonical_bytes(&self) -> Result<Vec<u8>, fgdb_types::ScalarEncodeError> {
        enum Task<'a> {
            Value(&'a GraphValue),
            Key(&'a str),
            End(usize),
        }
        let mut bytes = CANONICAL_GRAPH_VALUE_DOMAIN.to_vec();
        let mut pending = vec![Task::Value(self)];
        while let Some(task) = pending.pop() {
            let value = match task {
                Task::End(at) => {
                    let len = (bytes.len() - at - 8) as u64;
                    bytes[at..at + 8].copy_from_slice(&len.to_be_bytes());
                    continue;
                }
                Task::Key(key) => {
                    bytes.extend_from_slice(&(key.len() as u64).to_be_bytes());
                    bytes.extend_from_slice(key.as_bytes());
                    continue;
                }
                Task::Value(value) => value,
            };
            let at = bytes.len();
            bytes.extend_from_slice(&0u64.to_be_bytes());
            pending.push(Task::End(at));
            match value {
                Self::Scalar(value) => {
                    bytes.push(0);
                    let encoded = value.encode()?;
                    bytes.extend_from_slice(&(encoded.len() as u64).to_be_bytes());
                    bytes.extend_from_slice(&encoded);
                }
                Self::Vertex(value) => {
                    bytes.push(1);
                    bytes.extend_from_slice(&value.0.to_be_bytes());
                }
                Self::Path(value) => {
                    bytes.push(2);
                    bytes.extend_from_slice(&value.start.0.to_be_bytes());
                    bytes.extend_from_slice(&(value.steps.len() as u64).to_be_bytes());
                    for (edge, vertex) in &value.steps {
                        bytes.extend_from_slice(&edge.0.to_be_bytes());
                        bytes.extend_from_slice(&vertex.0.to_be_bytes());
                    }
                }
                Self::Vertices(values) => {
                    bytes.push(3);
                    bytes.extend_from_slice(&(values.len() as u64).to_be_bytes());
                    for value in values {
                        bytes.extend_from_slice(&value.0.to_be_bytes());
                    }
                }
                Self::Edges(values) => {
                    bytes.push(4);
                    bytes.extend_from_slice(&(values.len() as u64).to_be_bytes());
                    for value in values {
                        bytes.extend_from_slice(&value.0.to_be_bytes());
                    }
                }
                Self::Edge(value) => {
                    bytes.push(5);
                    bytes.extend_from_slice(&value.0.to_be_bytes());
                }
                Self::List(values) => {
                    bytes.push(6);
                    bytes.extend_from_slice(&(values.len() as u64).to_be_bytes());
                    // Child tasks reserve their own length prefix when visited.
                    for value in values.iter().rev() {
                        pending.push(Task::Value(value));
                    }
                    continue;
                }
                // Each key's bytes precede its value; values carry their own
                // length prefix exactly as list children do.
                Self::Map { keys, values } => {
                    bytes.push(7);
                    bytes.extend_from_slice(&(values.len() as u64).to_be_bytes());
                    for (key, value) in keys.iter().zip(values.iter()).rev() {
                        pending.push(Task::Value(value));
                        pending.push(Task::Key(key));
                    }
                    continue;
                }
            }
        }
        Ok(bytes)
    }

    /// Count nested cells and variable payload without cloning their storage.
    /// Traversal is iterative; an arbitrary-depth public input cannot exhaust
    /// the stack. The frame vector is the only scratch and stays O(nesting).
    #[must_use]
    pub fn payload_units(&self) -> usize {
        enum Frame<'a> {
            Root(&'a GraphValue),
            Rest(&'a [GraphValue], usize),
        }
        let mut total = 0usize;
        let mut pending = vec![Frame::Root(self)];
        while let Some(frame) = pending.pop() {
            match frame {
                Frame::Root(value) => {
                    total = total.saturating_add(1);
                    let children = match value {
                        Self::List(values) => Some(values),
                        Self::Map { keys, values } => {
                            for key in keys.iter() {
                                total = total.saturating_add(
                                    key.len().div_ceil(GRAPH_VALUE_PAYLOAD_UNIT_BYTES),
                                );
                            }
                            Some(values)
                        }
                        _ => None,
                    };
                    if let Some(values) = children {
                        if let Some(first) = values.first() {
                            pending.push(Frame::Rest(values, 1));
                            pending.push(Frame::Root(first));
                        }
                    } else {
                        total = total.saturating_add(value.leaf_payload_units());
                    }
                }
                Frame::Rest(values, at) => {
                    if at < values.len() {
                        pending.push(Frame::Rest(values, at + 1));
                        pending.push(Frame::Root(&values[at]));
                    }
                }
            }
        }
        total
    }

    fn leaf_payload_units(&self) -> usize {
        let sizes = match self {
            Self::Scalar(scalar) => return scalar_leaf_units(scalar),
            Self::Path(value) => [core::mem::size_of_val(value.steps()), 0],
            Self::Vertices(value) => [core::mem::size_of_val(value.as_ref()), 0],
            Self::Edges(value) => [core::mem::size_of_val(value.as_ref()), 0],
            _ => [0, 0],
        };
        sizes
            .into_iter()
            .map(|n| n.div_ceil(GRAPH_VALUE_PAYLOAD_UNIT_BYTES))
            .sum()
    }

    /// Iterative owned copy. Reserve traversal frames, every nested cell and
    /// leaf payload before growing storage or cloning any payload.
    pub fn copy_with_control<E>(
        &self,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Self, E> {
        enum Task<'a> {
            Value(&'a GraphValue),
            Finish(usize),
            FinishMap(usize, &'a [Box<str>]),
        }
        control(GlaExecutionEvent::ScratchEntry)?;
        let mut pending = vec![Task::Value(self)];
        let mut output = Vec::new();
        while let Some(task) = pending.pop() {
            control(GlaExecutionEvent::Work)?;
            match task {
                Task::Finish(start) => {
                    control(GlaExecutionEvent::ScratchEntry)?;
                    let children = output.split_off(start).into_boxed_slice();
                    output.push(Self::List(children));
                }
                Task::FinishMap(start, keys) => {
                    control(GlaExecutionEvent::ScratchEntry)?;
                    for key in keys {
                        for _ in 0..=key.len().div_ceil(GRAPH_VALUE_PAYLOAD_UNIT_BYTES) {
                            control(GlaExecutionEvent::ScratchEntry)?;
                        }
                    }
                    let values = output.split_off(start).into_boxed_slice();
                    output.push(Self::Map {
                        keys: keys.into(),
                        values,
                    });
                }
                Task::Value(Self::Map { keys, values }) => {
                    control(GlaExecutionEvent::ScratchEntry)?;
                    pending.push(Task::FinishMap(output.len(), keys));
                    for value in values.iter().rev() {
                        control(GlaExecutionEvent::ScratchEntry)?;
                        pending.push(Task::Value(value));
                    }
                }
                Task::Value(Self::List(values)) => {
                    control(GlaExecutionEvent::ScratchEntry)?;
                    pending.push(Task::Finish(output.len()));
                    for value in values.iter().rev() {
                        control(GlaExecutionEvent::ScratchEntry)?;
                        pending.push(Task::Value(value));
                    }
                }
                Task::Value(value) => {
                    for _ in 0..=value.leaf_payload_units() {
                        control(GlaExecutionEvent::ScratchEntry)?;
                    }
                    output.push(value.clone());
                }
            }
        }
        Ok(output.pop().expect("one root copied"))
    }
    #[must_use]
    pub fn as_scalar(&self) -> Option<&CanonicalScalar> {
        match self {
            Self::Scalar(value) => Some(value),
            _ => None,
        }
    }

    #[must_use]
    pub fn as_vertex(&self) -> Option<VId> {
        match self {
            Self::Vertex(value) => Some(*value),
            _ => None,
        }
    }

    #[must_use]
    pub fn as_edge(&self) -> Option<EId> {
        match self {
            Self::Edge(value) => Some(*value),
            _ => None,
        }
    }

    #[must_use]
    pub fn as_path(&self) -> Option<&GraphPath> {
        match self {
            Self::Path(value) => Some(value),
            _ => None,
        }
    }
    #[must_use]
    pub fn is_null(&self) -> bool {
        matches!(self, Self::Scalar(CanonicalScalar::Null))
    }
}

impl core::fmt::Debug for GraphValue {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let kind = match self {
            Self::Scalar(_) => "Scalar",
            Self::Vertex(_) => "Vertex",
            Self::Path(_) => "Path",
            Self::Vertices(_) => "Vertices",
            Self::Edges(_) => "Edges",
            Self::Edge(_) => "Edge",
            Self::List(_) => "List",
            Self::Map { .. } => "Map",
        };
        f.debug_tuple(kind).field(&"[REDACTED]").finish()
    }
}

/// One complete, owned value tuple. A row's positions match the immutable
/// prepared column schema. Results do not retain the source snapshot lifetime.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GraphValueRow {
    values: Box<[GraphValue]>,
}

impl GraphValueRow {
    pub(crate) fn into_prefix(self, width: usize) -> Self {
        let mut values = self.values.into_vec();
        values.truncate(width);
        Self {
            values: values.into_boxed_slice(),
        }
    }

    pub fn canonical_bytes(&self) -> Result<Vec<u8>, fgdb_types::ScalarEncodeError> {
        let mut bytes = CANONICAL_GRAPH_ROW_DOMAIN.to_vec();
        bytes.extend_from_slice(&(self.values.len() as u64).to_be_bytes());
        for value in &self.values {
            let encoded = value.canonical_bytes()?;
            bytes.extend_from_slice(&(encoded.len() as u64).to_be_bytes());
            bytes.extend_from_slice(&encoded);
        }
        Ok(bytes)
    }

    /// Decode one complete canonical result row. Every frame must be consumed
    /// exactly, and each cell obeys the same depth/node limits as
    /// [GraphValue::validate_bounds]. Artifact-bound scalars require
    /// [Self::decode_canonical_with_resolver].
    ///
    /// Lengths are checked against the input before fallible reservations.
    /// The caller must bound the encoded row size and account for the owned
    /// decoded row separately from any spill-buffer memory pool.
    pub fn decode_canonical(bytes: &[u8]) -> Result<Self, GraphValueDecodeError> {
        Self::decode_canonical_inner(bytes, None, false)
    }

    /// Decode a spooled row from authenticated intermediate storage without a
    /// live [`fgdb_types::CanonicalScalarResolver`], preserving stored zoned
    /// timestamps.
    pub fn decode_stored(bytes: &[u8]) -> Result<Self, GraphValueDecodeError> {
        Self::decode_canonical_inner(bytes, None, true)
    }

    /// Decode using only the supplied content-addressed scalar artifacts.
    /// Nested scalar cells use the same resolver; no host locale or timezone
    /// database is consulted.
    pub fn decode_canonical_with_resolver(
        bytes: &[u8],
        resolver: &dyn fgdb_types::CanonicalScalarResolver,
    ) -> Result<Self, GraphValueDecodeError> {
        Self::decode_canonical_inner(bytes, Some(resolver), false)
    }

    fn decode_canonical_inner(
        bytes: &[u8],
        resolver: Option<&dyn fgdb_types::CanonicalScalarResolver>,
        allow_stored_zoned: bool,
    ) -> Result<Self, GraphValueDecodeError> {
        let mut row = CanonicalValueReader { bytes };
        row.domain(CANONICAL_GRAPH_ROW_DOMAIN)?;
        let declared = row.u64()?;
        if declared > MAX_PATTERN_VERTICES as u64 {
            return Err(GraphValueDecodeError::ColumnLimit { declared });
        }
        let count = usize::try_from(declared).map_err(|_| GraphValueDecodeError::LengthOverflow)?;
        // A column has an outer length, value domain, body length and tag,
        // even before accounting for that tag's mandatory payload.
        row.require_items(count, 8 + CANONICAL_GRAPH_VALUE_DOMAIN.len() + 8 + 1)?;
        let mut values = canonical_decode_vec(count)?;
        for _ in 0..count {
            let mut column = row.frame()?;
            column.domain(CANONICAL_GRAPH_VALUE_DOMAIN)?;
            // Claim the root once; collections claim their complete child
            // inventories before allocating, rather than as visits occur.
            let mut remaining = GraphValue::MAX_LIST_NODES - 1;
            values.push(decode_graph_value(
                &mut column,
                0,
                &mut remaining,
                resolver,
                allow_stored_zoned,
            )?);
            column.finish()?;
        }
        row.finish()?;
        Ok(Self {
            values: values.into_boxed_slice(),
        })
    }

    /// Compiler-owned relational operators call this only after schema checks
    /// and per-cell/payload reservations. It is not a public unchecked row API.
    pub fn from_owned_values(values: Vec<GraphValue>) -> Self {
        debug_assert!(values.len() <= MAX_PATTERN_VERTICES);
        Self {
            values: values.into_boxed_slice(),
        }
    }

    /// A list-comprehension element scope (fgdb-20foe): this row's values,
    /// already copied under the caller's reservations, followed by `slots`
    /// element slots (one per binding, innermost last). Element scopes may
    /// exceed the pattern column bound by their nesting depth, which the
    /// expression-nesting limit bounds.
    pub(crate) fn element_scope(values: Vec<GraphValue>, slots: usize) -> Self {
        let mut values = values;
        for _ in 0..slots {
            values.push(GraphValue::Scalar(fgdb_types::CanonicalScalar::Null));
        }
        Self {
            values: values.into_boxed_slice(),
        }
    }

    /// Bind the slot `offset` positions before the end of an element scope
    /// (0 is the innermost binding).
    pub(crate) fn set_from_end(&mut self, offset: usize, value: GraphValue) {
        let len = self.values.len();
        if let Some(slot) = len
            .checked_sub(offset + 1)
            .and_then(|at| self.values.get_mut(at))
        {
            *slot = value;
        }
    }

    /// Zero-column identity tuple used by the relational singleton source.
    pub(crate) fn unit() -> Self {
        Self {
            values: Box::new([]),
        }
    }

    #[must_use]
    pub fn values(&self) -> &[GraphValue] {
        &self.values
    }
    #[must_use]
    pub fn get(&self, column: usize) -> Option<&GraphValue> {
        self.values.get(column)
    }
    #[must_use]
    pub fn len(&self) -> usize {
        self.values.len()
    }
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
}

impl core::fmt::Debug for GraphValueRow {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("GraphValueRow")
            .field("columns", &self.len())
            .field("values", &"[REDACTED]")
            .finish()
    }
}

/// Each additional 64 bytes of variable scalar payload reserves a logical
/// scratch entry before copying. This is not an allocator-byte/peak-memory cap.
pub const GRAPH_VALUE_PAYLOAD_UNIT_BYTES: usize = 64;

/// A scalar's variable payload quanta, as GraphValue::payload_units counts a
/// scalar leaf: a text's characters and comparison key, bytes, a zone name.
fn scalar_leaf_units(scalar: &CanonicalScalar) -> usize {
    let sizes = match scalar {
        CanonicalScalar::Text(value) => [
            value.len(),
            value.canonical_sort_key().map_or(0, <[u8]>::len),
        ],
        CanonicalScalar::Bytes(value) => [value.as_slice().len(), 0],
        CanonicalScalar::Timestamp(value) => {
            [value.zone().map_or(0, |zone| zone.identifier().len()), 0]
        }
        _ => [0, 0],
    };
    sizes
        .into_iter()
        .map(|n| n.div_ceil(GRAPH_VALUE_PAYLOAD_UNIT_BYTES))
        .sum()
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum ValueRef<'a> {
    Scalar(&'a CanonicalScalar),
    Vertex(VId),
    Path(&'a GraphPath),
    Vertices(&'a [VId]),
    Edges(&'a [EId]),
    Edge(EId),
    List(&'a [GraphValue]),
    Map {
        keys: &'a [Box<str>],
        values: &'a [GraphValue],
    },
    /// A stored List or Map property read in place (fgdb-bqri3). The
    /// aggregation fast path borrows its group keys and extrema from the
    /// snapshot, where a collection is a CanonicalPropertyValue, not a
    /// GraphValue. It orders as the List or Map it holds
    /// (cmp_orderability); row identity and retention never meet it.
    Stored(&'a CanonicalPropertyValue),
}

impl<'a> ValueRef<'a> {
    /// A stored value read in place: a scalar as itself, a List or Map as
    /// Stored.
    pub(crate) fn stored(value: &'a CanonicalPropertyValue) -> Self {
        value.as_scalar().map_or(Self::Stored(value), Self::Scalar)
    }
}

impl ValueRef<'_> {
    fn payload_units(self) -> usize {
        match self {
            Self::Path(path) => return path.len().saturating_mul(2).saturating_add(1),
            Self::Vertices(values) => return values.len(),
            Self::Edges(values) => return values.len(),
            Self::List(values) => {
                return values
                    .iter()
                    .fold(0usize, |n, v| n.saturating_add(v.payload_units()));
            }
            Self::Map { keys, values } => {
                let keys = keys.iter().fold(0usize, |n, key| {
                    n.saturating_add(key.len().div_ceil(GRAPH_VALUE_PAYLOAD_UNIT_BYTES))
                });
                return values
                    .iter()
                    .fold(keys, |n, v| n.saturating_add(v.payload_units()));
            }
            // As the GraphValue it admits to (GraphValue::payload_units).
            Self::Stored(CanonicalPropertyValue::List(list)) => {
                return list.values().iter().fold(0usize, |n, v| {
                    n.saturating_add(ValueRef::stored(v).payload_units())
                });
            }
            Self::Stored(CanonicalPropertyValue::Map(map)) => {
                return map.entries().iter().fold(0usize, |n, entry| {
                    n.saturating_add(
                        entry
                            .key()
                            .as_str()
                            .len()
                            .div_ceil(GRAPH_VALUE_PAYLOAD_UNIT_BYTES),
                    )
                    .saturating_add(ValueRef::stored(entry.value()).payload_units())
                });
            }
            Self::Stored(CanonicalPropertyValue::Scalar(scalar)) => {
                return Self::Scalar(scalar).payload_units();
            }
            _ => {}
        }
        let bytes = match self {
            Self::Scalar(CanonicalScalar::Bytes(value)) => value.as_slice().len(),
            Self::Scalar(CanonicalScalar::Text(value)) => {
                // Both lengths have construction-time bounds; their sum fits
                // usize on every supported target, including wasm32.
                value.len() + value.canonical_sort_key().map_or(0, <[u8]>::len)
            }
            Self::Scalar(CanonicalScalar::Timestamp(value)) => {
                value.zone().map_or(0, |zone| zone.identifier().len())
            }
            _ => 0,
        };
        bytes.div_ceil(GRAPH_VALUE_PAYLOAD_UNIT_BYTES)
    }

    fn into_owned(self) -> GraphValue {
        match self {
            Self::Scalar(value) => GraphValue::Scalar(value.clone()),
            Self::Vertex(value) => GraphValue::Vertex(value),
            Self::Path(value) => GraphValue::Path(value.clone()),
            Self::Vertices(value) => GraphValue::Vertices(value.into()),
            Self::Edges(value) => GraphValue::Edges(value.into()),
            Self::Edge(value) => GraphValue::Edge(value),
            Self::List(values) => GraphValue::List(values.into()),
            Self::Map { keys, values } => GraphValue::Map {
                keys: keys.into(),
                values: values.into(),
            },
            Self::Stored(_) => unreachable!("a stored value orders in place, never retained"),
        }
    }
}

// Heterogeneous lookup compares borrowed source cells with owned result cells
// under exactly the same order as GraphValueRow::Ord. Hashes never substitute
// for identity. No candidate scalar or tuple allocation is needed for lookup.
pub(crate) trait RowKey {
    fn width(&self) -> usize;
    fn cell(&self, at: usize) -> ValueRef<'_>;
}

struct BorrowedRow<'a>(&'a [ValueRef<'a>]);
impl RowKey for BorrowedRow<'_> {
    fn width(&self) -> usize {
        self.0.len()
    }
    fn cell(&self, at: usize) -> ValueRef<'_> {
        self.0[at]
    }
}
impl RowKey for GraphValueRow {
    fn width(&self) -> usize {
        self.values.len()
    }
    fn cell(&self, at: usize) -> ValueRef<'_> {
        value_ref(&self.values[at])
    }
}

/// An owned value as the borrowed cell lookups and retention compare.
fn value_ref(value: &GraphValue) -> ValueRef<'_> {
    match value {
        GraphValue::Scalar(value) => ValueRef::Scalar(value),
        GraphValue::Vertex(value) => ValueRef::Vertex(*value),
        GraphValue::Path(value) => ValueRef::Path(value),
        GraphValue::Vertices(value) => ValueRef::Vertices(value),
        GraphValue::Edges(value) => ValueRef::Edges(value),
        GraphValue::Edge(value) => ValueRef::Edge(*value),
        GraphValue::List(value) => ValueRef::List(value),
        GraphValue::Map { keys, values } => ValueRef::Map { keys, values },
    }
}
impl<'a> Borrow<dyn RowKey + 'a> for GraphValueRow {
    fn borrow(&self) -> &(dyn RowKey + 'a) {
        self
    }
}
impl PartialEq for dyn RowKey + '_ {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for dyn RowKey + '_ {}
impl PartialOrd for dyn RowKey + '_ {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for dyn RowKey + '_ {
    fn cmp(&self, other: &Self) -> Ordering {
        for at in 0..self.width().min(other.width()) {
            let order = self.cell(at).cmp(&other.cell(at));
            if order != Ordering::Equal {
                return order;
            }
        }
        self.width().cmp(&other.width())
    }
}

pub(super) fn collect_values<'a, E: From<crate::GraphIntegerError>>(
    columns: &[ValueProjection],
    bindings: &[Option<VId>],
    projected: &mut ProjectedRows<GraphValueRow>,
    property: &mut impl FnMut(VId, PropertyKeyId) -> Result<Option<&'a CanonicalPropertyValue>, E>,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<(), E> {
    collect_values_with_paths(columns, bindings, &[], &[], projected, property, control)
}

pub(super) fn collect_values_with_paths<'a, E: From<crate::GraphIntegerError>>(
    columns: &[ValueProjection],
    bindings: &[Option<VId>],
    paths: &[Option<GraphPath>],
    collected: &[Option<GraphValue>],
    projected: &mut ProjectedRows<GraphValueRow>,
    property: &mut impl FnMut(VId, PropertyKeyId) -> Result<Option<&'a CanonicalPropertyValue>, E>,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<(), E> {
    collect_values_with_element_properties(
        columns,
        bindings,
        paths,
        collected,
        projected,
        property,
        &mut |_, _| panic!("edge properties require an explicit edge property source"),
        &mut |_| panic!("vertex labels require an explicit label source"),
        &mut |_| panic!("edge type requires an explicit edge source"),
        control,
    )
}

#[allow(clippy::too_many_arguments)]
pub(super) fn collect_values_with_element_properties<'a, E: From<crate::GraphIntegerError>>(
    columns: &[ValueProjection],
    bindings: &[Option<VId>],
    paths: &[Option<GraphPath>],
    collected: &[Option<GraphValue>],
    projected: &mut ProjectedRows<GraphValueRow>,
    property: &mut impl FnMut(VId, PropertyKeyId) -> Result<Option<&'a CanonicalPropertyValue>, E>,
    edge_property: &mut impl FnMut(EId, PropertyKeyId) -> Result<Option<&'a CanonicalPropertyValue>, E>,
    vertex_labels: &mut impl FnMut(VId) -> Result<Option<&'a [GraphValue]>, E>,
    edge_type: &mut impl FnMut(EId) -> Result<Option<&'a CanonicalScalar>, E>,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<(), E> {
    collect_values_with_element_maps(
        columns,
        bindings,
        paths,
        collected,
        projected,
        property,
        edge_property,
        vertex_labels,
        edge_type,
        &mut |_| panic!("property maps require an enumerating vertex source"),
        &mut |_| panic!("property maps require an enumerating edge source"),
        control,
    )
}

pub(crate) type PropertyMapParts<'a> = (&'a [Box<str>], &'a [GraphValue]);

pub(crate) fn property_map_parts(
    value: Option<&GraphValue>,
) -> Result<PropertyMapParts<'_>, crate::GraphIntegerError> {
    let error = |kind| crate::GraphIntegerError {
        instruction: 0,
        kind,
    };
    let value =
        value.ok_or_else(|| error(crate::GraphIntegerErrorKind::PropertyMapSourceRequired))?;
    if !value.validate_bounds() {
        return Err(error(crate::GraphIntegerErrorKind::NonMap));
    }
    value
        .as_map()
        .ok_or_else(|| error(crate::GraphIntegerErrorKind::NonMap))
}

#[allow(clippy::too_many_arguments)]
pub(super) fn collect_values_with_element_maps<'a, E: From<crate::GraphIntegerError>>(
    columns: &[ValueProjection],
    bindings: &[Option<VId>],
    paths: &[Option<GraphPath>],
    collected: &[Option<GraphValue>],
    projected: &mut ProjectedRows<GraphValueRow>,
    property: &mut impl FnMut(VId, PropertyKeyId) -> Result<Option<&'a CanonicalPropertyValue>, E>,
    edge_property: &mut impl FnMut(EId, PropertyKeyId) -> Result<Option<&'a CanonicalPropertyValue>, E>,
    vertex_labels: &mut impl FnMut(VId) -> Result<Option<&'a [GraphValue]>, E>,
    edge_type: &mut impl FnMut(EId) -> Result<Option<&'a CanonicalScalar>, E>,
    vertex_properties: &mut impl FnMut(VId) -> Result<PropertyMapParts<'a>, E>,
    edge_properties: &mut impl FnMut(EId) -> Result<PropertyMapParts<'a>, E>,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<(), E> {
    let null = CanonicalScalar::Null;
    let mut computed: [Option<GraphValue>; MAX_PATTERN_VERTICES] = core::array::from_fn(|_| None);
    for (at, column) in columns.iter().enumerate() {
        let ValueProjection::Path { capture, function } = column else {
            continue;
        };
        let Some(path) = paths.get(*capture as usize).and_then(Option::as_ref) else {
            continue;
        };
        computed[at] = match function {
            GraphPathFunction::Value => None,
            GraphPathFunction::Edge => path
                .steps()
                .first()
                .map(|(edge, _)| GraphValue::Edge(*edge)),
            GraphPathFunction::Length => {
                Some(GraphValue::Scalar(CanonicalScalar::Int(path.len() as i64)))
            }
            GraphPathFunction::Nodes => {
                for _ in 0..=path.len() {
                    control(GlaExecutionEvent::Work)?;
                    control(GlaExecutionEvent::ScratchEntry)?;
                }
                Some(GraphValue::Vertices(path.nodes().collect()))
            }
            GraphPathFunction::Edges => {
                for _ in 0..path.len() {
                    control(GlaExecutionEvent::Work)?;
                    control(GlaExecutionEvent::ScratchEntry)?;
                }
                Some(GraphValue::Edges(path.edges().collect()))
            }
            GraphPathFunction::Labels | GraphPathFunction::Type | GraphPathFunction::Properties => {
                None
            }
        };
    }
    // Stored List and Map properties (fgdb-k3nub) are admitted as owned query
    // values here, after every selected field is read, so their cells can
    // borrow them exactly as path-function cells borrow `computed`.
    let mut stored: [Option<GraphValue>; MAX_PATTERN_VERTICES] = core::array::from_fn(|_| None);
    let mut collections: [Option<&'a CanonicalPropertyValue>; MAX_PATTERN_VERTICES] =
        [None; MAX_PATTERN_VERTICES];
    let mut key = [ValueRef::Scalar(&null); MAX_PATTERN_VERTICES];
    for (at, column) in columns.iter().enumerate() {
        control(GlaExecutionEvent::Work)?;
        key[at] = match column {
            ValueProjection::Vertex { slot } => {
                bindings[slot.ordinal() as usize].map_or(ValueRef::Scalar(&null), ValueRef::Vertex)
            }
            ValueProjection::Property { slot, key } => {
                // An absent binding is not a vertex with a missing property.
                // Never consult the source with a fabricated identity.
                let value = match bindings[slot.ordinal() as usize] {
                    Some(vid) => property(vid, *key)?,
                    None => None,
                };
                match value {
                    Some(CanonicalPropertyValue::Scalar(value)) => ValueRef::Scalar(value),
                    Some(value) => {
                        collections[at] = Some(value);
                        ValueRef::Scalar(&null)
                    }
                    None => ValueRef::Scalar(&null),
                }
            }
            ValueProjection::EdgeProperty { capture, key } => {
                let value = match paths.get(*capture as usize).and_then(Option::as_ref) {
                    Some(path) => {
                        let [(edge, _)] = path.steps() else {
                            unreachable!("edge property captures contain exactly one relationship")
                        };
                        edge_property(*edge, *key)?
                    }
                    None => None,
                };
                match value {
                    Some(CanonicalPropertyValue::Scalar(value)) => ValueRef::Scalar(value),
                    Some(value) => {
                        collections[at] = Some(value);
                        ValueRef::Scalar(&null)
                    }
                    None => ValueRef::Scalar(&null),
                }
            }
            ValueProjection::Labels { slot } => match bindings[slot.ordinal() as usize] {
                Some(vid) => match vertex_labels(vid)? {
                    Some(labels) => ValueRef::List(labels),
                    None => ValueRef::List(&[]),
                },
                None => ValueRef::Scalar(&null),
            },
            ValueProjection::Properties { slot } => match bindings[slot.ordinal() as usize] {
                Some(vid) => {
                    let (keys, values) = vertex_properties(vid)?;
                    ValueRef::Map { keys, values }
                }
                None => ValueRef::Scalar(&null),
            },
            ValueProjection::EdgeProperties { capture } => match paths
                .get(*capture as usize)
                .and_then(Option::as_ref)
            {
                Some(path) => {
                    let [(edge, _)] = path.steps() else {
                        unreachable!("edge property-map captures contain exactly one relationship")
                    };
                    let (keys, values) = edge_properties(*edge)?;
                    ValueRef::Map { keys, values }
                }
                None => ValueRef::Scalar(&null),
            },
            ValueProjection::Type { capture } => {
                let value = match paths.get(*capture as usize).and_then(Option::as_ref) {
                    Some(path) => {
                        let [(edge, _)] = path.steps() else {
                            unreachable!("edge captures contain exactly one relationship")
                        };
                        edge_type(*edge)?
                    }
                    None => None,
                };
                ValueRef::Scalar(value.unwrap_or(&null))
            }
            ValueProjection::Path { capture, function } => {
                match (function, computed[at].as_ref()) {
                    (GraphPathFunction::Value, _) => paths
                        .get(*capture as usize)
                        .and_then(Option::as_ref)
                        .map_or(ValueRef::Scalar(&null), ValueRef::Path),
                    (_, Some(GraphValue::Scalar(value))) => ValueRef::Scalar(value),
                    (_, Some(GraphValue::Vertices(value))) => ValueRef::Vertices(value),
                    (_, Some(GraphValue::Edges(value))) => ValueRef::Edges(value),
                    (_, Some(GraphValue::Edge(value))) => ValueRef::Edge(*value),
                    _ => ValueRef::Scalar(&null),
                }
            }
            // The list this occurrence's Collect group published (fgdb-7nwox).
            // An executor that collects none, or a list past one value's
            // bounds, refuses; neither is ever read as NULL or truncated.
            ValueProjection::Collected { group } => {
                let refused = |kind| {
                    E::from(crate::GraphIntegerError {
                        instruction: 0,
                        kind,
                    })
                };
                let list = collected
                    .get(*group as usize)
                    .and_then(Option::as_ref)
                    .ok_or_else(|| {
                        refused(crate::GraphIntegerErrorKind::CollectionSourceRequired)
                    })?;
                if !list.validate_bounds() {
                    return Err(refused(crate::GraphIntegerErrorKind::CollectionBounds));
                }
                value_ref(list)
            }
        };
        for _ in 0..key[at].payload_units() {
            control(GlaExecutionEvent::Work)?;
        }
    }
    for at in 0..columns.len() {
        if let Some(value) = collections[at] {
            stored[at] = Some(GraphValue::admit_property_value(value, control)?);
        }
    }
    for (cell, value) in key.iter_mut().zip(&stored) {
        if let Some(value) = value {
            *cell = value_ref(value);
        }
    }
    let borrowed = BorrowedRow(&key[..columns.len()]);
    // Resolve ALL selected fields before testing the page cutoff. A later
    // unreadable property is an error even when an earlier cell already sorts
    // after the retained prefix, or the logical LIMIT is zero.
    if !projected.should_retain_value(&borrowed as &dyn RowKey, control)? {
        return Ok(());
    }
    control(GlaExecutionEvent::ScratchEntry)?;
    let mut values = Vec::new();
    for at in 0..columns.len() {
        control(GlaExecutionEvent::ScratchEntry)?;
        if computed[at].is_some() {
            continue;
        }
        for _ in 0..key[at].payload_units() {
            control(GlaExecutionEvent::ScratchEntry)?;
        }
    }
    let mut copied: [Option<GraphValue>; MAX_PATTERN_VERTICES] = core::array::from_fn(|_| None);
    for at in 0..columns.len() {
        if computed[at].is_none() && stored[at].is_none() {
            copied[at] = Some(key[at].into_owned());
        }
    }
    // An admitted stored collection moves into the row; it is never copied.
    for at in 0..columns.len() {
        values.push(
            copied[at]
                .take()
                .or_else(|| computed[at].take())
                .or_else(|| stored[at].take())
                .expect("resolved cell"),
        );
    }
    projected.insert_value(GraphValueRow {
        values: values.into_boxed_slice(),
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use fgdb_types::CanonicalF64;
    use std::collections::BTreeSet;

    /// A caller's own source or control failure, or a data exception the
    /// projection raised. The projection never folds the second into the first.
    #[derive(Debug, PartialEq)]
    enum Failure<S> {
        Source(S),
        Data(crate::GraphIntegerError),
    }
    impl<S> From<crate::GraphIntegerError> for Failure<S> {
        fn from(error: crate::GraphIntegerError) -> Self {
            Self::Data(error)
        }
    }

    fn canonical_test_frame(payload: &[u8]) -> Vec<u8> {
        let mut bytes = (payload.len() as u64).to_be_bytes().to_vec();
        bytes.extend_from_slice(payload);
        bytes
    }

    fn canonical_test_row_from_value(value: &[u8]) -> Vec<u8> {
        let mut row = b"fgdb:graph-row:v1\0".to_vec();
        row.extend_from_slice(&1u64.to_be_bytes());
        row.extend_from_slice(&canonical_test_frame(value));
        row
    }

    fn canonical_test_row_from_body(body: &[u8]) -> Vec<u8> {
        let mut value = b"fgdb:graph-value:v1\0".to_vec();
        value.extend_from_slice(&canonical_test_frame(body));
        canonical_test_row_from_value(&value)
    }

    #[test]
    fn canonical_rows_round_trip_every_value_kind_and_reject_every_truncation() {
        let scalar_values = [
            CanonicalScalar::Null,
            CanonicalScalar::Bool(true),
            CanonicalScalar::Int(i64::MIN),
            CanonicalScalar::Decimal(
                fgdb_types::CanonicalDecimal::from_coefficient(-123_456).unwrap(),
            ),
            CanonicalScalar::Float(CanonicalF64::new(f64::NAN)),
            CanonicalScalar::ucs_basic_text("a\0é").unwrap(),
            CanonicalScalar::Timestamp(
                fgdb_types::CanonicalTimestamp::offset_only(-7, 900).unwrap(),
            ),
            CanonicalScalar::bytes(vec![0, 255, 0]).unwrap(),
        ];
        let mut values: Vec<_> = scalar_values.into_iter().map(GraphValue::Scalar).collect();
        values.extend([
            GraphValue::Vertex(VId(0)),
            GraphValue::Edge(EId(u128::MAX)),
            GraphValue::Path(GraphPath::new(VId(u128::MAX), Box::new([]))),
            GraphValue::Path(GraphPath::new(
                VId(0),
                vec![(EId(0), VId(1)), (EId(u128::MAX), VId(0))].into_boxed_slice(),
            )),
            GraphValue::Vertices(Box::new([])),
            GraphValue::Vertices(vec![VId(0), VId(u128::MAX)].into_boxed_slice()),
            GraphValue::Edges(Box::new([])),
            GraphValue::Edges(vec![EId(u128::MAX), EId(0)].into_boxed_slice()),
            GraphValue::List(Box::new([])),
            GraphValue::map(vec![]).unwrap(),
            GraphValue::map(vec![
                (
                    "é".into(),
                    GraphValue::List(
                        vec![
                            GraphValue::Vertex(VId(3)),
                            GraphValue::Scalar(CanonicalScalar::Null),
                        ]
                        .into_boxed_slice(),
                    ),
                ),
                ("".into(), GraphValue::Scalar(CanonicalScalar::Int(8))),
                (
                    "\0".into(),
                    GraphValue::Edges(vec![EId(4)].into_boxed_slice()),
                ),
            ])
            .unwrap(),
        ]);
        for row in [
            GraphValueRow::from_owned_values(values),
            GraphValueRow::unit(),
            GraphValueRow::from_owned_values(vec![
                GraphValue::Scalar(CanonicalScalar::Null);
                MAX_PATTERN_VERTICES
            ]),
        ] {
            for value in row.values() {
                assert_eq!(
                    value.canonical_byte_len().unwrap(),
                    value.canonical_bytes().unwrap().len(),
                );
            }
            let bytes = row.canonical_bytes().unwrap();
            let decoded = GraphValueRow::decode_canonical(&bytes).unwrap();
            assert_eq!(decoded, row);
            assert_eq!(decoded.canonical_bytes().unwrap(), bytes);
            assert!(decoded.values().iter().all(GraphValue::validate_bounds));
            for end in 0..bytes.len() {
                assert!(
                    GraphValueRow::decode_canonical(&bytes[..end]).is_err(),
                    "truncation at {end} of {}",
                    bytes.len(),
                );
            }
        }

        // An independent fixed-width wire fixture, including a legal zero ID.
        let mut body = vec![1];
        body.extend_from_slice(&0u128.to_be_bytes());
        assert_eq!(
            GraphValueRow::decode_canonical(&canonical_test_row_from_body(&body)).unwrap(),
            GraphValueRow::from_owned_values(vec![GraphValue::Vertex(VId(0))]),
        );
    }

    #[test]
    fn canonical_row_decoder_refuses_domains_tags_and_impossible_lengths() {
        let row = GraphValueRow::from_owned_values(vec![GraphValue::Vertex(VId(1))]);
        let bytes = row.canonical_bytes().unwrap();
        let mut wrong_row = bytes.clone();
        wrong_row[0] ^= 1;
        assert_eq!(
            GraphValueRow::decode_canonical(&wrong_row),
            Err(GraphValueDecodeError::InvalidDomain),
        );
        let mut wrong_value = bytes.clone();
        wrong_value[CANONICAL_GRAPH_ROW_DOMAIN.len() + 8 + 8] ^= 1;
        assert_eq!(
            GraphValueRow::decode_canonical(&wrong_value),
            Err(GraphValueDecodeError::InvalidDomain),
        );
        assert_eq!(
            GraphValueRow::decode_canonical(&canonical_test_row_from_body(&[255])),
            Err(GraphValueDecodeError::UnknownTag(255)),
        );
        for declared in [MAX_PATTERN_VERTICES as u64 + 1, u64::MAX] {
            let mut malformed = CANONICAL_GRAPH_ROW_DOMAIN.to_vec();
            malformed.extend_from_slice(&declared.to_be_bytes());
            assert_eq!(
                GraphValueRow::decode_canonical(&malformed),
                Err(GraphValueDecodeError::ColumnLimit { declared }),
            );
        }
        for at in [
            CANONICAL_GRAPH_ROW_DOMAIN.len() + 8,
            CANONICAL_GRAPH_ROW_DOMAIN.len() + 8 + 8 + CANONICAL_GRAPH_VALUE_DOMAIN.len(),
        ] {
            for length in [0u64, u64::MAX] {
                let mut malformed = bytes.clone();
                malformed[at..at + 8].copy_from_slice(&length.to_be_bytes());
                assert!(GraphValueRow::decode_canonical(&malformed).is_err());
            }
        }
        for tag in [0, 2, 3, 4, 6, 7] {
            let mut body = vec![tag];
            if tag == 2 {
                body.extend_from_slice(&1u128.to_be_bytes());
            }
            body.extend_from_slice(&u64::MAX.to_be_bytes());
            assert!(
                GraphValueRow::decode_canonical(&canonical_test_row_from_body(&body)).is_err(),
                "forged count/length for tag {tag}",
            );
        }
    }

    #[test]
    fn canonical_row_decoder_consumes_each_frame_exactly() {
        let mut vertex = vec![1];
        vertex.extend_from_slice(&12u128.to_be_bytes());
        let mut row = canonical_test_row_from_body(&vertex);
        row.push(0);
        assert_eq!(
            GraphValueRow::decode_canonical(&row),
            Err(GraphValueDecodeError::TrailingBytes),
        );

        let mut value = CANONICAL_GRAPH_VALUE_DOMAIN.to_vec();
        value.extend_from_slice(&canonical_test_frame(&vertex));
        value.push(0);
        assert_eq!(
            GraphValueRow::decode_canonical(&canonical_test_row_from_value(&value)),
            Err(GraphValueDecodeError::TrailingBytes),
        );

        vertex.push(0);
        assert_eq!(
            GraphValueRow::decode_canonical(&canonical_test_row_from_body(&vertex)),
            Err(GraphValueDecodeError::TrailingBytes),
        );
        let mut list = vec![6];
        list.extend_from_slice(&1u64.to_be_bytes());
        list.extend_from_slice(&canonical_test_frame(&vertex));
        assert_eq!(
            GraphValueRow::decode_canonical(&canonical_test_row_from_body(&list)),
            Err(GraphValueDecodeError::TrailingBytes),
        );

        let mut scalar = vec![0];
        scalar.extend_from_slice(&canonical_test_frame(&[0, 0]));
        assert_eq!(
            GraphValueRow::decode_canonical(&canonical_test_row_from_body(&scalar)),
            Err(GraphValueDecodeError::Scalar(
                fgdb_types::ScalarDecodeError::WrongPayloadLength {
                    tag: 0,
                    expected: 0,
                    got: 1,
                },
            )),
        );
        // Ordered scalar bytes for negative zero must be refused, not repaired.
        let mut negative_zero = vec![4];
        negative_zero.extend_from_slice(&0x7fff_ffff_ffff_ffffu64.to_be_bytes());
        let mut scalar = vec![0];
        scalar.extend_from_slice(&canonical_test_frame(&negative_zero));
        assert_eq!(
            GraphValueRow::decode_canonical(&canonical_test_row_from_body(&scalar)),
            Err(GraphValueDecodeError::Scalar(
                fgdb_types::ScalarDecodeError::NonCanonicalFloat {
                    bits: 0x8000_0000_0000_0000,
                },
            )),
        );
    }

    #[test]
    fn canonical_row_decoder_refuses_noncanonical_maps_without_reordering() {
        for keys in [["same", "same"], ["z", "a"]] {
            let row = GraphValueRow::from_owned_values(vec![GraphValue::Map {
                keys: keys.into_iter().map(Box::<str>::from).collect(),
                values: vec![
                    GraphValue::Scalar(CanonicalScalar::Int(1)),
                    GraphValue::Scalar(CanonicalScalar::Int(2)),
                ]
                .into_boxed_slice(),
            }]);
            assert_eq!(
                GraphValueRow::decode_canonical(&row.canonical_bytes().unwrap()),
                Err(GraphValueDecodeError::NonCanonicalMapKeys),
            );
        }
        let mut map = vec![7];
        map.extend_from_slice(&1u64.to_be_bytes());
        map.extend_from_slice(&1u64.to_be_bytes());
        map.push(255);
        let mut null = vec![0];
        null.extend_from_slice(&canonical_test_frame(&[0]));
        map.extend_from_slice(&canonical_test_frame(&null));
        assert_eq!(
            GraphValueRow::decode_canonical(&canonical_test_row_from_body(&map)),
            Err(GraphValueDecodeError::InvalidMapKey),
        );
    }

    #[test]
    fn canonical_row_decoder_enforces_exact_per_cell_depth_and_node_limits() {
        let mut nested = GraphValue::Scalar(CanonicalScalar::Null);
        for depth in 0..GraphValue::MAX_LIST_DEPTH {
            nested = if depth % 2 == 0 {
                GraphValue::List(vec![nested].into_boxed_slice())
            } else {
                GraphValue::map(vec![("k".into(), nested)]).unwrap()
            };
        }
        assert!(nested.validate_bounds());
        let row = GraphValueRow::from_owned_values(vec![nested.clone()]);
        assert_eq!(
            GraphValueRow::decode_canonical(&row.canonical_bytes().unwrap()).unwrap(),
            row,
        );
        let too_deep = GraphValueRow::from_owned_values(vec![GraphValue::List(
            vec![nested].into_boxed_slice(),
        )]);
        assert_eq!(
            GraphValueRow::decode_canonical(&too_deep.canonical_bytes().unwrap()),
            Err(GraphValueDecodeError::DepthLimit),
        );

        let at_limit = GraphValue::List(
            vec![GraphValue::Scalar(CanonicalScalar::Null); GraphValue::MAX_LIST_NODES - 1]
                .into_boxed_slice(),
        );
        assert!(at_limit.validate_bounds());
        // The budget is per cell, not shared by otherwise valid columns.
        let row = GraphValueRow::from_owned_values(vec![at_limit.clone(), at_limit]);
        assert_eq!(
            GraphValueRow::decode_canonical(&row.canonical_bytes().unwrap()).unwrap(),
            row,
        );
        let too_many = GraphValueRow::from_owned_values(vec![GraphValue::List(
            vec![GraphValue::Scalar(CanonicalScalar::Null); GraphValue::MAX_LIST_NODES]
                .into_boxed_slice(),
        )]);
        assert_eq!(
            GraphValueRow::decode_canonical(&too_many.canonical_bytes().unwrap()),
            Err(GraphValueDecodeError::NodeLimit),
        );
    }

    #[test]
    fn canonical_row_decoder_reserves_sibling_nodes_before_descending() {
        let count = GraphValue::MAX_LIST_NODES / 2;
        for (outer_tag, outer_minimum) in [(6_u8, 9_usize), (7, 17)] {
            for (inner_tag, inner_minimum) in [(6_u8, 9_usize), (7, 17)] {
                // Each collection separately fits the node ceiling, and its
                // byte envelope can hold its declared immediate children.
                // Together, root + outer siblings + inner descendants cannot.
                let mut inner = vec![inner_tag];
                inner.extend_from_slice(&(count as u64).to_be_bytes());
                inner.resize(inner.len() + count * inner_minimum, 0);
                let mut outer = vec![outer_tag];
                outer.extend_from_slice(&(count as u64).to_be_bytes());
                if outer_tag == 7 {
                    outer.extend_from_slice(&0_u64.to_be_bytes());
                }
                outer.extend_from_slice(&canonical_test_frame(&inner));
                outer.resize(outer.len().max(1 + 8 + count * outer_minimum), 0);
                let row = canonical_test_row_from_body(&outer);
                assert!(row.len() < 1024 * 1024);
                // The zero-filled descendants are deliberately malformed.
                // Charging only visited nodes allocates the inner inventory
                // and reaches Truncated there. Refuse the overclaimed nodes
                // before reserving that vector or descending into its bytes.
                assert_eq!(
                    GraphValueRow::decode_canonical(&row),
                    Err(GraphValueDecodeError::NodeLimit),
                    "outer tag {outer_tag}, inner tag {inner_tag}",
                );
            }
        }
    }

    struct RowScalarResolver {
        available: bool,
        offset: i32,
    }

    impl fgdb_types::CollationResolver for RowScalarResolver {
        fn artifact_available(&self, object_id: &fgdb_types::ObjectId) -> bool {
            self.available && *object_id == fgdb_types::ObjectId([7; 32])
        }

        fn canonical_sort_key_len(
            &self,
            _: &fgdb_types::NonBinaryTextBinding,
            text: &str,
        ) -> Result<usize, fgdb_types::CollationResolverError> {
            Ok(text.len())
        }

        fn write_canonical_sort_key(
            &self,
            _: &fgdb_types::NonBinaryTextBinding,
            text: &str,
            output: &mut [u8],
        ) -> Result<usize, fgdb_types::CollationResolverError> {
            if output.len() != text.len() {
                return Err(fgdb_types::CollationResolverError::new(1));
            }
            output.copy_from_slice(text.as_bytes());
            Ok(output.len())
        }

        fn canonical_sort_key_matches(
            &self,
            _: &fgdb_types::NonBinaryTextBinding,
            text: &str,
            candidate: &[u8],
        ) -> Result<bool, fgdb_types::CollationResolverError> {
            Ok(candidate == text.as_bytes())
        }
    }

    impl fgdb_types::TzdbResolver for RowScalarResolver {
        fn contains_tzdb(&self, oid: &fgdb_types::ObjectId) -> bool {
            self.available && *oid == fgdb_types::ObjectId([9; 32])
        }

        fn canonical_utc_offset_seconds(
            &self,
            oid: &fgdb_types::ObjectId,
            zone: &str,
            instant: i128,
        ) -> Option<i32> {
            (self.contains_tzdb(oid) && zone == "Etc/UTC" && instant == 0).then_some(self.offset)
        }
    }

    #[test]
    fn canonical_rows_use_only_the_explicit_resolver_for_nested_scalars() {
        let available = RowScalarResolver {
            available: true,
            offset: 0,
        };
        let missing = RowScalarResolver {
            available: false,
            offset: 0,
        };
        let wrong_offset = RowScalarResolver {
            available: true,
            offset: 60,
        };
        let binding = fgdb_types::NonBinaryTextBinding {
            unicode_data_oid: fgdb_types::ObjectId([7; 32]),
            normalization_oid: fgdb_types::ObjectId([7; 32]),
            segmentation_oid: fgdb_types::ObjectId([7; 32]),
            collation_oid: fgdb_types::ObjectId([7; 32]),
        };
        for scalar in [
            CanonicalScalar::Text(
                fgdb_types::CanonicalText::new_non_binary("pinned", binding, &available).unwrap(),
            ),
            CanonicalScalar::Timestamp(
                fgdb_types::CanonicalTimestamp::zoned(
                    0,
                    0,
                    "Etc/UTC",
                    fgdb_types::ObjectId([9; 32]),
                    &available,
                )
                .unwrap(),
            ),
        ] {
            let row = GraphValueRow::from_owned_values(vec![GraphValue::List(
                vec![
                    GraphValue::map(vec![(
                        "artifact".into(),
                        GraphValue::Scalar(scalar.clone()),
                    )])
                    .unwrap(),
                ]
                .into_boxed_slice(),
            )]);
            let bytes = row.canonical_bytes().unwrap();
            let decoded =
                GraphValueRow::decode_canonical_with_resolver(&bytes, &available).unwrap();
            assert_eq!(decoded, row);
            assert_eq!(decoded.canonical_bytes().unwrap(), bytes);
            assert_eq!(
                GraphValueRow::decode_canonical(&bytes),
                Err(GraphValueDecodeError::Scalar(
                    CanonicalScalar::decode(&scalar.encode().unwrap()).unwrap_err(),
                )),
            );
            assert_eq!(
                GraphValueRow::decode_canonical_with_resolver(&bytes, &missing),
                Err(GraphValueDecodeError::Scalar(
                    CanonicalScalar::decode_with_resolver(&scalar.encode().unwrap(), &missing)
                        .unwrap_err(),
                )),
            );
            if matches!(&scalar, CanonicalScalar::Timestamp(_)) {
                assert!(
                    GraphValueRow::decode_canonical_with_resolver(&bytes, &wrong_offset).is_err()
                );
            }
        }
    }

    #[test]
    fn bounded_values_read_rejected_payloads_without_copying_them() {
        let payload = CanonicalPropertyValue::from(CanonicalScalar::bytes(vec![3; 4096]).unwrap());
        let mut rows = ProjectedRows::for_plan(
            false,
            &[super::super::GlaOperator::Limit {
                offset: 0,
                count: Some(1),
            }],
        );
        let mut reads = 0;
        let mut scratch = 0;
        for owner in 0..32 {
            collect_values(
                &columns(),
                &[Some(VId(owner)), Some(VId(42))],
                &mut rows,
                &mut |_, _| {
                    reads += 1;
                    Ok::<_, Failure<()>>(Some(&payload))
                },
                &mut |event| {
                    scratch += usize::from(event == GlaExecutionEvent::ScratchEntry);
                    Ok(())
                },
            )
            .unwrap();
        }
        assert_eq!(reads, 32, "cutoff cannot suppress fallible field reads");
        assert_eq!(scratch, 1 + 2 + 64, "only the first payload row is copied");
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows.first().unwrap().get(0).unwrap().as_vertex(),
            Some(VId(0))
        );
        assert_eq!(
            collect_values(
                &columns(),
                &[Some(VId(99)), Some(VId(42))],
                &mut rows,
                &mut |_, _| Err::<Option<&CanonicalPropertyValue>, _>(Failure::Source(
                    "late property failure"
                )),
                &mut |_| Ok(())
            ),
            Err(Failure::Source("late property failure"))
        );
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn refused_replacement_keeps_the_previous_complete_value_row() {
        let payload = CanonicalPropertyValue::from(CanonicalScalar::bytes(vec![1; 129]).unwrap());
        let initialized = || {
            let mut rows = ProjectedRows::for_plan(
                false,
                &[super::super::GlaOperator::Limit {
                    offset: 0,
                    count: Some(1),
                }],
            );
            collect_values(
                &columns(),
                &[Some(VId(9)), Some(VId(2))],
                &mut rows,
                &mut |_, _| Ok::<_, Failure<usize>>(Some(&payload)),
                &mut |_| Ok(()),
            )
            .unwrap();
            rows
        };
        let mut measured = initialized();
        let mut total = 0;
        collect_values(
            &columns(),
            &[Some(VId(1)), Some(VId(2))],
            &mut measured,
            &mut |_, _| Ok::<_, Failure<usize>>(Some(&payload)),
            &mut |_| {
                total += 1;
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(
            measured.first().unwrap().get(0).unwrap().as_vertex(),
            Some(VId(1))
        );
        for stop in 1..=total {
            let mut rows = initialized();
            let before = rows.first().unwrap().clone();
            let mut calls = 0;
            let result = collect_values(
                &columns(),
                &[Some(VId(1)), Some(VId(2))],
                &mut rows,
                &mut |_, _| Ok::<_, Failure<usize>>(Some(&payload)),
                &mut |_| {
                    calls += 1;
                    if calls == stop {
                        Err(Failure::Source(stop))
                    } else {
                        Ok(())
                    }
                },
            );
            assert_eq!(result, Err(Failure::Source(stop)));
            assert_eq!(calls, stop);
            assert_eq!(rows.len(), 1);
            assert_eq!(rows.first(), Some(&before));
        }
    }

    fn columns() -> [ValueProjection; 2] {
        [
            ValueProjection::Vertex {
                slot: BindingSlot(0),
            },
            ValueProjection::Property {
                slot: BindingSlot(1),
                key: PropertyKeyId(7),
            },
        ]
    }

    #[test]
    fn owned_and_borrowed_value_keys_have_identical_total_order() {
        let scalars = [
            CanonicalScalar::Null,
            CanonicalScalar::Bool(false),
            CanonicalScalar::Int(-1),
            CanonicalScalar::Int(0),
            CanonicalScalar::Float(CanonicalF64::new(f64::NAN)),
            CanonicalScalar::ucs_basic_text("private payload").unwrap(),
            CanonicalScalar::bytes(vec![0, 255]).unwrap(),
        ];
        let mut rows = Vec::new();
        for scalar in &scalars {
            rows.push(GraphValueRow {
                values: vec![
                    GraphValue::Scalar(scalar.clone()),
                    GraphValue::Vertex(VId(9)),
                ]
                .into(),
            });
            rows.push(GraphValueRow {
                values: vec![
                    GraphValue::Vertex(VId(9)),
                    GraphValue::Scalar(scalar.clone()),
                ]
                .into(),
            });
        }
        for left in &rows {
            for right in &rows {
                let borrowed: Vec<_> = (0..right.len()).map(|at| right.cell(at)).collect();
                let key = BorrowedRow(&borrowed);
                assert_eq!(left.cmp(right), (left as &dyn RowKey).cmp(&key));
                let set = BTreeSet::from([left.clone()]);
                assert_eq!(set.contains(&key as &dyn RowKey), left == right);
            }
        }
        assert!(!format!("{rows:?}").contains("private payload"));
        assert!(!format!("{rows:?}").contains("VId(9)"));
    }

    #[test]
    fn missing_and_stored_null_collapse_but_vertex_correlations_survive() {
        let null = CanonicalPropertyValue::Scalar(CanonicalScalar::Null);
        let mut rows = ProjectedRows::new(true);
        for (owner, present) in [(1, false), (1, true), (2, false)] {
            collect_values(
                &columns(),
                &[Some(VId(owner)), Some(VId(4))],
                &mut rows,
                &mut |_, _| Ok::<_, Failure<()>>(present.then_some(&null)),
                &mut |_| Ok(()),
            )
            .unwrap();
        }
        assert_eq!(rows.len(), 2);
        for row in rows.into_rows() {
            assert!(row.get(1).unwrap().is_null());
            assert!(row.get(0).unwrap().as_vertex().is_some());
            assert_eq!(row.get(2), None);
            assert!(!row.is_empty());
        }
    }

    #[test]
    fn duplicate_payload_rows_allocate_nothing_and_payload_growth_is_charged() {
        let payload = CanonicalScalar::bytes(vec![7; 129]).unwrap();
        let stored = CanonicalPropertyValue::Scalar(payload.clone());
        let mut rows = ProjectedRows::new(true);
        let mut scratch = 0;
        for _ in 0..2 {
            collect_values(
                &columns(),
                &[Some(VId(1)), Some(VId(2))],
                &mut rows,
                &mut |_, _| Ok::<_, Failure<()>>(Some(&stored)),
                &mut |event| {
                    scratch += usize::from(event == GlaExecutionEvent::ScratchEntry);
                    Ok(())
                },
            )
            .unwrap();
        }
        assert_eq!(rows.len(), 1);
        assert_eq!(scratch, 1 + 2 + 3);
        assert_eq!(
            rows.first().unwrap().get(1).unwrap().as_scalar(),
            Some(&payload)
        );
    }

    #[test]
    fn every_value_projection_checkpoint_refuses_before_row_publication() {
        let payload = CanonicalPropertyValue::from(
            CanonicalScalar::ucs_basic_text(&"x".repeat(129)).unwrap(),
        );
        let mut total = 0;
        collect_values(
            &columns(),
            &[Some(VId(1)), Some(VId(2))],
            &mut ProjectedRows::new(true),
            &mut |_, _| Ok::<_, Failure<usize>>(Some(&payload)),
            &mut |_| {
                total += 1;
                Ok(())
            },
        )
        .unwrap();
        for stop in 1..=total {
            let mut rows = ProjectedRows::new(true);
            let mut calls = 0;
            let result = collect_values(
                &columns(),
                &[Some(VId(1)), Some(VId(2))],
                &mut rows,
                &mut |_, _| Ok::<_, Failure<usize>>(Some(&payload)),
                &mut |_| {
                    calls += 1;
                    if calls == stop {
                        Err(Failure::Source(stop))
                    } else {
                        Ok(())
                    }
                },
            );
            assert_eq!(result, Err(Failure::Source(stop)));
            assert_eq!(calls, stop);
            assert!(rows.is_empty());
        }
        let mut rows = ProjectedRows::new(true);
        assert_eq!(
            collect_values(
                &columns(),
                &[Some(VId(1)), Some(VId(2))],
                &mut rows,
                &mut |_, _| Err::<Option<&CanonicalPropertyValue>, _>(Failure::Source("source")),
                &mut |_| Ok(())
            ),
            Err(Failure::Source("source"))
        );
        assert!(rows.is_empty());
    }

    #[test]
    fn all_value_rows_charge_each_payload_and_keep_null_occurrences() {
        let payload = CanonicalScalar::bytes(vec![7; 129]).unwrap();
        let stored = CanonicalPropertyValue::Scalar(payload.clone());
        let mut rows = ProjectedRows::new(false);
        let mut scratch = 0;
        for present in [true, false, true, false] {
            collect_values(
                &columns(),
                &[Some(VId(1)), Some(VId(2))],
                &mut rows,
                &mut |_, _| Ok::<_, Failure<()>>(present.then_some(&stored)),
                &mut |event| {
                    scratch += usize::from(event == GlaExecutionEvent::ScratchEntry);
                    Ok(())
                },
            )
            .unwrap();
        }
        assert_eq!(scratch, 2 * (1 + 2 + 3) + 2 * (1 + 2));
        let rows: Vec<_> = rows.into_rows().collect();
        assert_eq!(rows.len(), 4);
        assert!(rows[0].get(1).unwrap().is_null());
        assert_eq!(rows[0], rows[1]);
        assert_eq!(rows[2], rows[3]);
        assert_eq!(rows[2].get(1).unwrap().as_scalar(), Some(&payload));
    }

    #[test]
    fn every_all_value_checkpoint_preserves_previously_completed_occurrences() {
        let payload = CanonicalPropertyValue::from(CanonicalScalar::bytes(vec![9; 129]).unwrap());
        let mut total = 0;
        collect_values(
            &columns(),
            &[Some(VId(1)), Some(VId(2))],
            &mut ProjectedRows::new(false),
            &mut |_, _| Ok::<_, Failure<usize>>(Some(&payload)),
            &mut |_| {
                total += 1;
                Ok(())
            },
        )
        .unwrap();
        for stop in 1..=total {
            let mut rows = ProjectedRows::new(false);
            collect_values(
                &columns(),
                &[Some(VId(1)), Some(VId(2))],
                &mut rows,
                &mut |_, _| Ok::<_, Failure<usize>>(Some(&payload)),
                &mut |_| Ok(()),
            )
            .unwrap();
            let mut calls = 0;
            let result = collect_values(
                &columns(),
                &[Some(VId(1)), Some(VId(2))],
                &mut rows,
                &mut |_, _| Ok::<_, Failure<usize>>(Some(&payload)),
                &mut |_| {
                    calls += 1;
                    if calls == stop {
                        Err(Failure::Source(stop))
                    } else {
                        Ok(())
                    }
                },
            );
            assert_eq!(result, Err(Failure::Source(stop)));
            assert_eq!(calls, stop);
            assert_eq!(
                rows.len(),
                1,
                "refused occurrence never enters the private collector"
            );
        }
    }

    #[test]
    fn absent_bindings_project_null_without_reading_a_sentinel_vertex() {
        let selected = [
            ValueProjection::Vertex {
                slot: BindingSlot(0),
            },
            ValueProjection::Vertex {
                slot: BindingSlot(1),
            },
            ValueProjection::Property {
                slot: BindingSlot(1),
                key: PropertyKeyId(7),
            },
        ];
        for owner in [VId(0), VId(u128::MAX)] {
            let mut rows = ProjectedRows::new(false);
            collect_values(
                &selected,
                &[Some(owner), None],
                &mut rows,
                &mut |_, _| {
                    Err::<Option<&CanonicalPropertyValue>, _>(Failure::Source(
                        "null binding reached the source",
                    ))
                },
                &mut |_| Ok(()),
            )
            .unwrap();
            let row = rows.first().unwrap();
            assert_eq!(row.get(0).unwrap().as_vertex(), Some(owner));
            assert!(row.get(1).unwrap().is_null());
            assert!(row.get(2).unwrap().is_null());
        }
        let scalar = CanonicalScalar::Int(12);
        let stored = CanonicalPropertyValue::Scalar(scalar.clone());
        let mut calls = 0;
        let mut rows = ProjectedRows::new(false);
        collect_values(
            &selected,
            &[Some(VId(u128::MAX)), Some(VId(0))],
            &mut rows,
            &mut |vid, _| {
                assert_eq!(vid, VId(0));
                calls += 1;
                Ok::<_, Failure<()>>(Some(&stored))
            },
            &mut |_| Ok(()),
        )
        .unwrap();
        assert_eq!(calls, 1);
        let row = rows.first().unwrap();
        assert_eq!(row.get(1).unwrap().as_vertex(), Some(VId(0)));
        assert_eq!(row.get(2).unwrap().as_scalar(), Some(&scalar));
    }
}
