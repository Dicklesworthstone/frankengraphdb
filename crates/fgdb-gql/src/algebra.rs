//! Typed GLA lowering for the existing bounded MATCH language.
//!
//! This is the scan-backed migration seam of
//! `fgdb-boundplan-gla-lowering-seam-r2kd`, not the registered FreeJoin physical
//! family. The legacy language's set-of-vertex-IDs contract is explicit:
//! Project -> Distinct -> OrderByVertexId -> Limit. It is not GQL's general
//! multiset or path-identity contract. Operators are immutable after lowering.

use crate::{BoundPlan, EdgeDirection, ReturnProjection};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_types::CanonicalScalar;

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

/// A position-independent predicate. Missing and non-integer properties fail
/// every integer comparison, including NotEqual; no implicit coercion occurs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VertexPredicate {
    HasLabel(LabelId),
    IntegerProperty {
        key: PropertyKeyId,
        comparison: IntegerComparison,
        value: i64,
    },
}

impl VertexPredicate {
    #[must_use]
    pub fn matches(
        &self,
        labels: &[LabelId],
        properties: &[(PropertyKeyId, CanonicalScalar)],
    ) -> bool {
        match self {
            Self::HasLabel(label) => labels.contains(label),
            Self::IntegerProperty {
                key,
                comparison,
                value,
            } => properties.iter().any(|(actual_key, scalar)| {
                actual_key == key
                    && matches!(scalar, CanonicalScalar::Int(actual)
                        if comparison.accepts(*actual, *value))
            }),
        }
    }
}

/// A linear, typed subset of the GLA DAG. Each operator consumes its predecessor.
/// ScanEdges binds slots 0/1; Expand appends one binding; Select preserves the
/// complete binding row until Project. Only the final Distinct removes rows.
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
    Project {
        slot: BindingSlot,
    },
    Distinct,
    OrderByVertexId,
    Limit {
        offset: u64,
        count: Option<u64>,
    },
}

/// An immutable executable logical definition, not a second parser or binder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GlaPlan {
    operators: Vec<GlaOperator>,
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

impl GlaPlan {
    /// Lower every currently executable BoundPlan field exactly once. Positional
    /// predicate slots exist only at this legacy adapter, never in the executor.
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
            // Incoming two-hop statements retain the bounded binder's edge-flow
            // property role: dst properties constrain the anchor, labels the via.
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
            // A forged unlabeled node plan must not become an all-vertices scan.
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
        Self { operators }
    }

    #[must_use]
    pub fn operators(&self) -> &[GlaOperator] {
        &self.operators
    }

    /// Canonical, domain-separated logical transcript. This is an unreleased
    /// application transcript, not an Appendix A durable format or certificate.
    /// Syntax spelling and variable names are deliberately not logical identity.
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
}
