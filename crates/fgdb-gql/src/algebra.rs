//! Typed GLA lowering for the existing bounded MATCH language.
//!
//! The default output is a sorted distinct vertex-ID set. Typed graph-pattern
//! value plans additionally support bags and scoped nullable bindings. This
//! is not the registered FreeJoin physical implementation.

mod boolean;
mod existence;
mod ordering;
mod output;
mod pattern;
mod predicate;
mod values;
pub use boolean::{
    BoundBooleanExpression, GraphBooleanError, GraphBooleanExpression, GraphBooleanOp,
    GraphBooleanOperand, MAX_BOOLEAN_INSTRUCTIONS,
};
pub use existence::{GraphExistence, GraphMatchClause};
pub use ordering::{GraphOrderError, GraphValueOrder};
pub use output::{GlaIdentityOutput, GlaOutput, GraphBindingRow};
pub use pattern::{
    GraphPatternBuilder, MAX_PATTERN_BINDINGS, MAX_PATTERN_EDGES, MAX_PATTERN_IDENTITIES,
    MAX_PATTERN_NAME_BYTES, MAX_PATTERN_PREDICATES, MAX_PATTERN_VERTICES, PatternBuildError,
    PatternLimitDimension, PreparedGraphPattern,
};
pub use predicate::{MAX_SCALAR_PREDICATE_BYTES, ScalarPredicate, ScalarPredicateError};
pub use values::{
    GRAPH_VALUE_PAYLOAD_UNIT_BYTES, GraphColumn, GraphPath, GraphPathFunction, GraphValue,
    GraphValueRow, ValueProjection,
};
pub(crate) use values::{RowKey, ValueRef};

use crate::{BoundPlan, EdgeDirection, ReturnProjection};
use core::marker::PhantomData;
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_types::{CanonicalScalar, VId};

/// An ordinal in the binding table, independent of a parser variable's name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct BindingSlot(pub(crate) u32);

impl BindingSlot {
    #[must_use]
    pub const fn ordinal(self) -> u32 {
        self.0
    }
}

/// Physical orientation after the bounded binder's one-hop normalization.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum GlaDirection {
    Forward,
    Reverse,
    Undirected,
}

/// Search semantics of one finite path atom, not a terminal row quantifier.
/// ALL and repetition-restricted modes retain edge-occurrence multiplicity;
/// ANY selects one occurrence per endpoint pair. DISTINCT remains a separate
/// output operation. Captured atoms retain the selected real edge identities.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphWalkSearch {
    /// Every walk whose length lies in the admitted interval.
    All,
    /// Every tied minimum-length walk in that interval for each endpoint pair.
    /// A lower bound delays settlement; it is not a filter applied after an
    /// unrestricted shortest-path search. Each atom selects independently of
    /// surrounding joins. No weighted or repetition-restricted search is implied.
    AllShortest,
    /// One minimum-length occurrence within the interval per endpoint pair.
    /// Equal-depth prefixes coalesce before expansion, not after enumerating
    /// all ties. Surrounding binding occurrences still multiply independently.
    AnyShortest,
    /// Every path without a repeated vertex, including the starting vertex.
    /// Membership is path-local; distinct routes to an endpoint remain distinct.
    Acyclic,
    /// Every path whose vertices are distinct except that its last vertex may
    /// equal its first. A closing return is terminal, including a self-loop.
    /// This is vertex restriction, not edge-unique TRAIL or shortest selection.
    Simple,
    /// Every path without a repeated EId. Vertices, including the source, may
    /// repeat and closed trails may continue. Real edge identities are needed
    /// even when the output contains only endpoints. No shortest selection or
    /// edge uniqueness across separate atoms is implied.
    Trail,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IntegerComparison {
    Equal,
    NotEqual,
    Greater,
    Less,
    GreaterOrEqual,
    LessOrEqual,
}

impl IntegerComparison {
    #[must_use]
    pub const fn accepts(self, actual: i64, expected: i64) -> bool {
        match self {
            Self::Equal => actual == expected,
            Self::NotEqual => actual != expected,
            Self::Greater => actual > expected,
            Self::Less => actual < expected,
            Self::GreaterOrEqual => actual >= expected,
            Self::LessOrEqual => actual <= expected,
        }
    }

    fn tag(self) -> u8 {
        match self {
            Self::Equal => 0,
            Self::NotEqual => 1,
            Self::Greater => 2,
            Self::Less => 3,
            Self::GreaterOrEqual => 4,
            Self::LessOrEqual => 5,
        }
    }
}

/// Ordinary comparisons reject missing, null and incompatible scalar kinds.
/// PropertyNull explicitly tests missing/stored null without conflating errors.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VertexPredicate {
    HasLabel(LabelId),
    IntegerProperty {
        key: PropertyKeyId,
        comparison: IntegerComparison,
        value: i64,
    },
    ScalarProperty {
        key: PropertyKeyId,
        predicate: ScalarPredicate,
    },
    PropertyNull {
        key: PropertyKeyId,
        is_null: bool,
    },
}

impl VertexPredicate {
    #[must_use]
    pub fn matches(
        &self,
        labels: &[LabelId],
        properties: &[(PropertyKeyId, CanonicalScalar)],
    ) -> bool {
        self.matches_borrowed(
            labels.iter().copied(),
            properties.iter().map(|(key, value)| (*key, value)),
        )
    }

