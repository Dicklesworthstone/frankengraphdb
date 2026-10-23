//! Relationship MERGE actions over the occurrence bag of a chained write.
//!
//! The ordinary MERGE definition owns endpoint identity and exact matching.
//! These clauses own only property assignments to each resolved relationship.

use crate::algebra::GraphValueRow;
use crate::{
    GraphEdgeUpsertAction, GraphEdgeUpsertBranch, GraphEdgeUpsertBuildError, GraphEdgeUpsertValue,
    GraphIntegerExpression, GraphSetColumnType, MAX_GRAPH_EDGE_UPSERT_ACTIONS,
    PreparedGraphEdgeMerge,
};
use fgdb_delta_types::PropertyKeyId;
use std::collections::BTreeSet;

/// A property of the selected relationship, or a column of its input row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphEdgeMergeRowInput {
    Property(PropertyKeyId),
    Column(usize),
}

/// A value computed from the carried row by the shared native value compiler.
#[derive(Clone, PartialEq, Eq)]
pub struct GraphEdgeMergeRowComputed(pub(crate) crate::GraphVertexMergeRowComputed);

impl GraphEdgeMergeRowComputed {
    /// Admit and evaluate the row value as an ordinary stored property value.
    /// `None` is a value outside the stored-property domain.
    pub fn written<C>(
        &self,
        row: &GraphValueRow,
        control: &mut impl FnMut(crate::GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<fgdb_types::CanonicalPropertyValue>, crate::GraphIntegerEvaluationError<C>>
    {
        self.0.written(row, control)
    }
}

/// One assignment in a simultaneous ON or trailing SET clause.
#[derive(Clone, PartialEq, Eq)]
pub enum GraphEdgeMergeRowAction {
    /// A scalar or stored constant. Expression and Composite values are
    /// rejected by preparation; row expressions use the explicit input frame.
    Edge(GraphEdgeUpsertAction<GraphEdgeUpsertValue>),
    Expression {
        key: PropertyKeyId,
        inputs: Vec<GraphEdgeMergeRowInput>,
        value: GraphIntegerExpression,
    },
    Value {
        key: PropertyKeyId,
        value: GraphEdgeMergeRowComputed,
    },
}

impl GraphEdgeMergeRowAction {
    #[must_use]
    pub const fn key(&self) -> PropertyKeyId {
        match self {
            Self::Edge(action) => action.key,
            Self::Expression { key, .. } | Self::Value { key, .. } => *key,
        }
    }
}

impl core::fmt::Debug for GraphEdgeMergeRowAction {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::Edge(_) => "Edge([REDACTED])",
            Self::Expression { .. } => "Expression([REDACTED])",
            Self::Value { .. } => "Value([REDACTED])",
        })
    }
}

/// Only the selected ON clause runs, followed by the common SET clause.
/// Every input occurrence resolves again after the previous occurrence's
/// actions, and every parallel match receives its own selected clauses.
#[derive(Clone, Default, Debug, PartialEq, Eq)]
pub struct GraphEdgeMergeRowActions {
    pub on_match: Vec<GraphEdgeMergeRowAction>,
    pub on_create: Vec<GraphEdgeMergeRowAction>,
    pub after: Vec<GraphEdgeMergeRowAction>,
}

/// A row-fed relationship MERGE with occurrence-preserving property actions.
#[derive(Clone, PartialEq, Eq)]
pub struct PreparedGraphEdgeUpsertRows {
    merge: PreparedGraphEdgeMerge,
    actions: GraphEdgeMergeRowActions,
}

impl core::fmt::Debug for PreparedGraphEdgeUpsertRows {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PreparedGraphEdgeUpsertRows")
            .field("actions", &self.actions)
            .field("definition", &"[REDACTED]")
            .finish()
    }
}

