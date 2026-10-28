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

    /// As [`Self::written`], with the host's point reader at the same frozen
    /// input cut as the carried row. An open element's `x.key` must observe
    /// that cut even after an earlier occurrence has changed the property.
    /// Source and control failures retain the caller's original error type.
    pub fn written_with_elements<C>(
        &self,
        row: &GraphValueRow,
        control: &mut impl FnMut(crate::GlaExecutionEvent) -> Result<(), C>,
        elements: &mut impl crate::GraphElementReader<C>,
    ) -> Result<Option<fgdb_types::CanonicalPropertyValue>, crate::GraphIntegerEvaluationError<C>>
    {
        let value = crate::set_ops::projection::evaluate_value_reading(
            &self.0.0,
            row,
            0,
            control,
            &mut crate::set_ops::projection::HostElements(elements),
        )
        .map_err(|failure| match failure {
            crate::set_ops::ProjectionFailure::Control(error) => {
                crate::GraphIntegerEvaluationError::Control(error)
            }
            crate::set_ops::ProjectionFailure::Arithmetic { error, .. } => {
                crate::GraphIntegerEvaluationError::Value(error)
            }
        })?;
        Ok(value.to_written_property())
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
    /// `v.key = value` on the vertex in input column `column` (an endpoint
    /// or another vertex the row carries), not on the relationship. The
    /// value is computed from the row as `Value` is (fgdb-at06q).
    RowVertex {
        column: usize,
        key: PropertyKeyId,
        value: GraphEdgeMergeRowComputed,
    },
}

impl GraphEdgeMergeRowAction {
    #[must_use]
    pub const fn key(&self) -> PropertyKeyId {
        match self {
            Self::Edge(action) => action.key,
            Self::Expression { key, .. }
            | Self::Value { key, .. }
            | Self::RowVertex { key, .. } => *key,
        }
    }

    /// The input column of the vertex this assignment writes, or `None`
    /// when it writes the merged relationship.
    #[must_use]
    pub const fn vertex_column(&self) -> Option<usize> {
        match self {
            Self::RowVertex { column, .. } => Some(*column),
            Self::Edge(_) | Self::Expression { .. } | Self::Value { .. } => None,
        }
    }
}