    /// Evaluate borrowed canonical fields without cloning scalar payloads.
    #[must_use]
    pub fn matches_borrowed<'a>(
        &self,
        labels: impl IntoIterator<Item = LabelId>,
        properties: impl IntoIterator<Item = (PropertyKeyId, &'a CanonicalScalar)>,
    ) -> bool {
        match self {
            Self::HasLabel(label) => labels.into_iter().any(|actual| actual == *label),
            Self::IntegerProperty {
                key,
                comparison,
                value,
            } => properties.into_iter().any(|(actual_key, scalar)| {
                actual_key == *key
                    && comparison
                        .accepts_scalar_pair(Some(scalar), Some(&CanonicalScalar::Int(*value)))
            }),
            Self::ScalarProperty { key, predicate } => {
                let actual = properties
                    .into_iter()
                    .find(|(actual_key, _)| actual_key == key)
                    .map(|(_, value)| value);
                predicate.matches(actual)
            }
            Self::PropertyNull { key, is_null } => {
                let actual = properties
                    .into_iter()
                    .find(|(actual_key, _)| actual_key == key)
                    .map(|(_, value)| value);
                actual.is_none_or(|value| matches!(value, CanonicalScalar::Null)) == *is_null
            }
        }
    }
}

/// Typed GLA operators. Scoped left/semi/anti joins have compiler-owned
/// boundaries and binding slots; callers cannot construct executable plans.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GlaOperator {
    Empty,
    ScanVertices,
    ScanEdges {
        relation: RelationId,
        direction: GlaDirection,
    },
    Select {
        slot: BindingSlot,
        predicates: Vec<VertexPredicate>,
    },
    VertexIdentity {
        left: BindingSlot,
        right: BindingSlot,
        equal: bool,
    },
    Expand {
        source: BindingSlot,
        relation: RelationId,
        direction: GlaDirection,
    },
    /// Append an endpoint for each selected path occurrence in the inclusive
    /// finite interval. Search selects WALK, shortest WALK, ACYCLIC, SIMPLE or TRAIL.
    /// Endpoint predicates apply after expansion, not to transit vertices.
    /// A CapturePath operator may retain the real identified route. Repetition
    /// restrictions apply to this atom, not to a surrounding compound path.
    VarLengthExpand {
        source: BindingSlot,
        relation: RelationId,
        direction: GlaDirection,
        bounds: crate::GraphWalkBounds,
        search: GraphWalkSearch,
    },
    /// Evaluate the enclosed binding scope once per outer occurrence. The
    /// first complete witness resolves the predicate; it is not an output row.
    Probe {
        group: u32,
        end: u32,
        anti: bool,
    },
    ProbeEnd {
        group: u32,
    },
    /// Correlated left join. `slots` is the complete inner frame width,
    /// including copies of correlated bindings. No match extends that frame
    /// with nulls exactly once; the projection retains original outer slots.
    Optional {
        group: u32,
        end: u32,
        slots: u32,
    },
    /// Match success is recorded here, before executing any later clause.
    OptionalEnd {
        group: u32,
    },
    /// Copy a correlated identity into the inner scope, without scanning rows.
    BindVertex {
        source: BindingSlot,
    },
    /// Copy an outer predicate operand, INCLUDING a null binding. Unlike
    /// BindVertex, this is a value capture, not a positive node match. It never
    /// scans a vertex or replaces null with a fabricated graph identity.
    BindOuterVertex {
        source: BindingSlot,
    },
    Project {
        slot: BindingSlot,
    },
    Distinct,
    OrderByVertexId,
    Limit {
        offset: u64,
        count: Option<u64>,
    },
    /// Column order is semantic. Distinctness applies to the entire tuple.
    ProjectBindings {
        slots: Vec<BindingSlot>,
    },
    OrderByBindings,
    /// Exact canonical properties and IDs from the same complete binding.
    ProjectValues {
        columns: Vec<ValueProjection>,
    },
    OrderByValues,
    /// Prepared output-column ordering, with canonical whole-row tie-breaking.
    /// Shared immutable metadata is allocated during preparation, not per row.
    OrderByValueColumns {
        columns: std::sync::Arc<[GraphValueOrder]>,
    },
    /// A binding-dependent selection inside the current positive MATCH scope.
    /// Both operands use the admitted property source. Unlike Select, its
    /// result cannot be cached under only one vertex identity.
    CompareProperties {
        left: BindingSlot,
        left_key: PropertyKeyId,
        right: BindingSlot,
        right_key: PropertyKeyId,
        comparison: IntegerComparison,
    },
    /// A checked three-valued expression over the complete current binding.
    /// Only TRUE continues; UNKNOWN is not Boolean false under negation.
    SelectBoolean {
        expression: BoundBooleanExpression,
    },
    /// Assemble the ordered identified expansion segments of one captured path.
    CapturePath {
        capture: u32,
        start: BindingSlot,
        segments: Vec<BindingSlot>,
    },
    SelectPathLength {
        capture: u32,
        comparison: IntegerComparison,
        value: i64,
    },
    SelectPathNull {
        capture: u32,
        function: GraphPathFunction,
        is_null: bool,
    },
}

/// The vertices a plan's vertex scans can bind. An edge root or expansion
/// binds only endpoints of edges in [`GlaPlan::edge_relations`], so the edge
/// reads witness those vertices, not this domain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VertexScanDomain {
    /// No vertex scan: every bound vertex is an endpoint of a read edge.
    Unscanned,
    /// One root scan whose conjunctive selection requires this label.
    Label(LabelId),
    /// Any vertex can be bound.
    All,
}

impl VertexScanDomain {
    /// Whether a vertex carrying `labels` can be bound by a vertex scan.
    #[must_use]
    pub fn admits(self, labels: &[LabelId]) -> bool {
        match self {
            Self::Unscanned => false,
            Self::Label(label) => labels.contains(&label),
            Self::All => true,
        }
    }
}

/// Immutable logical definition with a statically determined output row shape.
/// Existing language and API calls retain their default `VId` result type.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GlaPlan<Row = VId> {
    operators: Vec<GlaOperator>,
    pub(crate) visible_columns: Option<usize>,
    pub reverse_catalog: Option<std::sync::Arc<crate::graph_text::ReverseSymbolCatalog>>,
    output: PhantomData<fn() -> Row>,
}