impl PreparedGraphEdgeUpsertRows {
    /// Validate every branch's input frame and clause-local target uniqueness.
    /// Reassigning a property in the later SET remains a distinct action.
    pub fn prepare(
        merge: PreparedGraphEdgeMerge,
        actions: GraphEdgeMergeRowActions,
    ) -> Result<Self, GraphEdgeUpsertBuildError> {
        let types = merge.input_column_types();
        for (branch, selected) in [
            (GraphEdgeUpsertBranch::Match, &actions.on_match),
            (GraphEdgeUpsertBranch::Create, &actions.on_create),
        ] {
            let observed = selected.len().saturating_add(actions.after.len());
            if observed > MAX_GRAPH_EDGE_UPSERT_ACTIONS {
                return Err(GraphEdgeUpsertBuildError::TooManyActions {
                    branch,
                    limit: MAX_GRAPH_EDGE_UPSERT_ACTIONS,
                    observed,
                });
            }
            for clause in [selected, &actions.after] {
                let mut keys = BTreeSet::new();
                for (action, assignment) in clause.iter().enumerate() {
                    if !keys.insert(assignment.key()) {
                        return Err(GraphEdgeUpsertBuildError::DuplicateProperty { branch });
                    }
                    let valid = match assignment {
                        GraphEdgeMergeRowAction::Edge(action) => matches!(
                            action.value,
                            GraphEdgeUpsertValue::Literal(_) | GraphEdgeUpsertValue::Stored(_)
                        ),
                        GraphEdgeMergeRowAction::Expression { inputs, value, .. } => {
                            if inputs.len() > MAX_GRAPH_EDGE_UPSERT_ACTIONS {
                                return Err(GraphEdgeUpsertBuildError::TooManyInputs {
                                    branch,
                                    action,
                                    limit: MAX_GRAPH_EDGE_UPSERT_ACTIONS,
                                    observed: inputs.len(),
                                });
                            }
                            inputs.iter().all(|input| match input {
                                GraphEdgeMergeRowInput::Property(_) => true,
                                GraphEdgeMergeRowInput::Column(column) => matches!(
                                    types.get(*column),
                                    Some(
                                        GraphSetColumnType::Scalar
                                            | GraphSetColumnType::Stored
                                            | GraphSetColumnType::Any
                                    )
                                ),
                            }) && !value
                                .referenced_columns()
                                .any(|column| column >= inputs.len())
                                && value.referenced_locals().next().is_none()
                        }
                        GraphEdgeMergeRowAction::Value { value, .. } => matches!(
                            crate::GraphSetProjection::admit_output(&value.0.0, &types, action),
                            Ok(GraphSetColumnType::Scalar
                                | GraphSetColumnType::Stored
                                | GraphSetColumnType::List
                                | GraphSetColumnType::Any)
                        ),
                    };
                    if !valid {
                        return Err(GraphEdgeUpsertBuildError::InvalidExpressionInput {
                            branch,
                            action,
                        });
                    }
                }
            }
        }
        Ok(Self { merge, actions })
    }

    #[must_use]
    pub fn merge(&self) -> &PreparedGraphEdgeMerge {
        &self.merge
    }

    #[must_use]
    pub fn actions(&self) -> &GraphEdgeMergeRowActions {
        &self.actions
    }

    #[must_use]
    pub fn action_count(&self, branch: GraphEdgeUpsertBranch) -> usize {
        let selected = match branch {
            GraphEdgeUpsertBranch::NoInput => return 0,
            GraphEdgeUpsertBranch::Match => &self.actions.on_match,
            GraphEdgeUpsertBranch::Create => &self.actions.on_create,
        };
        selected.len() + self.actions.after.len()
    }

