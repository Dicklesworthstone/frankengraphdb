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
pub use boolean::{BoundBooleanExpression, GraphBooleanError, GraphBooleanExpression,
    GraphBooleanOp, GraphBooleanOperand, MAX_BOOLEAN_INSTRUCTIONS};
pub use existence::{GraphExistence, GraphMatchClause};
pub use ordering::{GraphOrderError, GraphValueOrder};
pub(crate) use values::{RowKey, ValueRef};
pub use output::{GlaIdentityOutput, GlaOutput, GraphBindingRow};
pub use pattern::{
    GraphPatternBuilder, MAX_PATTERN_EDGES, MAX_PATTERN_IDENTITIES, MAX_PATTERN_NAME_BYTES,
    MAX_PATTERN_PREDICATES, MAX_PATTERN_VERTICES, PatternBuildError, PatternLimitDimension,
    PreparedGraphPattern,
};
pub use predicate::{MAX_SCALAR_PREDICATE_BYTES, ScalarPredicate, ScalarPredicateError};
pub use values::{
    GRAPH_VALUE_PAYLOAD_UNIT_BYTES, GraphColumn, GraphValue, GraphValueRow, ValueProjection,
};

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
                    && matches!(scalar, CanonicalScalar::Int(actual)
                    if comparison.accepts(*actual, *value))
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
}

/// Immutable logical definition with a statically determined output row shape.
/// Existing language and API calls retain their default `VId` result type.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GlaPlan<Row = VId> {
    operators: Vec<GlaOperator>,
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
    fn from_operators(operators: Vec<GlaOperator>) -> Self {
        Self {
            operators,
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
                GlaOperator::ScanEdges { .. } | GlaOperator::Expand { .. }
            )
        })
    }

    /// Property projections require a real source even without a predicate.
    #[must_use]
    pub fn projects_properties(&self) -> bool {
        self.operators.iter().any(|operator| match operator {
            GlaOperator::ProjectValues { columns } => columns
                .iter()
                .any(|column| matches!(column, ValueProjection::Property { .. })),
            _ => false,
        })
    }

    #[must_use]
    pub fn needs_vertex_values(&self) -> bool {
        self.projects_properties()
            || self.operators.iter().any(|operator| {
                matches!(
                    operator,
                    GlaOperator::Select { .. } | GlaOperator::CompareProperties { .. }
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
            }
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