impl core::fmt::Debug for GraphEdgeMergeRowAction {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::Edge(_) => "Edge([REDACTED])",
            Self::Expression { .. } => "Expression([REDACTED])",
            Self::Value { .. } => "Value([REDACTED])",
            Self::RowVertex { .. } => "RowVertex([REDACTED])",
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
    /// Validate every branch's input frame and clause-local target uniqueness
    /// (one assignment per element and key). Reassigning a property in the
    /// later SET remains a distinct action.
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
                    if !keys.insert((assignment.vertex_column(), assignment.key())) {
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
                        GraphEdgeMergeRowAction::RowVertex { column, value, .. } => {
                            // As for MERGE's endpoint columns, an open row
                            // value is checked by the native host before its
                            // assignment. NULL is a no-op; another kind refuses.
                            matches!(
                                types.get(*column),
                                Some(GraphSetColumnType::Vertex | GraphSetColumnType::Any)
                            ) && matches!(
                                crate::GraphSetProjection::admit_output(&value.0.0, &types, action),
                                Ok(GraphSetColumnType::Scalar
                                    | GraphSetColumnType::Stored
                                    | GraphSetColumnType::List
                                    | GraphSetColumnType::Any)
                            )
                        }
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
                    GraphEdgeMergeRowAction::RowVertex { column, value, .. } => {
                        bytes.push(4);
                        bytes.extend_from_slice(&(*column as u64).to_be_bytes());
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
            (GraphSymbolKind::Label, "A" | "Visible") => Some(GraphSymbol::Label(LabelId(1))),
            (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
            (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
            (GraphSymbolKind::Property, "secret") => Some(GraphSymbol::Property(PropertyKeyId(3))),
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
    fn carried_vertices_are_assignment_targets_of_their_own() {
        // TCK Merge6 [1]: an anonymous relationship's ON clause writes an
        // endpoint (fgdb-at06q).
        let targets = |text: &str| {
            let program = bound(text, &GqlParameters::new());
            let upsert = definition(&program.steps()[0]).clone();
            let target = |actions: &[GraphEdgeMergeRowAction]| {
                actions
                    .iter()
                    .map(|action| (action.vertex_column(), action.key()))
                    .collect::<Vec<_>>()
            };
            (
                target(&upsert.actions().on_create),
                target(&upsert.actions().on_match),
                target(&upsert.actions().after),
                upsert.canonical_bytes(),
            )
        };
        let (create, matched, after, endpoint) =
            targets("MATCH (a:A), (b:A) MERGE (a)-[:R]->(b) ON CREATE SET b.p = 1");
        assert_eq!(create, [(Some(1), P)]);
        assert!(matched.is_empty() && after.is_empty());
        // The same key on the relationship and on each endpoint is three
        // assignments, not a duplicate; the trailing SET writes too.
        let (create, matched, after, _) = targets(
            "MATCH (a:A), (b:A) MERGE (a)-[r:R]->(b) \
             ON CREATE SET a.p = 1, b.p = a.p, r.p = 3 ON MATCH SET b.q = 2 SET r.q = r.p",
        );
        assert_eq!(create, [(Some(0), P), (Some(1), P), (None, P)]);
        assert_eq!(matched, [(Some(1), Q)]);
        assert_eq!(after, [(None, Q)]);
        // The definition binds which element an assignment writes.
        let (_, _, _, source) =
            targets("MATCH (a:A), (b:A) MERGE (a)-[:R]->(b) ON CREATE SET a.p = 1");
        assert_ne!(endpoint, source);
    }

    #[test]
    fn computed_endpoint_columns_admit_actions_without_erasing_target_types() {
        for endpoints in [
            "WITH ns[0] AS a, ns[1] AS b",
            "WITH head(ns) AS a, last(ns) AS b",
            "UNWIND ns AS a WITH a, a AS b",
        ] {
            let text = format!(
                "MATCH (n:A) WITH collect(n) AS ns {endpoints} \
                 MERGE (a)-[r:R]->(b) ON CREATE SET a.q=1 ON MATCH SET b.q=2"
            );
            let program = bound(&text, &GqlParameters::new());
            let upsert = definition(program.steps().last().unwrap());
            assert_eq!(upsert.actions().on_create[0].vertex_column(), Some(0));
            assert_eq!(upsert.actions().on_match[0].vertex_column(), Some(1));
            assert_eq!(
                &upsert.merge().input_column_types()[..2],
                &[GraphSetColumnType::Any, GraphSetColumnType::Any]
            );
        }
        // An explicitly scalar or relationship target is still a definition
        // error. Only an open value defers the vertex check to the native host.
        for input in ["1", "true", "[1,2]"] {
            let text = format!(
                "MATCH (a:A),(b:A) WITH a,b,{input} AS target \
                 MERGE (a)-[r:R]->(b) ON CREATE SET target.q=1"
            );
            assert!(
                PreparedGraphWriteChainText::prepare(&text, R, symbols).is_err(),
                "{text}"
            );
        }
        assert!(
            PreparedGraphWriteChainText::prepare(
                "MATCH (a)-[e:R]->(b) MERGE (a)-[r:S]->(b) ON CREATE SET e.q=1",
                R,
                symbols,
            )
            .is_err()
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
    fn following_create_reads_final_merge_fields_through_admitted_private_bindings() {
        let private = bound(
            "CREATE (a:Visible), (b:Visible) MERGE (a)-[e:R]->(b) \
             CREATE (c:Visible {p:e.secret})",
            &GqlParameters::new(),
        );
        assert!(!private.returns_rows());
        let GraphWriteChainStep::EdgeMergeQuery(query) = &private.steps()[1] else {
            panic!("a future field read needs the MERGE's final-state projection");
        };
        assert!(
            query
                .bindings()
                .contains(&crate::GraphEdgeMergeBinding::Property(PropertyKeyId(3),))
        );
        assert!(matches!(private.steps()[2], GraphWriteChainStep::Insert(_)));

        let endpoints = bound(
            "MATCH (a:Visible)-[:R]->(b:Visible) MERGE (a)-[s:S {p:7}]-(b) \
             CREATE (copy:Visible {p:b.p}) RETURN copy.p AS destination ORDER BY destination",
            &GqlParameters::new(),
        );
        let GraphWriteChainStep::EdgeMergeQuery(query) = &endpoints.steps()[0] else {
            panic!("the carried endpoint field must be a final-state binding");
        };
        assert!(query.bindings().iter().any(|binding| matches!(
            binding,
            crate::GraphEdgeMergeBinding::InputProperty { key: P, .. }
        )));
        assert_eq!(endpoints.columns(), &["destination"]);

        for head in ["WITH r", "WITH r AS edge"] {
            let variable = if head.ends_with("edge") { "edge" } else { "r" };
            let actions = bound(
                &format!(
                    "MERGE (a {{p:10}}) MERGE (b {{p:20}}) MERGE (a)-[r:R]->(b) \
                     ON MATCH SET r.p=r.q,r.q=r.p SET r.p=r.p+10 {head} \
                     CREATE (c {{p:{variable}.p,q:{variable}.q}}) RETURN c.p,c.q"
                ),
                &GqlParameters::new(),
            );
            let GraphWriteChainStep::EdgeUpsertQuery { query, .. } = &actions.steps()[2] else {
                panic!("the ON/SET head must keep its private final-state fields");
            };
            assert_eq!(
                query.column_types(),
                &[
                    GraphSetColumnType::Edge,
                    GraphSetColumnType::Stored,
                    GraphSetColumnType::Stored,
                ],
            );
            assert!(
                query
                    .bindings()
                    .contains(&crate::GraphEdgeMergeBinding::Property(P))
            );
            assert!(
                query
                    .bindings()
                    .contains(&crate::GraphEdgeMergeBinding::Property(Q))
            );
            assert_eq!(actions.columns(), &["c.p", "c.q"]);
        }
    }

    #[test]
    fn vertex_with_preimages_are_overlaid_by_the_terminal_set_before_return() {
        let program = bound(
            "UNWIND [1,2] AS k MERGE (a {p:k}) ON CREATE SET a.p=k \
             MERGE (b {p:k+10}) ON CREATE SET b.p=k+10 \
             WITH a,b SET b.p=b.p+100 RETURN b.p",
            &GqlParameters::new(),
        );
        let GraphWriteChainStep::VertexMergeQuery(head) = &program.steps()[1] else {
            panic!("WITH must retain the MERGE's final property preimage");
        };
        assert!(
            head.bindings()
                .contains(&crate::GraphVertexMergeRowsBinding::Property(P))
        );
        assert_eq!(
            head.column_types(),
            &[
                GraphSetColumnType::Vertex,
                GraphSetColumnType::Vertex,
                GraphSetColumnType::Stored,
            ],
        );
        let GraphWriteChainStep::MutationQuery(tail) = &program.steps()[2] else {
            panic!("the terminal SET must keep its existing mutation/RETURN owner");
        };
        let incoming = vec![
            GraphValueRow::from_owned_values(vec![
                GraphValue::Vertex(VId(1)),
                GraphValue::Vertex(VId(11)),
                scalar(11),
            ]),
            GraphValueRow::from_owned_values(vec![
                GraphValue::Vertex(VId(2)),
                GraphValue::Vertex(VId(12)),
                scalar(12),
            ]),
        ];
        let proposal = tail
            .execute_governed_over::<(), ()>(
                Some(incoming),
                crate::GraphMutationPolicy::new(
                    crate::GqlQueryPolicy::new(0, 2, 100_000, 100_000),
                    2,
                ),
                |_, _| panic!("the mutation consumes only its admitted private row"),
                || Ok(()),
            )
            .unwrap();
        let (_, returning) = proposal.into_parts();
        assert_eq!(returning.rows.result_rows, 2);
        assert_eq!(returning.value[0].values(), &[scalar(111)]);
        assert_eq!(returning.value[1].values(), &[scalar(112)]);
    }

    #[test]
    fn invalid_branch_targets_duplicates_and_schemas_refuse_before_execution() {
        for tail in [
            "ON CREATE SET r:A",
            "ON CREATE SET a:A",
            "ON CREATE SET r.p=1 ON CREATE SET r.q=2",
            "ON MATCH SET r.p=1,r.p=2",
            "ON MATCH SET a.p=1,a.p=2",
            "ON MATCH SET r.p=unknown.p",
            "ON MATCH SET unknown.p=1",
            "ON MATCH SET r.p=a",
            "ON MATCH SET a.p=b",
            "ON CREATE SET a.p=r.p",
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