fn predicates(
    label: Option<LabelId>,
    properties: [Option<(PropertyKeyId, i64)>; 6],
) -> Vec<VertexPredicate> {
    let mut result = Vec::new();
    if let Some(label) = label {
        result.push(VertexPredicate::HasLabel(label));
    }
    let comparisons = [
        IntegerComparison::Equal,
        IntegerComparison::NotEqual,
        IntegerComparison::Greater,
        IntegerComparison::Less,
        IntegerComparison::GreaterOrEqual,
        IntegerComparison::LessOrEqual,
    ];
    for (property, comparison) in properties.into_iter().zip(comparisons) {
        if let Some((key, value)) = property {
            result.push(VertexPredicate::IntegerProperty {
                key,
                comparison,
                value,
            });
        }
    }
    result
}

fn select(operators: &mut Vec<GlaOperator>, slot: BindingSlot, predicates: Vec<VertexPredicate>) {
    if !predicates.is_empty() {
        operators.push(GlaOperator::Select { slot, predicates });
    }
}

fn bind_alias(
    operators: &mut Vec<GlaOperator>,
    previous: &[(BindingSlot, &str)],
    slot: BindingSlot,
    name: &str,
) {
    if let Some((representative, _)) = previous.iter().find(|(_, bound)| *bound == name) {
        operators.push(GlaOperator::VertexIdentity {
            left: *representative,
            right: slot,
            equal: true,
        });
    }
}

impl GlaPlan {
    /// Normalize the legacy positional representation once, at this boundary.
    #[must_use]
    pub fn lower(plan: &BoundPlan) -> Self {
        let source = BindingSlot(0);
        let destination = BindingSlot(1);
        let far_end = BindingSlot(2);
        let mut source_predicates = predicates(
            plan.src_label,
            [
                plan.src_prop,
                plan.src_prop_ne,
                plan.src_prop_gt,
                plan.src_prop_lt,
                plan.src_prop_ge,
                plan.src_prop_le,
            ],
        );
        let mut operators = Vec::new();
        let projection = if let Some(relation) = plan.relation {
            let reverse = plan.direction == EdgeDirection::Incoming && plan.hop2_relation.is_some();
            let direction = match plan.direction {
                EdgeDirection::Undirected => GlaDirection::Undirected,
                EdgeDirection::Incoming if reverse => GlaDirection::Reverse,
                EdgeDirection::Incoming | EdgeDirection::Outgoing => GlaDirection::Forward,
            };
            operators.push(GlaOperator::ScanEdges {
                relation,
                direction,
            });
            bind_alias(
                &mut operators,
                &[(source, plan.src_var.as_str())],
                destination,
                &plan.dst_var,
            );
            let destination_properties = predicates(
                None,
                [
                    plan.dst_prop,
                    plan.dst_prop_ne,
                    plan.dst_prop_gt,
                    plan.dst_prop_lt,
                    plan.dst_prop_ge,
                    plan.dst_prop_le,
                ],
            );
            let mut destination_predicates = predicates(plan.dst_label, [None; 6]);
            // Retain the bounded incoming-two-hop property-role contract.
            if reverse {
                source_predicates.extend(destination_properties);
            } else {
                destination_predicates.extend(destination_properties);
            }
            select(&mut operators, source, source_predicates);
            select(&mut operators, destination, destination_predicates);
            for (present, equal) in [(plan.neq.is_some(), false), (plan.eq.is_some(), true)] {
                if present {
                    operators.push(GlaOperator::VertexIdentity {
                        left: source,
                        right: destination,
                        equal,
                    });
                }
            }
            if let Some(relation) = plan.hop2_relation {
                operators.push(GlaOperator::Expand {
                    source: destination,
                    relation,
                    direction,
                });
                if let Some(name) = &plan.hop2_dst_var {
                    bind_alias(
                        &mut operators,
                        &[
                            (source, plan.src_var.as_str()),
                            (destination, plan.dst_var.as_str()),
                        ],
                        far_end,
                        name,
                    );
                }
                select(
                    &mut operators,
                    far_end,
                    predicates(
                        None,
                        [
                            plan.hop2_dst_prop,
                            plan.hop2_dst_prop_ne,
                            plan.hop2_dst_prop_gt,
                            plan.hop2_dst_prop_lt,
                            plan.hop2_dst_prop_ge,
                            plan.hop2_dst_prop_le,
                        ],
                    ),
                );
            }
            match plan.projection {
                ReturnProjection::Source => source,
                ReturnProjection::Destination => destination,
                ReturnProjection::Hop2Destination if plan.hop2_relation.is_some() => far_end,
                ReturnProjection::Hop2Destination => destination,
            }
        } else {
            operators.push(if plan.src_label.is_some() {
                GlaOperator::ScanVertices
            } else {
                GlaOperator::Empty
            });
            select(&mut operators, source, source_predicates);
            source
        };
        operators.extend([
            GlaOperator::Project { slot: projection },
            GlaOperator::Distinct,
            GlaOperator::OrderByVertexId,
            GlaOperator::Limit {
                offset: plan.skip.unwrap_or(0),
                count: plan.limit,
            },
        ]);
        Self::from_operators(operators)
    }
}

impl<Row> GlaPlan<Row> {
    // This stays private to the algebra compiler and its child modules. Row and
    // terminal operator shape must be chosen together, not supplied by callers.
    pub(crate) fn from_operators(mut operators: Vec<GlaOperator>) -> Self {
        // WALK and independent scans need the complete vertex domain, not just
        // requested edge endpoints. Normalize an edge root onto the existing
        // two-table admission path so isolates, intermediate values, snapshot
        // budgets and transaction scan observations all share the same source.
        // Expanding the root adds no binding slot: ScanEdges already bound two.
        let mut probe_depth = 0usize;
        let mut normalize = false;
        for op in &operators {
            match op {
                GlaOperator::Probe { .. } => probe_depth += 1,
                GlaOperator::ProbeEnd { .. } => {
                    probe_depth = probe_depth.saturating_sub(1);
                }
                GlaOperator::VarLengthExpand { .. } | GlaOperator::ScanVertices
                    if probe_depth == 0 =>
                {
                    normalize = true;
                    break;
                }
                _ => {}
            }
        }
        if normalize
            && let Some(GlaOperator::ScanEdges {
                relation,
                direction,
            }) = operators.first().cloned()
        {
            operators[0] = GlaOperator::ScanVertices;
            operators.insert(
                1,
                GlaOperator::Expand {
                    source: BindingSlot(0),
                    relation,
                    direction,
                },
            );
            // Every scope is after the root. Keep compiler-owned jump
            // targets aligned with the one newly inserted instruction.
            for op in &mut operators {
                match op {
                    GlaOperator::Probe { end, .. } | GlaOperator::Optional { end, .. } => *end += 1,
                    _ => {}
                }
            }
        }
        Self {
            operators,
            visible_columns: None,
            reverse_catalog: None,
            output: PhantomData,
        }
    }

