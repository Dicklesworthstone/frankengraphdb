//! Canonical property-value cells and correlated projection rows.
//!
//! Source scalars are borrowed while a candidate key is tested. DISTINCT
//! copies a new value row once; ALL copies each retained matching occurrence.
//! Every copy follows its logical scratch reservations.

use super::{BindingSlot, MAX_PATTERN_VERTICES};
use crate::GlaExecutionEvent;
use crate::algebra_exec::ProjectedRows;
use fgdb_delta_types::PropertyKeyId;
use fgdb_types::{CanonicalScalar, EId, VId};
use std::borrow::Borrow;
use std::cmp::Ordering;

/// An owned traversal, ordered by alternating vertex and edge identities.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GraphPath {
    start: VId,
    steps: Box<[(EId, VId)]>,
}

impl GraphPath {
    pub(crate) fn new(start: VId, steps: Box<[(EId, VId)]>) -> Self {
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
    pub const fn name(self) -> &'a str {
        match self {
            Self::Vertex { name, .. }
            | Self::Property { name, .. }
            | Self::EdgeProperty { name, .. }
            | Self::Path { name, .. } => name,
        }
    }

    #[allow(dead_code)]
    pub(super) const fn variable(self) -> &'a str {
        match self {
            Self::Vertex { variable, .. }
            | Self::Property { variable, .. }
            | Self::EdgeProperty { variable, .. }
            | Self::Path { variable, .. } => variable,
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
                _ => true,
            }
        }
        let mut remaining = Self::MAX_LIST_NODES;
        visit(self, 0, &mut remaining)
    }

    /// Self-delimiting, domain-separated identity bytes. Encoding is iterative;
    /// even externally constructed deep values never recurse on the stack.
    pub fn canonical_bytes(&self) -> Result<Vec<u8>, fgdb_types::ScalarEncodeError> {
        enum Task<'a> {
            Value(&'a GraphValue),
            End(usize),
        }
        let mut bytes = b"fgdb:graph-value:v1\0".to_vec();
        let mut pending = vec![Task::Value(self)];
        while let Some(task) = pending.pop() {
            let value = match task {
                Task::End(at) => {
                    let len = (bytes.len() - at - 8) as u64;
                    bytes[at..at + 8].copy_from_slice(&len.to_be_bytes());
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
                    if let Self::List(values) = value {
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
            Self::Scalar(CanonicalScalar::Text(value)) => [
                value.len(),
                value.canonical_sort_key().map_or(0, <[u8]>::len),
            ],
            Self::Scalar(CanonicalScalar::Bytes(value)) => [value.as_slice().len(), 0],
            Self::Scalar(CanonicalScalar::Timestamp(value)) => {
                [value.zone().map_or(0, |zone| zone.identifier().len()), 0]
            }
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
        let mut bytes = b"fgdb:graph-row:v1\0".to_vec();
        bytes.extend_from_slice(&(self.values.len() as u64).to_be_bytes());
        for value in &self.values {
            let encoded = value.canonical_bytes()?;
            bytes.extend_from_slice(&(encoded.len() as u64).to_be_bytes());
            bytes.extend_from_slice(&encoded);
        }
        Ok(bytes)
    }
    /// Compiler-owned relational operators call this only after schema checks
    /// and per-cell/payload reservations. It is not a public unchecked row API.
    pub(crate) fn from_owned_values(values: Vec<GraphValue>) -> Self {
        debug_assert!(!values.is_empty() && values.len() <= MAX_PATTERN_VERTICES);
        Self {
            values: values.into_boxed_slice(),
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

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum ValueRef<'a> {
    Scalar(&'a CanonicalScalar),
    Vertex(VId),
    Path(&'a GraphPath),
    Vertices(&'a [VId]),
    Edges(&'a [EId]),
    Edge(EId),
    List(&'a [GraphValue]),
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
        match &self.values[at] {
            GraphValue::Scalar(value) => ValueRef::Scalar(value),
            GraphValue::Vertex(value) => ValueRef::Vertex(*value),
            GraphValue::Path(value) => ValueRef::Path(value),
            GraphValue::Vertices(value) => ValueRef::Vertices(value),
            GraphValue::Edges(value) => ValueRef::Edges(value),
            GraphValue::Edge(value) => ValueRef::Edge(*value),
            GraphValue::List(value) => ValueRef::List(value),
        }
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

pub(super) fn collect_values<'a, E>(
    columns: &[ValueProjection],
    bindings: &[Option<VId>],
    projected: &mut ProjectedRows<GraphValueRow>,
    property: &mut impl FnMut(VId, PropertyKeyId) -> Result<Option<&'a CanonicalScalar>, E>,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<(), E> {
    collect_values_with_paths(columns, bindings, &[], projected, property, control)
}

pub(super) fn collect_values_with_paths<'a, E>(
    columns: &[ValueProjection],
    bindings: &[Option<VId>],
    paths: &[Option<GraphPath>],
    projected: &mut ProjectedRows<GraphValueRow>,
    property: &mut impl FnMut(VId, PropertyKeyId) -> Result<Option<&'a CanonicalScalar>, E>,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<(), E> {
    collect_values_with_element_properties(
        columns,
        bindings,
        paths,
        projected,
        property,
        &mut |_, _| panic!("edge properties require an explicit edge property source"),
        &mut |_| panic!("vertex labels require an explicit label source"),
        &mut |_| panic!("edge type requires an explicit edge source"),
        control,
    )
}

#[allow(clippy::too_many_arguments)]
pub(super) fn collect_values_with_element_properties<'a, E>(
    columns: &[ValueProjection],
    bindings: &[Option<VId>],
    paths: &[Option<GraphPath>],
    projected: &mut ProjectedRows<GraphValueRow>,
    property: &mut impl FnMut(VId, PropertyKeyId) -> Result<Option<&'a CanonicalScalar>, E>,
    edge_property: &mut impl FnMut(EId, PropertyKeyId) -> Result<Option<&'a CanonicalScalar>, E>,
    vertex_labels: &mut impl FnMut(VId) -> Result<Option<&'a [GraphValue]>, E>,
    edge_type: &mut impl FnMut(EId) -> Result<Option<&'a CanonicalScalar>, E>,
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
            GraphPathFunction::Labels | GraphPathFunction::Type => None,
        };
    }
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
                ValueRef::Scalar(value.unwrap_or(&null))
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
                ValueRef::Scalar(value.unwrap_or(&null))
            }
            ValueProjection::Labels { slot } => match bindings[slot.ordinal() as usize] {
                Some(vid) => match vertex_labels(vid)? {
                    Some(labels) => ValueRef::List(labels),
                    None => ValueRef::List(&[]),
                },
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
        };
        for _ in 0..key[at].payload_units() {
            control(GlaExecutionEvent::Work)?;
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
        if computed[at].is_none() {
            copied[at] = Some(key[at].into_owned());
        }
    }
    for at in 0..columns.len() {
        values.push(
            copied[at]
                .take()
                .or_else(|| computed[at].take())
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

    #[test]
    fn bounded_values_read_rejected_payloads_without_copying_them() {
        let payload = CanonicalScalar::bytes(vec![3; 4096]).unwrap();
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
                    Ok::<_, ()>(Some(&payload))
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
                &mut |_, _| Err::<Option<&CanonicalScalar>, _>("late property failure"),
                &mut |_| Ok(())
            ),
            Err("late property failure")
        );
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn refused_replacement_keeps_the_previous_complete_value_row() {
        let payload = CanonicalScalar::bytes(vec![1; 129]).unwrap();
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
                &mut |_, _| Ok::<_, usize>(Some(&payload)),
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
            &mut |_, _| Ok::<_, usize>(Some(&payload)),
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
                &mut |_, _| Ok::<_, usize>(Some(&payload)),
                &mut |_| {
                    calls += 1;
                    if calls == stop { Err(stop) } else { Ok(()) }
                },
            );
            assert_eq!(result, Err(stop));
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
        let null = CanonicalScalar::Null;
        let mut rows = ProjectedRows::new(true);
        for (owner, present) in [(1, false), (1, true), (2, false)] {
            collect_values(
                &columns(),
                &[Some(VId(owner)), Some(VId(4))],
                &mut rows,
                &mut |_, _| Ok::<_, ()>(present.then_some(&null)),
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
        let mut rows = ProjectedRows::new(true);
        let mut scratch = 0;
        for _ in 0..2 {
            collect_values(
                &columns(),
                &[Some(VId(1)), Some(VId(2))],
                &mut rows,
                &mut |_, _| Ok::<_, ()>(Some(&payload)),
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
        let payload = CanonicalScalar::ucs_basic_text(&"x".repeat(129)).unwrap();
        let mut total = 0;
        collect_values(
            &columns(),
            &[Some(VId(1)), Some(VId(2))],
            &mut ProjectedRows::new(true),
            &mut |_, _| Ok::<_, usize>(Some(&payload)),
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
                &mut |_, _| Ok::<_, usize>(Some(&payload)),
                &mut |_| {
                    calls += 1;
                    if calls == stop { Err(stop) } else { Ok(()) }
                },
            );
            assert_eq!(result, Err(stop));
            assert_eq!(calls, stop);
            assert!(rows.is_empty());
        }
        let mut rows = ProjectedRows::new(true);
        assert_eq!(
            collect_values(
                &columns(),
                &[Some(VId(1)), Some(VId(2))],
                &mut rows,
                &mut |_, _| Err::<Option<&CanonicalScalar>, _>("source"),
                &mut |_| Ok(())
            ),
            Err("source")
        );
        assert!(rows.is_empty());
    }

    #[test]
    fn all_value_rows_charge_each_payload_and_keep_null_occurrences() {
        let payload = CanonicalScalar::bytes(vec![7; 129]).unwrap();
        let mut rows = ProjectedRows::new(false);
        let mut scratch = 0;
        for present in [true, false, true, false] {
            collect_values(
                &columns(),
                &[Some(VId(1)), Some(VId(2))],
                &mut rows,
                &mut |_, _| Ok::<_, ()>(present.then_some(&payload)),
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
        let payload = CanonicalScalar::bytes(vec![9; 129]).unwrap();
        let mut total = 0;
        collect_values(
            &columns(),
            &[Some(VId(1)), Some(VId(2))],
            &mut ProjectedRows::new(false),
            &mut |_, _| Ok::<_, usize>(Some(&payload)),
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
                &mut |_, _| Ok::<_, usize>(Some(&payload)),
                &mut |_| Ok(()),
            )
            .unwrap();
            let mut calls = 0;
            let result = collect_values(
                &columns(),
                &[Some(VId(1)), Some(VId(2))],
                &mut rows,
                &mut |_, _| Ok::<_, usize>(Some(&payload)),
                &mut |_| {
                    calls += 1;
                    if calls == stop { Err(stop) } else { Ok(()) }
                },
            );
            assert_eq!(result, Err(stop));
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
                &mut |_, _| Err::<Option<&CanonicalScalar>, _>("null binding reached the source"),
                &mut |_| Ok(()),
            )
            .unwrap();
            let row = rows.first().unwrap();
            assert_eq!(row.get(0).unwrap().as_vertex(), Some(owner));
            assert!(row.get(1).unwrap().is_null());
            assert!(row.get(2).unwrap().is_null());
        }
        let scalar = CanonicalScalar::Int(12);
        let mut calls = 0;
        let mut rows = ProjectedRows::new(false);
        collect_values(
            &selected,
            &[Some(VId(u128::MAX)), Some(VId(0))],
            &mut rows,
            &mut |vid, _| {
                assert_eq!(vid, VId(0));
                calls += 1;
                Ok::<_, ()>(Some(&scalar))
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