    /// Bind identities, input positions, branch order and action bytecode to
    /// one deterministic definition. Values remain redacted by Debug.
    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        fn field(bytes: &mut Vec<u8>, value: &[u8]) {
            bytes.extend_from_slice(&(value.len() as u64).to_be_bytes());
            bytes.extend_from_slice(value);
        }
        let mut bytes = b"fgdb:edge-upsert-rows:v1\0".to_vec();
        field(&mut bytes, &self.merge.canonical_bytes());
        for clause in [
            &self.actions.on_match,
            &self.actions.on_create,
            &self.actions.after,
        ] {
            bytes.extend_from_slice(&(clause.len() as u64).to_be_bytes());
            for action in clause {
                bytes.extend_from_slice(&action.key().0.to_be_bytes());
                match action {
                    GraphEdgeMergeRowAction::Edge(action) => match &action.value {
                        GraphEdgeUpsertValue::Literal(value) => {
                            bytes.push(0);
                            field(&mut bytes, value.canonical_bytes());
                        }
                        GraphEdgeUpsertValue::Stored(value) => match value.encode() {
                            Ok(value) => {
                                bytes.push(1);
                                field(&mut bytes, &value);
                            }
                            Err(_) => bytes.push(255),
                        },
                        // Preparation rejects these variants.
                        GraphEdgeUpsertValue::Expression { .. }
                        | GraphEdgeUpsertValue::Composite { .. } => bytes.push(254),
                    },
                    GraphEdgeMergeRowAction::Expression { inputs, value, .. } => {
                        bytes.push(2);
                        bytes.extend_from_slice(&(inputs.len() as u64).to_be_bytes());
                        for input in inputs {
                            match input {
                                GraphEdgeMergeRowInput::Property(key) => {
                                    bytes.push(0);
                                    bytes.extend_from_slice(&key.0.to_be_bytes());
                                }
                                GraphEdgeMergeRowInput::Column(column) => {
                                    bytes.push(1);
                                    bytes.extend_from_slice(&(*column as u64).to_be_bytes());
                                }
                            }
                        }
                        field(&mut bytes, &value.canonical_bytes());
                    }
                    GraphEdgeMergeRowAction::Value { value, .. } => {
                        bytes.push(3);
                        let mut encoded = Vec::new();
                        value.0.0.append_canonical_bytes(&mut encoded);
                        field(&mut bytes, &encoded);
                    }
                }
            }
        }
        bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::algebra::GraphValue;
    use crate::{
        GqlParameters, GraphSymbol, GraphSymbolKind, GraphWriteChainStep, PreparedGraphWriteChain,
        PreparedGraphWriteChainText,
    };
    use fgdb_delta_types::{ElementId, LabelId, RelationId};
    use fgdb_types::{CanonicalScalar, EId, VId};

    const R: RelationId = RelationId(1);
    const P: PropertyKeyId = PropertyKeyId(1);
    const Q: PropertyKeyId = PropertyKeyId(2);

    fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        match (kind, name) {
            (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
            (GraphSymbolKind::Relation, "S") => Some(GraphSymbol::Relation(RelationId(2))),
            (GraphSymbolKind::Label, "A") => Some(GraphSymbol::Label(LabelId(1))),
            (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
            (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
            _ => None,
        }
    }

    fn scalar(value: i64) -> GraphValue {
        GraphValue::Scalar(CanonicalScalar::Int(value))
    }

    fn bound(text: &str, parameters: &GqlParameters) -> PreparedGraphWriteChain {
        assert!(PreparedGraphWriteChainText::is_chain(text).unwrap());
        let parsed = PreparedGraphWriteChainText::prepare(text, R, symbols).unwrap();
        let bound = parsed.bind_parameters(parameters).unwrap();
        assert_eq!(parsed.returns_rows(), bound.returns_rows());
        bound
    }

    fn definition(step: &GraphWriteChainStep) -> &PreparedGraphEdgeUpsertRows {
        match step {
            GraphWriteChainStep::EdgeUpsert(upsert)
            | GraphWriteChainStep::EdgeUpsertRows { upsert, .. }
            | GraphWriteChainStep::EdgeUpsertQuery { upsert, .. } => upsert,
            _ => panic!("expected action-bearing relationship MERGE"),
        }
    }

    #[test]
    fn parameter_map_actions_bind_shared_values_and_keep_return_metadata() {
        let row = GraphValue::map(vec![
            ("s".into(), scalar(1)),
            ("d".into(), scalar(2)),
            ("w".into(), scalar(7)),
        ])
        .unwrap();
        let parameters = GqlParameters::new()
            .with_list("rows", vec![row.clone()])
            .unwrap()
            .with_int64("tail", 9)
            .unwrap();
        let text = "UNWIND $rows AS row MERGE (a:A {p:row.s}) \
                    MERGE (b:A {p:row.d}) MERGE (a)-[e:R]->(b) \
                    ON CREATE SET e.p=row.w ON MATCH SET e.p=row.w \
                    SET e.q=$tail RETURN e.p AS weight,e.q AS tail \
                    ORDER BY weight LIMIT 1";
        let parsed = PreparedGraphWriteChainText::prepare(text, R, symbols).unwrap();
        assert_eq!(parsed.parameter_schema().len(), 2);
        let program = parsed.bind_parameters(&parameters).unwrap();
        let last = program.steps().last().unwrap();
        assert!(matches!(last, GraphWriteChainStep::EdgeUpsertQuery { .. }));
        assert_eq!(program.columns(), &["weight", "tail"]);
        assert!(program.returns_rows());
        let upsert = definition(last);
        assert_eq!(upsert.action_count(GraphEdgeUpsertBranch::Create), 2);
        assert_eq!(upsert.action_count(GraphEdgeUpsertBranch::Match), 2);
        assert_eq!(upsert.action_count(GraphEdgeUpsertBranch::NoInput), 0);
        let input = GraphValueRow::from_owned_values(vec![
            row.clone(),
            GraphValue::Vertex(VId(11)),
            GraphValue::Vertex(VId(12)),
        ]);
        for actions in [&upsert.actions().on_create, &upsert.actions().on_match] {
            let GraphEdgeMergeRowAction::Value { key, value } = &actions[0] else {
                panic!("row.w must use the shared row-value compiler");
            };
            assert_eq!(*key, P);
            assert_eq!(
                value.written(&input, &mut |_| Ok::<_, ()>(())).unwrap(),
                Some(CanonicalScalar::Int(7).into()),
            );
        }
        let changed = GqlParameters::new()
            .with_list("rows", vec![row])
            .unwrap()
            .with_int64("tail", 10)
            .unwrap();
        let changed = parsed.bind_parameters(&changed).unwrap();
        assert_ne!(
            upsert.canonical_bytes(),
            definition(changed.steps().last().unwrap()).canonical_bytes(),
        );
    }

    #[test]
    fn computed_actions_have_explicit_edge_and_row_inputs_and_private_continuations() {
        let program = bound(
            "UNWIND [1,2] AS delta MERGE (a:A {p:1}) MERGE (b:A {p:2}) \
             MERGE (a)-[r:R]->(b) ON MATCH SET r.p=r.p+delta SET r.q=r.p \
             CREATE (a)-[:S]->(b)",
            &GqlParameters::new(),
        );
        assert!(!program.returns_rows());
        let step = &program.steps()[2];
        assert!(matches!(
            step,
            GraphWriteChainStep::EdgeUpsertRows { keep: 3, .. }
        ));
        let upsert = definition(step);
        let GraphEdgeMergeRowAction::Expression { key, inputs, value } =
            &upsert.actions().on_match[0]
        else {
            panic!("mixed edge and row inputs need an explicit scalar frame");
        };
        assert_eq!(*key, P);
        assert_eq!(
            inputs,
            &[
                GraphEdgeMergeRowInput::Property(P),
                GraphEdgeMergeRowInput::Column(0)
            ],
        );
        assert_eq!(
            value
                .evaluate_scalar_with_control(&[scalar(10), scalar(2)], &mut |_| Ok::<_, ()>(()))
                .unwrap(),
            CanonicalScalar::Int(12),
        );
        let input = GraphValueRow::from_owned_values(vec![
            scalar(2),
            GraphValue::Vertex(VId(11)),
            GraphValue::Vertex(VId(12)),
        ]);
        let continued = step
            .continuation_row_controlled(&input, ElementId::Edge(EId(20)), &mut |_| {
                Ok::<_, crate::GraphIntegerError>(())
            })
            .unwrap();
        assert_eq!(
            continued.values(),
            &[
                scalar(2),
                GraphValue::Vertex(VId(11)),
                GraphValue::Vertex(VId(12)),
                GraphValue::Edge(EId(20)),
            ],
        );
        let mut malformed = upsert.actions().clone();
        let GraphEdgeMergeRowAction::Expression { inputs, .. } = &mut malformed.on_match[0] else {
            unreachable!();
        };
        inputs[1] = GraphEdgeMergeRowInput::Column(99);
        assert!(matches!(
            PreparedGraphEdgeUpsertRows::prepare(upsert.merge().clone(), malformed),
            Err(GraphEdgeUpsertBuildError::InvalidExpressionInput { .. }),
        ));
    }

    #[test]
    fn with_head_and_plain_set_preserve_existing_step_boundaries() {
        let headed = bound(
            "MERGE (a:A {p:1}) MERGE (b:A {p:2}) MERGE (a)-[r:R]->(b) \
             ON CREATE SET r.p=7 WITH a,b,r CREATE (a)-[:S]->(b)",
            &GqlParameters::new(),
        );
        assert!(matches!(
            headed.steps()[2],
            GraphWriteChainStep::EdgeUpsertQuery { .. }
        ));
        assert!(!headed.returns_rows());
        for (tail, upsert) in [
            ("SET r.p=3", true),
            ("SET a.p=3", false),
            ("SET r.p=3,a.q=4", false),
        ] {
            let program = bound(
                &format!("MERGE (a:A {{p:1}}) MERGE (b:A {{p:2}}) MERGE (a)-[r:R]->(b) {tail}"),
                &GqlParameters::new(),
            );
            if upsert {
                assert!(matches!(
                    program.steps().last().unwrap(),
                    GraphWriteChainStep::EdgeUpsert(_)
                ));
            } else {
                assert!(matches!(
                    program.steps()[2],
                    GraphWriteChainStep::EdgeMergeRows { .. }
                ));
                assert!(matches!(
                    program.steps().last().unwrap(),
                    GraphWriteChainStep::Mutation(_)
                ));
            }
            assert!(!program.returns_rows());
        }
    }

    #[test]
    fn invalid_branch_targets_duplicates_and_schemas_refuse_before_execution() {
        for tail in [
            "ON CREATE SET a.p=1",
            "ON CREATE SET r:A",
            "ON CREATE SET r.p=1 ON CREATE SET r.q=2",
            "ON MATCH SET r.p=1,r.p=2",
            "ON MATCH SET r.p=unknown.p",
            "ON MATCH SET r.p=a",
        ] {
            let text =
                format!("MERGE (a:A {{p:1}}) MERGE (b:A {{p:2}}) MERGE (a)-[r:R]->(b) {tail}");
            let result = PreparedGraphWriteChainText::prepare(&text, R, symbols)
                .and_then(|parsed| parsed.bind_parameters(&GqlParameters::new()));
            assert!(result.is_err(), "{text}");
        }
        let anonymous = "MERGE (a:A {p:1}) MERGE (b:A {p:2}) \
                         MERGE (a)-[:R]->(b) ON CREATE SET r.p=1";
        assert!(PreparedGraphWriteChainText::prepare(anonymous, R, symbols).is_err());
        // A relationship property map still defines identity; it cannot
        // quietly become a row-dependent action map.
        let dynamic_map = "UNWIND [1] AS w MERGE (a:A {p:1}) MERGE (b:A {p:2}) \
                           MERGE (a)-[r:R {p:w}]->(b) ON CREATE SET r.q=w";
        assert!(PreparedGraphWriteChainText::prepare(dynamic_map, R, symbols).is_err());
    }
}