    #[must_use]
    pub fn operators(&self) -> &[GlaOperator] {
        &self.operators
    }

    #[must_use]
    pub fn scans_edges(&self) -> bool {
        matches!(self.operators.first(), Some(GlaOperator::ScanEdges { .. }))
    }

    /// An outer vertex scan may also need topology for correlated predicates.
    /// This is distinct from the root scan that determines the outer rows.
    #[must_use]
    pub fn reads_edges(&self) -> bool {
        self.operators.iter().any(|operator| {
            matches!(
                operator,
                GlaOperator::ScanEdges { .. }
                    | GlaOperator::Expand { .. }
                    | GlaOperator::VarLengthExpand { .. }
            )
        })
    }

    /// Every relation whose edges this plan can read. Each edge operator
    /// names exactly one relation, so no edge of another relation can
    /// change this plan's rows. That is what lets a transaction record a
    /// per-relation read witness instead of the whole edge table
    /// (fgdb-whole-edge-read-flag-4qe1z). Empty when the plan reads no edges.
    #[must_use]
    pub fn edge_relations(&self) -> std::collections::BTreeSet<RelationId> {
        self.operators
            .iter()
            .filter_map(|operator| match operator {
                GlaOperator::ScanEdges { relation, .. }
                | GlaOperator::Expand { relation, .. }
                | GlaOperator::VarLengthExpand { relation, .. } => Some(*relation),
                _ => None,
            })
            .collect()
    }

    /// Which vertices this plan's vertex scans can bind (fgdb-h1d6l). Slot 0
    /// is always the root, because a nested scope offsets its slots past the
    /// outer frame, so a label selected on slot 0 filters every row. Any
    /// other vertex scan (an independent component, or a scope's
    /// uncorrelated root) can bind any vertex.
    #[must_use]
    pub fn vertex_scan_domain(&self) -> VertexScanDomain {
        let mut scans = self
            .operators
            .iter()
            .enumerate()
            .filter(|(_, operator)| matches!(operator, GlaOperator::ScanVertices));
        match (scans.next(), scans.next()) {
            (None, _) => VertexScanDomain::Unscanned,
            (Some((0, _)), None) => self
                .operators
                .iter()
                .find_map(|operator| match operator {
                    GlaOperator::Select { slot, predicates } if slot.ordinal() == 0 => {
                        predicates.iter().find_map(|predicate| match predicate {
                            VertexPredicate::HasLabel(label) => Some(*label),
                            _ => None,
                        })
                    }
                    _ => None,
                })
                .map_or(VertexScanDomain::All, VertexScanDomain::Label),
            _ => VertexScanDomain::All,
        }
    }

    /// Captures and edge-unique traversal require actual source EIds. TRAIL
    /// cannot drop its identities merely because only endpoints are projected.
    #[must_use]
    pub fn requires_identified_edges(&self) -> bool {
        self.operators.iter().any(|op| {
            matches!(
                op,
                GlaOperator::CapturePath { .. }
                    | GlaOperator::VarLengthExpand {
                        search: GraphWalkSearch::Trail,
                        ..
                    }
            )
        })
    }

    /// Scalar property projections require a real vertex source even without
    /// a predicate. Edge-property plans additionally need the element accessor.
    #[must_use]
    pub fn projects_properties(&self) -> bool {
        self.operators.iter().any(|operator| match operator {
            GlaOperator::ProjectValues { columns } => columns
                .iter()
                .any(|column| matches!(column, ValueProjection::Property { .. })),
            _ => false,
        })
    }

    /// Captured relationship payloads need the explicit edge property source.
    #[must_use]
    pub fn projects_edge_properties(&self) -> bool {
        self.operators.iter().any(|operator| match operator {
            GlaOperator::ProjectValues { columns } => columns
                .iter()
                .any(|column| matches!(column, ValueProjection::EdgeProperty { .. })),
            GlaOperator::SelectBoolean { expression } => expression.contains_edge_property(),
            _ => false,
        })
    }

    #[must_use]
    pub fn projects_labels(&self) -> bool {
        self.operators.iter().any(|operator| match operator {
            GlaOperator::ProjectValues { columns } => columns
                .iter()
                .any(|column| matches!(column, ValueProjection::Labels { .. })),
            _ => false,
        })
    }

    #[must_use]
    pub fn projects_types(&self) -> bool {
        self.operators.iter().any(|operator| match operator {
            GlaOperator::ProjectValues { columns } => columns
                .iter()
                .any(|column| matches!(column, ValueProjection::Type { .. })),
            _ => false,
        })
    }

    /// The binding slots whose vertex rows (labels or properties) this plan
    /// reads, or `None` when that cannot be stated slot by slot (a Boolean
    /// expression or a path projection may read any bound vertex).
    #[must_use]
    pub fn vertex_value_slots(&self) -> Option<std::collections::BTreeSet<u32>> {
        let mut slots = std::collections::BTreeSet::new();
        for operator in &self.operators {
            match operator {
                GlaOperator::Select { slot, predicates } if !predicates.is_empty() => {
                    slots.insert(slot.ordinal());
                }
                GlaOperator::CompareProperties { left, right, .. } => {
                    slots.insert(left.ordinal());
                    slots.insert(right.ordinal());
                }
                GlaOperator::SelectBoolean { .. } => return None,
                GlaOperator::ProjectValues { columns } => {
                    for column in columns {
                        match column {
                            ValueProjection::Property { slot, .. }
                            | ValueProjection::Labels { slot } => {
                                slots.insert(slot.ordinal());
                            }
                            ValueProjection::Path { .. } => return None,
                            ValueProjection::Vertex { .. }
                            | ValueProjection::EdgeProperty { .. }
                            | ValueProjection::Type { .. } => {}
                        }
                    }
                }
                _ => {}
            }
        }
        Some(slots)
    }

    #[must_use]
    pub fn needs_vertex_values(&self) -> bool {
        self.projects_properties()
            || self.projects_labels()
            || self.operators.iter().any(|operator| {
                matches!(
                    operator,
                    GlaOperator::Select { .. }
                        | GlaOperator::CompareProperties { .. }
                        | GlaOperator::SelectBoolean { .. }
                )
            })
    }
    /// Application transcript, not an Appendix A durable format. Existing
    /// scalar tags/bytes are unchanged. Tuple projection and ordering have
    /// distinct tags, so neither column order nor tuple identity is erased.
    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut bytes = b"fgdb:bounded-gla:v1\0".to_vec();
        bytes.extend_from_slice(&(self.operators.len() as u64).to_be_bytes());
        for operator in &self.operators {
            match operator {
                GlaOperator::Empty => bytes.push(0),
                GlaOperator::ScanVertices => bytes.push(1),
                GlaOperator::ScanEdges {
                    relation,
                    direction,
                } => {
                    bytes.push(2);
                    bytes.extend_from_slice(&relation.0.to_be_bytes());
                    bytes.push(direction_tag(*direction));
                }
                GlaOperator::Select { slot, predicates } => {
                    bytes.push(3);
                    bytes.extend_from_slice(&slot.0.to_be_bytes());
                    bytes.extend_from_slice(&(predicates.len() as u64).to_be_bytes());
                    for predicate in predicates {
                        match predicate {
                            VertexPredicate::HasLabel(label) => {
                                bytes.push(0);
                                bytes.extend_from_slice(&label.0.to_be_bytes());
                            }
                            VertexPredicate::IntegerProperty {
                                key,
                                comparison,
                                value,
                            } => {
                                bytes.push(1);
                                bytes.extend_from_slice(&key.0.to_be_bytes());
                                bytes.push(comparison.tag());
                                bytes.extend_from_slice(&value.to_be_bytes());
                            }
                            VertexPredicate::ScalarProperty { key, predicate } => {
                                bytes.push(2);
                                bytes.extend_from_slice(&key.0.to_be_bytes());
                                predicate.append_transcript(&mut bytes);
                            }
                            VertexPredicate::PropertyNull { key, is_null } => {
                                bytes.push(3);
                                bytes.extend_from_slice(&key.0.to_be_bytes());
                                bytes.push(u8::from(*is_null));
                            }
                        }
                    }
                }
                GlaOperator::VertexIdentity { left, right, equal } => {
                    bytes.push(4);
                    bytes.extend_from_slice(&left.0.to_be_bytes());
                    bytes.extend_from_slice(&right.0.to_be_bytes());
                    bytes.push(u8::from(*equal));
                }
                GlaOperator::Expand {
                    source,
                    relation,
                    direction,
                } => {
                    bytes.push(5);
                    bytes.extend_from_slice(&source.0.to_be_bytes());
                    bytes.extend_from_slice(&relation.0.to_be_bytes());
                    bytes.push(direction_tag(*direction));
                }
                GlaOperator::VarLengthExpand {
                    source,
                    relation,
                    direction,
                    bounds,
                    search,
                } => {
                    // Preserve existing transcripts byte-for-byte. Every
                    // selector has its own tag even when outputs coincide.
                    bytes.push(match search {
                        GraphWalkSearch::All => 22,
                        GraphWalkSearch::AllShortest => 23,
                        GraphWalkSearch::AnyShortest => 24,
                        GraphWalkSearch::Acyclic => 29,
                        GraphWalkSearch::Simple => 30,
                        GraphWalkSearch::Trail => 31,
                    });
                    bytes.extend_from_slice(&source.0.to_be_bytes());
                    bytes.extend_from_slice(&relation.0.to_be_bytes());
                    bytes.push(direction_tag(*direction));
                    bytes.extend_from_slice(&bounds.minimum().to_be_bytes());
                    bytes.extend_from_slice(&bounds.maximum().to_be_bytes());
                }
                GlaOperator::Project { slot } => {
                    bytes.push(6);
                    bytes.extend_from_slice(&slot.0.to_be_bytes());
                }
                GlaOperator::Distinct => bytes.push(7),
                GlaOperator::OrderByVertexId => bytes.push(8),
                GlaOperator::Limit { offset, count } => {
                    bytes.push(9);
                    bytes.extend_from_slice(&offset.to_be_bytes());
                    bytes.push(u8::from(count.is_some()));
                    if let Some(count) = count {
                        bytes.extend_from_slice(&count.to_be_bytes());
                    }
                }
                GlaOperator::ProjectBindings { slots } => {
                    bytes.push(10);
                    bytes.extend_from_slice(&(slots.len() as u64).to_be_bytes());
                    for slot in slots {
                        bytes.extend_from_slice(&slot.0.to_be_bytes());
                    }
                }
                GlaOperator::OrderByBindings => bytes.push(11),
                GlaOperator::ProjectValues { columns } => {
                    bytes.push(12);
                    bytes.extend_from_slice(&(columns.len() as u64).to_be_bytes());
                    for column in columns {
                        match column {
                            ValueProjection::Vertex { slot } => {
                                bytes.push(0);
                                bytes.extend_from_slice(&slot.0.to_be_bytes());
                            }
                            ValueProjection::Property { slot, key } => {
                                bytes.push(1);
                                bytes.extend_from_slice(&slot.0.to_be_bytes());
                                bytes.extend_from_slice(&key.0.to_be_bytes());
                            }
                            ValueProjection::EdgeProperty { capture, key } => {
                                bytes.push(3);
                                bytes.extend_from_slice(&capture.to_be_bytes());
                                bytes.extend_from_slice(&key.0.to_be_bytes());
                            }
                            ValueProjection::Labels { slot } => {
                                bytes.push(4);
                                bytes.extend_from_slice(&slot.0.to_be_bytes());
                            }
                            ValueProjection::Type { capture } => {
                                bytes.push(5);
                                bytes.extend_from_slice(&capture.to_be_bytes());
                            }
                            ValueProjection::Path { capture, function } => {
                                bytes.push(2);
                                bytes.extend_from_slice(&capture.to_be_bytes());
                                bytes.push(*function as u8);
                            }
                        }
                    }
                }
                GlaOperator::OrderByValues => bytes.push(13),
                GlaOperator::OrderByValueColumns { columns } => {
                    bytes.push(20);
                    bytes.extend_from_slice(&(columns.len() as u64).to_be_bytes());
                    for column in columns.iter() {
                        bytes.extend_from_slice(&(column.column as u64).to_be_bytes());
                        bytes.push(u8::from(column.descending));
                        bytes.push(u8::from(column.nulls_first));
                    }
                }
                GlaOperator::Probe { group, end, anti } => {
                    bytes.push(14);
                    bytes.extend_from_slice(&group.to_be_bytes());
                    bytes.extend_from_slice(&end.to_be_bytes());
                    bytes.push(u8::from(*anti));
                }
                GlaOperator::ProbeEnd { group } => {
                    bytes.push(15);
                    bytes.extend_from_slice(&group.to_be_bytes());
                }
                GlaOperator::BindVertex { source } => {
                    bytes.push(16);
                    bytes.extend_from_slice(&source.0.to_be_bytes());
                }
                GlaOperator::BindOuterVertex { source } => {
                    bytes.push(25);
                    bytes.extend_from_slice(&source.0.to_be_bytes());
                }
                GlaOperator::Optional { group, end, slots } => {
                    bytes.push(17);
                    bytes.extend_from_slice(&group.to_be_bytes());
                    bytes.extend_from_slice(&end.to_be_bytes());
                    bytes.extend_from_slice(&slots.to_be_bytes());
                }
                GlaOperator::OptionalEnd { group } => {
                    bytes.push(18);
                    bytes.extend_from_slice(&group.to_be_bytes());
                }
                GlaOperator::CompareProperties {
                    left,
                    left_key,
                    right,
                    right_key,
                    comparison,
                } => {
                    bytes.push(19);
                    bytes.extend_from_slice(&left.0.to_be_bytes());
                    bytes.extend_from_slice(&left_key.0.to_be_bytes());
                    bytes.extend_from_slice(&right.0.to_be_bytes());
                    bytes.extend_from_slice(&right_key.0.to_be_bytes());
                    bytes.push(comparison.tag());
                }
                GlaOperator::SelectBoolean { expression } => {
                    bytes.push(21);
                    expression.append_transcript(&mut bytes);
                }
                GlaOperator::CapturePath {
                    capture,
                    start,
                    segments,
                } => {
                    bytes.push(26);
                    bytes.extend_from_slice(&capture.to_be_bytes());
                    bytes.extend_from_slice(&start.0.to_be_bytes());
                    bytes.extend_from_slice(&(segments.len() as u64).to_be_bytes());
                    for slot in segments {
                        bytes.extend_from_slice(&slot.0.to_be_bytes());
                    }
                }
                GlaOperator::SelectPathLength {
                    capture,
                    comparison,
                    value,
                } => {
                    bytes.push(27);
                    bytes.extend_from_slice(&capture.to_be_bytes());
                    bytes.push(comparison.tag());
                    bytes.extend_from_slice(&value.to_be_bytes());
                }
                GlaOperator::SelectPathNull {
                    capture,
                    function,
                    is_null,
                } => {
                    bytes.push(28);
                    bytes.extend_from_slice(&capture.to_be_bytes());
                    bytes.push(*function as u8);
                    bytes.push(u8::from(*is_null));
                }
            }
        }
        if let Some(width) = self.visible_columns {
            bytes.extend_from_slice(b"visible-prefix\0");
            bytes.extend_from_slice(&(width as u64).to_be_bytes());
        }
        bytes
    }
}

fn direction_tag(direction: GlaDirection) -> u8 {
    match direction {
        GlaDirection::Forward => 0,
        GlaDirection::Reverse => 1,
        GlaDirection::Undirected => 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RelationBind;

    fn bind() -> RelationBind {
        RelationBind::new()
            .with_relation("R", RelationId(1))
            .with_relation("S", RelationId(2))
            .with_label("L", LabelId(3))
            .with_property("n", PropertyKeyId(4))
    }

    #[test]
    fn output_contract_is_explicit_and_after_expansion() {
        let bound = bind()
            .bind("MATCH (a)-[:R]->(b)-[:S]->(c) RETURN b SKIP 1 LIMIT 2")
            .unwrap();
        let lowered = GlaPlan::lower(&bound);
        assert!(
            lowered
                .operators()
                .iter()
                .any(|op| matches!(op, GlaOperator::Expand { .. }))
        );
        assert_eq!(
            &lowered.operators()[lowered.operators().len() - 4..],
            &[
                GlaOperator::Project {
                    slot: BindingSlot(1)
                },
                GlaOperator::Distinct,
                GlaOperator::OrderByVertexId,
                GlaOperator::Limit {
                    offset: 1,
                    count: Some(2)
                },
            ]
        );
    }

    #[test]
    fn normalized_names_do_not_change_the_logical_transcript() {
        let a = bind().bind("MATCH (a)-[:R]->(b) RETURN b").unwrap();
        let b = bind().bind("MATCH (x)-[:R]->(y) RETURN y").unwrap();
        assert_eq!(
            GlaPlan::lower(&a).canonical_bytes(),
            GlaPlan::lower(&b).canonical_bytes()
        );
        let mut changed = a.clone();
        changed.neq = Some(("a".into(), "b".into()));
        assert_ne!(
            GlaPlan::lower(&a).canonical_bytes(),
            GlaPlan::lower(&changed).canonical_bytes()
        );
        changed = a.clone();
        changed.dst_prop_le = Some((PropertyKeyId(4), i64::MIN));
        assert_ne!(
            GlaPlan::lower(&a).canonical_bytes(),
            GlaPlan::lower(&changed).canonical_bytes()
        );
    }

    #[test]
    fn comparisons_do_not_subtract_and_missing_is_not_unequal() {
        for comparison in [
            IntegerComparison::Equal,
            IntegerComparison::NotEqual,
            IntegerComparison::Greater,
            IntegerComparison::Less,
            IntegerComparison::GreaterOrEqual,
            IntegerComparison::LessOrEqual,
        ] {
            let predicate = VertexPredicate::IntegerProperty {
                key: PropertyKeyId(4),
                comparison,
                value: i64::MAX,
            };
            assert!(!predicate.matches(&[], &[]));
            assert_eq!(
                predicate.matches(&[], &[(PropertyKeyId(4), CanonicalScalar::Int(i64::MIN))]),
                comparison.accepts(i64::MIN, i64::MAX)
            );
        }
    }

    #[test]
    fn integer_predicates_admit_float_properties_without_rounding_the_literal() {
        use fgdb_types::CanonicalF64;

        let comparisons = [
            IntegerComparison::Equal,
            IntegerComparison::NotEqual,
            IntegerComparison::Greater,
            IntegerComparison::Less,
            IntegerComparison::GreaterOrEqual,
            IntegerComparison::LessOrEqual,
        ];
        let equal = [true, false, false, false, true, true];
        let less = [false, true, false, true, false, true];
        let greater = [false, true, true, false, true, false];
        for (actual, literal, expected) in [
            (1.0, 1, equal),
            (-0.0, 0, equal),
            (1.5, 1, greater),
            (-1.5, -1, less),
            (9_007_199_254_740_992.0, 9_007_199_254_740_993, less),
            (9_223_372_036_854_775_808.0, i64::MAX, greater),
            (-9_223_372_036_854_775_808.0, i64::MIN, equal),
            (f64::NEG_INFINITY, i64::MIN, less),
            (f64::INFINITY, i64::MAX, greater),
            // STRICT_PORTABLE puts the canonical NaN after positive infinity.
            (f64::NAN, i64::MAX, greater),
        ] {
            let properties = [(
                PropertyKeyId(4),
                CanonicalScalar::Float(CanonicalF64::new(actual)),
            )];
            for (comparison, expected) in comparisons.into_iter().zip(expected) {
                let predicate = VertexPredicate::IntegerProperty {
                    key: PropertyKeyId(4),
                    comparison,
                    value: literal,
                };
                assert_eq!(predicate.matches(&[], &properties), expected);
                assert_eq!(
                    predicate
                        .matches_borrowed([], properties.iter().map(|(key, value)| (*key, value)),),
                    expected
                );
            }
        }
        for comparison in comparisons {
            let predicate = VertexPredicate::IntegerProperty {
                key: PropertyKeyId(4),
                comparison,
                value: 1,
            };
            for actual in [
                CanonicalScalar::Null,
                CanonicalScalar::Bool(true),
                CanonicalScalar::ucs_basic_text("1").unwrap(),
            ] {
                assert!(!predicate.matches(&[], &[(PropertyKeyId(4), actual)]));
            }
            assert!(!predicate.matches(&[], &[]));
            assert!(!predicate.matches(&[], &[(PropertyKeyId(99), CanonicalScalar::Int(1))]));
        }
    }

    #[test]
    fn lowered_node_and_edge_scans_keep_the_same_mixed_numeric_matches() {
        use fgdb_types::CanonicalF64;

        let values = [
            CanonicalScalar::Int(1),
            CanonicalScalar::Float(CanonicalF64::new(1.5)),
            CanonicalScalar::Float(CanonicalF64::new(3.0)),
            CanonicalScalar::Null,
            CanonicalScalar::ucs_basic_text("3").unwrap(),
            CanonicalScalar::Int(0),
        ];
        for statement in [
            "MATCH (a:L) WHERE a.n > 1 RETURN a",
            "MATCH (a:L)-[:R]->(b) WHERE a.n > 1 RETURN a",
        ] {
            let plan = GlaPlan::lower(&bind().bind(statement).unwrap());
            let vertices: Vec<_> = (1..=values.len() as u128).map(VId).collect();
            let edges: Vec<_> = vertices
                .iter()
                .map(|id| (*id, RelationId(1), *id))
                .collect();
            let rows = plan
                .execute(vertices, edges, |id, predicates| {
                    let properties = [(PropertyKeyId(4), values[id.0 as usize - 1].clone())];
                    Ok::<_, ()>(
                        predicates
                            .iter()
                            .all(|predicate| predicate.matches(&[LabelId(3)], &properties)),
                    )
                })
                .unwrap();
            assert_eq!(rows, vec![VId(2), VId(3)], "{statement}");
        }
    }

    #[test]
    fn incoming_normalization_is_explicit() {
        let one = bind().bind("MATCH (a)<-[:R]-(b) RETURN b").unwrap();
        let two = bind()
            .bind("MATCH (a)<-[:R]-(b)<-[:S]-(c) RETURN c")
            .unwrap();
        assert!(matches!(
            GlaPlan::lower(&one).operators().first(),
            Some(GlaOperator::ScanEdges {
                direction: GlaDirection::Forward,
                ..
            })
        ));
        assert!(matches!(
            GlaPlan::lower(&two).operators().first(),
            Some(GlaOperator::ScanEdges {
                direction: GlaDirection::Reverse,
                ..
            })
        ));
    }

    #[test]
    fn self_loops_and_closed_walks_preserve_binding_identity() {
        use fgdb_types::VId;
        let edges = [
            (VId(1), RelationId(1), VId(2)),
            (VId(2), RelationId(1), VId(2)),
            (VId(2), RelationId(2), VId(1)),
            (VId(2), RelationId(2), VId(3)),
        ];
        for statement in [
            "MATCH (a)-[:R]->(a) RETURN a",
            "MATCH (a)<-[:R]-(a) RETURN a",
            "MATCH (a)-[:R]-(a) RETURN a",
        ] {
            let plan = bind().bind(statement).unwrap();
            let rows = GlaPlan::lower(&plan)
                .execute([], edges, |_, _| Ok::<_, ()>(true))
                .unwrap();
            assert_eq!(rows, vec![VId(2)], "{statement}");
        }
        for statement in [
            "MATCH (a)-[:R]->(b)-[:S]->(a) RETURN a",
            "MATCH (a)<-[:R]-(b)<-[:S]-(a) RETURN a",
            "MATCH (a)-[:R]-(b)-[:S]-(a) RETURN a",
        ] {
            let plan = bind().bind(statement).unwrap();
            let rows = GlaPlan::lower(&plan)
                .execute([], edges, |_, _| Ok::<_, ()>(true))
                .unwrap();
            let expected = if plan.direction == EdgeDirection::Undirected {
                vec![VId(1), VId(2)]
            } else if plan.direction == EdgeDirection::Incoming {
                vec![VId(2)]
            } else {
                vec![VId(1)]
            };
            assert_eq!(rows, expected, "{statement}");
        }
    }

    #[test]
    fn alias_checks_follow_binding_and_precede_property_observation() {
        use fgdb_types::VId;
        let plan = bind()
            .bind("MATCH (a)-[:R]->(a) WHERE a.n = 7 RETURN a")
            .unwrap();
        let rows = GlaPlan::lower(&plan)
            .execute([], [(VId(1), RelationId(1), VId(2))], |_, _| {
                Err::<bool, _>("a non-loop must not reach its property read")
            })
            .unwrap();
        assert!(rows.is_empty());
        let closed = bind()
            .bind("MATCH (a)-[:R]->(b)-[:S]->(a) RETURN a")
            .unwrap();
        let renamed = bind()
            .bind("MATCH (x)-[:R]->(y)-[:S]->(x) RETURN x")
            .unwrap();
        let open = bind()
            .bind("MATCH (a)-[:R]->(b)-[:S]->(c) RETURN c")
            .unwrap();
        assert_eq!(
            GlaPlan::lower(&closed).canonical_bytes(),
            GlaPlan::lower(&renamed).canonical_bytes()
        );
        assert_ne!(
            GlaPlan::lower(&closed).canonical_bytes(),
            GlaPlan::lower(&open).canonical_bytes()
        );
    }

    #[test]
    fn every_three_position_alias_partition_matches_direct_enumeration() {
        use fgdb_types::VId;
        let universe = [
            (VId(1), RelationId(1), VId(1)),
            (VId(1), RelationId(1), VId(2)),
            (VId(2), RelationId(1), VId(1)),
            (VId(2), RelationId(2), VId(1)),
            (VId(1), RelationId(2), VId(2)),
            (VId(2), RelationId(2), VId(2)),
        ];
        for mask in 0..(1_u32 << universe.len()) {
            let edges: Vec<_> = universe
                .iter()
                .enumerate()
                .filter(|(i, _)| mask & (1_u32 << *i) != 0)
                .map(|(_, row)| *row)
                .collect();
            for names in [
                ["a", "b", "c"],
                ["a", "a", "b"],
                ["a", "b", "a"],
                ["a", "b", "b"],
                ["a", "a", "a"],
            ] {
                for (arrow, direction) in [
                    ("->", EdgeDirection::Outgoing),
                    ("-", EdgeDirection::Undirected),
                    ("<-", EdgeDirection::Incoming),
                ] {
                    let [a, b, c] = names;
                    let pattern = match direction {
                        EdgeDirection::Incoming => format!("MATCH ({a})<-[:R]-({b})<-[:S]-({c})"),
                        _ => format!("MATCH ({a})-[:R]{arrow}({b})-[:S]{arrow}({c})"),
                    };
                    let orient = |s, d| match direction {
                        EdgeDirection::Incoming => vec![(d, s)],
                        EdgeDirection::Undirected if s != d => vec![(s, d), (d, s)],
                        _ => vec![(s, d)],
                    };
                    for returned in names {
                        let statement = format!("{pattern} RETURN {returned}");
                        let plan = bind().bind(&statement).unwrap();
                        let mut expected = Vec::new();
                        for &(s, r, d) in &edges {
                            if r != RelationId(1) {
                                continue;
                            }
                            for (x, y) in orient(s, d) {
                                for &(s2, r2, d2) in &edges {
                                    if r2 != RelationId(2) {
                                        continue;
                                    }
                                    for (via, z) in orient(s2, d2) {
                                        let values = [x, y, z];
                                        let consistent = (0..3).all(|i| {
                                            (0..i).all(|j| {
                                                names[i] != names[j] || values[i] == values[j]
                                            })
                                        });
                                        if via == y && consistent {
                                            let at = names
                                                .iter()
                                                .position(|name| *name == returned)
                                                .unwrap();
                                            expected.push(values[at]);
                                        }
                                    }
                                }
                            }
                        }
                        expected.sort_unstable();
                        expected.dedup();
                        let actual = GlaPlan::lower(&plan)
                            .execute([], edges.iter().copied(), |_, _| Ok::<_, ()>(true))
                            .unwrap();
                        assert_eq!(actual, expected, "mask={mask}, {statement}");
                    }
                }
            }
        }
    }
}
