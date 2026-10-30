//! Branch-specific edge-property actions for bounded directed relationship MERGE.
//!
//! The underlying MERGE decides NoInput/Matched/Created once. Match/create
//! branches apply simultaneous SETs, then a distinct trailing SET clause reads
//! the preceding clause's staged effects. NoInput evaluates nothing, including
//! trailing expressions. Relationship deletion,
//! endpoint rewrites and property-valued match identity remain outside this core.

use crate::{
    GqlScalarParameter, GraphEdgeMergePolicy, GraphIntegerExpression, PreparedGraphEdgeMerge,
};
use fgdb_delta_types::PropertyKeyId;
use std::collections::BTreeSet;

pub const MAX_GRAPH_EDGE_UPSERT_ACTIONS: usize = 256;

#[derive(Clone, PartialEq, Eq)]
pub struct GraphEdgeUpsertAction<Value = GqlScalarParameter> {
    pub key: PropertyKeyId,
    pub value: Value,
}
impl<Value> core::fmt::Debug for GraphEdgeUpsertAction<Value> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("GraphEdgeUpsertAction([REDACTED])")
    }
}

/// A checked value expression over explicit properties of the chosen edge.
/// Input positions address `properties`, never an ambient MATCH binding. Each
/// action evaluates against its clause's frozen native overlay. No graph row,
/// parameter map or execution authority is retained by this definition.
#[derive(Clone, PartialEq, Eq)]
pub enum GraphEdgeUpsertValue {
    Literal(GqlScalarParameter),
    Expression {
        properties: Vec<PropertyKeyId>,
        value: GraphIntegerExpression,
    },
    /// The shared read-value algebra, including temporal constructors, map
    /// access, list indexing, comprehensions and reduce. Only a scalar result
    /// may be stored; collection-valued results refuse in the action owner.
    Composite {
        properties: Vec<PropertyKeyId>,
        value: crate::GraphSetValue,
    },
    /// A constant List value (fgdb-k3nub), evaluated when the statement
    /// binds and written as it is.
    Stored(fgdb_types::CanonicalPropertyValue),
}
impl core::fmt::Debug for GraphEdgeUpsertValue {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("GraphEdgeUpsertValue([REDACTED])")
    }
}
impl GraphEdgeUpsertValue {
    #[must_use]
    pub fn literal(&self) -> Option<&GqlScalarParameter> {
        match self {
            Self::Literal(value) => Some(value),
            Self::Expression { .. } | Self::Composite { .. } | Self::Stored(_) => None,
        }
    }

    /// Evaluate an already charged, owned property row without copying its
    /// payload. The caller supplies the selected edge's admitted properties in
    /// declaration order and retains its existing control and rollback owner.
    /// This performs no graph access. Dynamic collection results are returned
    /// intact so the action owner can refuse them, never flatten or truncate.
    pub fn evaluate_with_control<E>(
        &self,
        inputs: Vec<crate::algebra::GraphValue>,
        control: &mut impl FnMut(crate::GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<crate::algebra::GraphValue, crate::GraphIntegerEvaluationError<E>> {
        use crate::algebra::{GraphValue, GraphValueRow};
        use crate::{GraphIntegerEvaluationError, GraphSetValue};
        if let Self::Expression { value, .. } = self {
            return value
                .evaluate_scalar_with_control(&inputs, control)
                .map(GraphValue::Scalar);
        }
        // A constant stored value (fgdb-k3nub) needs no evaluation. Binding
        // admitted it through the stored bounds, so the error is impossible.
        if let Self::Stored(value) = self {
            return GraphValue::from_property_value(value).ok_or(
                GraphIntegerEvaluationError::Value(crate::GraphIntegerError {
                    instruction: 0,
                    kind: crate::GraphIntegerErrorKind::StoredValueBounds,
                }),
            );
        }
        control(crate::GlaExecutionEvent::ScratchEntry)
            .map_err(GraphIntegerEvaluationError::Control)?;
        let row = GraphValueRow::from_owned_values(inputs);
        let literal;
        let value = match self {
            Self::Literal(value) => {
                // GqlScalarParameter clones share their checked storage. The
                // value evaluator reserves the returned scalar's payload.
                literal = GraphSetValue::Literal(value.clone());
                &literal
            }
            Self::Composite { value, .. } => value,
            Self::Expression { .. } | Self::Stored(_) => unreachable!("returned above"),
        };
        crate::set_ops::evaluate_value(value, &row, 0, control).map_err(|error| match error {
            crate::set_ops::ProjectionFailure::Control(error) => {
                GraphIntegerEvaluationError::Control(error)
            }
            crate::set_ops::ProjectionFailure::Arithmetic { error, .. } => {
                GraphIntegerEvaluationError::Value(error)
            }
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphEdgeUpsertBranch {
    NoInput,
    Match,
    Create,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GraphEdgeUpsertBuildError {
    TooManyActions {
        branch: GraphEdgeUpsertBranch,
        limit: usize,
        observed: usize,
    },
    DuplicateProperty {
        branch: GraphEdgeUpsertBranch,
    },
    PropertyReplacement {
        branch: GraphEdgeUpsertBranch,
        action: usize,
    },
    TooManyInputs {
        branch: GraphEdgeUpsertBranch,
        action: usize,
        limit: usize,
        observed: usize,
    },
    InvalidExpressionInput {
        branch: GraphEdgeUpsertBranch,
        action: usize,
    },
}
impl core::fmt::Display for GraphEdgeUpsertBuildError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::TooManyActions {
                branch,
                limit,
                observed,
            } => write!(f, "{branch} has {observed} actions, limit {limit}"),
            Self::DuplicateProperty { branch } => {
                write!(f, "{branch} sets the same property twice")
            }
            Self::PropertyReplacement { branch, action } => write!(
                f,
                "{branch} action {action} has an invalid relationship property replacement"
            ),
            Self::TooManyInputs {
                branch,
                action,
                limit,
                observed,
            } => write!(
                f,
                "{branch} action {action} reads {observed} inputs, limit {limit}"
            ),
            Self::InvalidExpressionInput { branch, action } => write!(
                f,
                "{branch} action {action} reads a value other than the merged relationship's"
            ),
        }
    }
}
impl core::fmt::Display for GraphEdgeUpsertBranch {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::NoInput => "MERGE without an input row",
            Self::Match => "ON MATCH",
            Self::Create => "ON CREATE",
        })
    }
}
impl core::error::Error for GraphEdgeUpsertBuildError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GraphEdgeUpsertPolicy {
    /// One query allowance covers MERGE, property copies and final acceptance.
    pub merge: GraphEdgeMergePolicy,
    /// Selected ON plus trailing SET proposals, including overwritten fields.
    pub max_actions: u64,
}
impl GraphEdgeUpsertPolicy {
    #[must_use]
    pub const fn new(merge: GraphEdgeMergePolicy, max_actions: u64) -> Self {
        Self { merge, max_actions }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GraphEdgeUpsertStats {
    pub merge: crate::GraphEdgeMergeStats,
    pub branch: GraphEdgeUpsertBranch,
    pub action_effects: u64,
    /// Cumulative MERGE plus branch work/scratch. Do not add merge.evaluator
    /// again. Logical entries do not measure allocator or durable I/O costs.
    pub evaluator: crate::GlaExecutionStats,
}

#[derive(Debug)]
pub enum GraphEdgeUpsertError<E, A> {
    Merge(crate::GraphEdgeMergeError<E, A>),
    ActionLimit {
        limit: u64,
        observed: u128,
    },
    Staging(E),
    Expression {
        clause: usize,
        action: usize,
        source: crate::GraphIntegerError,
    },
    NonScalarValue {
        clause: usize,
        action: usize,
    },
}
impl<E: core::fmt::Display, A: core::fmt::Display> core::fmt::Display
    for GraphEdgeUpsertError<E, A>
{
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Merge(error) => error.fmt(f),
            Self::ActionLimit { limit, observed } => write!(
                f,
                "relationship MERGE branch action limit exceeded: {observed} > {limit}"
            ),
            Self::Staging(error) => error.fmt(f),
            Self::NonScalarValue { clause, action } => write!(
                f,
                "relationship MERGE clause {clause} action {action} produced a non-scalar property"
            ),
            Self::Expression {
                clause,
                action,
                source,
            } => {
                write!(
                    f,
                    "relationship MERGE clause {clause} action {action}: {source}"
                )
            }
        }
    }
}
impl<E: core::error::Error + 'static, A: core::error::Error + 'static> core::error::Error
    for GraphEdgeUpsertError<E, A>
{
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Merge(error) => Some(error),
            Self::Staging(error) => Some(error),
            Self::Expression { source, .. } => Some(source),
            Self::ActionLimit { .. } | Self::NonScalarValue { .. } => None,
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct PreparedGraphEdgeUpsert {
    merge: PreparedGraphEdgeMerge,
    on_match: Vec<GraphEdgeUpsertAction<GraphEdgeUpsertValue>>,
    on_create: Vec<GraphEdgeUpsertAction<GraphEdgeUpsertValue>>,
    after: Vec<GraphEdgeUpsertAction<GraphEdgeUpsertValue>>,
}
impl core::fmt::Debug for PreparedGraphEdgeUpsert {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PreparedGraphEdgeUpsert")
            .field("on_match", &self.on_match.len())
            .field("on_create", &self.on_create.len())
            .field("after", &self.after.len())
            .field("definition", &"[REDACTED]")
            .finish()
    }
}
fn validate(
    branch: GraphEdgeUpsertBranch,
    actions: &[GraphEdgeUpsertAction<GraphEdgeUpsertValue>],
) -> Result<(), GraphEdgeUpsertBuildError> {
    if actions.len() > MAX_GRAPH_EDGE_UPSERT_ACTIONS {
        return Err(GraphEdgeUpsertBuildError::TooManyActions {
            branch,
            limit: MAX_GRAPH_EDGE_UPSERT_ACTIONS,
            observed: actions.len(),
        });
    }
    let mut keys = BTreeSet::new();
    for (index, action) in actions.iter().enumerate() {
        if !keys.insert(action.key) {
            return Err(GraphEdgeUpsertBuildError::DuplicateProperty { branch });
        }
        let properties = match &action.value {
            // A constant, literal or stored (fgdb-k3nub), reads no input.
            GraphEdgeUpsertValue::Literal(_) | GraphEdgeUpsertValue::Stored(_) => continue,
            GraphEdgeUpsertValue::Expression { properties, .. }
            | GraphEdgeUpsertValue::Composite { properties, .. } => properties,
        };
        {
            if properties.len() > MAX_GRAPH_EDGE_UPSERT_ACTIONS {
                return Err(GraphEdgeUpsertBuildError::TooManyInputs {
                    branch,
                    action: index,
                    limit: MAX_GRAPH_EDGE_UPSERT_ACTIONS,
                    observed: properties.len(),
                });
            }
            let valid = match &action.value {
                GraphEdgeUpsertValue::Expression { value, .. } => {
                    !value
                        .referenced_columns()
                        .any(|column| column >= properties.len())
                        && value.referenced_locals().next().is_none()
                }
                GraphEdgeUpsertValue::Composite { value, .. } => matches!(
                    crate::GraphSetProjection::admit_output(
                        value,
                        &vec![crate::GraphSetColumnType::Scalar; properties.len()],
                        index,
                    ),
                    Ok(crate::GraphSetColumnType::Scalar | crate::GraphSetColumnType::Any)
                ),
                GraphEdgeUpsertValue::Literal(_) | GraphEdgeUpsertValue::Stored(_) => {
                    unreachable!("constants skipped above")
                }
            };
            if !valid {
                return Err(GraphEdgeUpsertBuildError::InvalidExpressionInput {
                    branch,
                    action: index,
                });
            }
        }
    }
    Ok(())
}
impl PreparedGraphEdgeUpsert {
    pub fn prepare(
        merge: PreparedGraphEdgeMerge,
        on_match: Vec<GraphEdgeUpsertAction>,
        on_create: Vec<GraphEdgeUpsertAction>,
    ) -> Result<Self, GraphEdgeUpsertBuildError> {
        let literals = |actions: Vec<GraphEdgeUpsertAction>| {
            actions
                .into_iter()
                .map(|action| GraphEdgeUpsertAction {
                    key: action.key,
                    value: GraphEdgeUpsertValue::Literal(action.value),
                })
                .collect()
        };
        Self::prepare_with_clauses(merge, literals(on_match), literals(on_create), Vec::new())
    }

    /// Compile two alternative ON clauses and one subsequent SET clause.
    /// Duplicate fields within a clause refuse; overwrites BETWEEN clauses are
    /// retained as distinct writes. Validate every branch, even an unused one.
    pub fn prepare_with_clauses(
        merge: PreparedGraphEdgeMerge,
        on_match: Vec<GraphEdgeUpsertAction<GraphEdgeUpsertValue>>,
        on_create: Vec<GraphEdgeUpsertAction<GraphEdgeUpsertValue>>,
        after: Vec<GraphEdgeUpsertAction<GraphEdgeUpsertValue>>,
    ) -> Result<Self, GraphEdgeUpsertBuildError> {
        for (branch, actions) in [
            (GraphEdgeUpsertBranch::Match, &on_match),
            (GraphEdgeUpsertBranch::Create, &on_create),
        ] {
            validate(branch, actions)?;
            validate(branch, &after)?;
            let observed = actions.len() + after.len();
            if observed > MAX_GRAPH_EDGE_UPSERT_ACTIONS {
                return Err(GraphEdgeUpsertBuildError::TooManyActions {
                    branch,
                    limit: MAX_GRAPH_EDGE_UPSERT_ACTIONS,
                    observed,
                });
            }
        }
        Ok(Self {
            merge,
            on_match,
            on_create,
            after,
        })
    }
    #[must_use]
    pub fn merge(&self) -> &PreparedGraphEdgeMerge {
        &self.merge
    }
    #[must_use]
    pub fn on_match(&self) -> &[GraphEdgeUpsertAction<GraphEdgeUpsertValue>] {
        &self.on_match
    }
    #[must_use]
    pub fn on_create(&self) -> &[GraphEdgeUpsertAction<GraphEdgeUpsertValue>] {
        &self.on_create
    }
    #[must_use]
    pub fn after(&self) -> &[GraphEdgeUpsertAction<GraphEdgeUpsertValue>] {
        &self.after
    }
    #[must_use]
    pub fn action_count(&self, branch: GraphEdgeUpsertBranch) -> usize {
        match branch {
            GraphEdgeUpsertBranch::NoInput => 0,
            GraphEdgeUpsertBranch::Match => self.on_match.len() + self.after.len(),
            GraphEdgeUpsertBranch::Create => self.on_create.len() + self.after.len(),
        }
    }
    pub(crate) fn fixed_action_usage(
        &self,
        branch: GraphEdgeUpsertBranch,
    ) -> Option<crate::GlaExecutionStats> {
        use crate::algebra::GRAPH_VALUE_PAYLOAD_UNIT_BYTES;
        use fgdb_types::CanonicalScalar;
        let selected = match branch {
            GraphEdgeUpsertBranch::NoInput => {
                return Some(crate::GlaExecutionStats {
                    work_units: 1,
                    scratch_entries: 0,
                });
            }
            GraphEdgeUpsertBranch::Match => &self.on_match,
            GraphEdgeUpsertBranch::Create => &self.on_create,
        };
        let mut scratch = self.action_count(branch) as u64;
        for action in selected.iter().chain(&self.after) {
            let GraphEdgeUpsertValue::Literal(value) = &action.value else {
                return None;
            };
            let sizes = match value.value() {
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
            for bytes in sizes {
                scratch += bytes.div_ceil(GRAPH_VALUE_PAYLOAD_UNIT_BYTES) as u64;
            }
        }
        Some(crate::GlaExecutionStats {
            work_units: scratch + 1,
            scratch_entries: scratch,
        })
    }
    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut bytes = b"fgdb:directed-edge-upsert:v2\0".to_vec();
        let merge = self.merge.canonical_bytes();
        bytes.extend_from_slice(&(merge.len() as u64).to_be_bytes());
        bytes.extend_from_slice(&merge);
        for actions in [&self.on_match, &self.on_create, &self.after] {
            bytes.extend_from_slice(&(actions.len() as u64).to_be_bytes());
            for action in actions {
                bytes.extend_from_slice(&action.key.0.to_be_bytes());
                match &action.value {
                    GraphEdgeUpsertValue::Literal(value) => {
                        bytes.push(0);
                        bytes.extend_from_slice(
                            &(value.canonical_bytes().len() as u64).to_be_bytes(),
                        );
                        bytes.extend_from_slice(value.canonical_bytes());
                    }
                    GraphEdgeUpsertValue::Expression { properties, value } => {
                        bytes.push(1);
                        bytes.extend_from_slice(&(properties.len() as u64).to_be_bytes());
                        for key in properties {
                            bytes.extend_from_slice(&key.0.to_be_bytes());
                        }
                        let expression = value.canonical_bytes();
                        bytes.extend_from_slice(&(expression.len() as u64).to_be_bytes());
                        bytes.extend_from_slice(&expression);
                    }
                    GraphEdgeUpsertValue::Composite { properties, value } => {
                        bytes.push(2);
                        bytes.extend_from_slice(&(properties.len() as u64).to_be_bytes());
                        for key in properties {
                            bytes.extend_from_slice(&key.0.to_be_bytes());
                        }
                        let mut expression = Vec::new();
                        value.append_canonical_bytes(&mut expression);
                        bytes.extend_from_slice(&(expression.len() as u64).to_be_bytes());
                        bytes.extend_from_slice(&expression);
                    }
                    // Binding admitted the value through the stored format's
                    // bounds, so it encodes; tag 5 marks the impossible
                    // failure instead of a panic. Tag 2 is Composite's.
                    GraphEdgeUpsertValue::Stored(value) => match value.encode() {
                        Ok(encoded) => {
                            bytes.push(4);
                            bytes.extend_from_slice(&(encoded.len() as u64).to_be_bytes());
                            bytes.extend_from_slice(&encoded);
                        }
                        Err(_) => bytes.push(5),
                    },
                }
            }
        }
        bytes
    }
}

#[cfg(test)]
mod composite_tests {
    use super::*;
    use crate::algebra::GraphValue;
    use crate::{GlaExecutionEvent, GraphIntegerEvaluationError, GraphSetValue};
    use fgdb_types::CanonicalScalar;

    fn integer(value: i64) -> GraphSetValue {
        GraphSetValue::Value(GraphValue::Scalar(CanonicalScalar::Int(value)))
    }

    fn indexed(value: GraphSetValue) -> GraphSetValue {
        GraphSetValue::Index {
            list: Box::new(GraphSetValue::List(vec![value])),
            index: Box::new(integer(0)),
        }
    }

    fn action(value: GraphSetValue) -> GraphEdgeUpsertAction<GraphEdgeUpsertValue> {
        GraphEdgeUpsertAction {
            key: PropertyKeyId(2),
            value: GraphEdgeUpsertValue::Composite {
                properties: vec![PropertyKeyId(1)],
                value,
            },
        }
    }

    #[test]
    fn composite_admission_checks_nested_columns_locals_and_scalar_domain() {
        for branch in [GraphEdgeUpsertBranch::Match, GraphEdgeUpsertBranch::Create] {
            assert!(validate(branch, &[action(indexed(GraphSetValue::Column(0)))]).is_ok());
            for value in [
                indexed(GraphSetValue::Column(1)),
                indexed(GraphSetValue::Local(0)),
                GraphSetValue::List(vec![integer(1)]),
            ] {
                assert_eq!(
                    validate(branch, &[action(value)]),
                    Err(GraphEdgeUpsertBuildError::InvalidExpressionInput { branch, action: 0 }),
                );
            }
            let mut wide = action(integer(1));
            let GraphEdgeUpsertValue::Composite { properties, .. } = &mut wide.value else {
                unreachable!()
            };
            properties.resize(MAX_GRAPH_EDGE_UPSERT_ACTIONS + 1, PropertyKeyId(1));
            assert!(matches!(
                validate(branch, &[wide]),
                Err(GraphEdgeUpsertBuildError::TooManyInputs { .. })
            ));
        }
    }

    #[test]
    fn composite_evaluation_moves_scalar_values_and_retains_every_control_failure() {
        let value = action(indexed(GraphSetValue::Column(0))).value;
        let input = || vec![GraphValue::Scalar(CanonicalScalar::Int(42))];
        let mut events = Vec::new();
        let result = value
            .evaluate_with_control(input(), &mut |event| {
                events.push(event);
                Ok::<_, usize>(())
            })
            .unwrap();
        assert_eq!(result, GraphValue::Scalar(CanonicalScalar::Int(42)));
        assert!(events.contains(&GlaExecutionEvent::Work));
        assert!(events.contains(&GlaExecutionEvent::ScratchEntry));
        for stop in 0..events.len() {
            let mut at = 0;
            let result = value.evaluate_with_control(input(), &mut |_| {
                let current = at;
                at += 1;
                if current == stop { Err(stop) } else { Ok(()) }
            });
            assert!(
                matches!(result, Err(GraphIntegerEvaluationError::Control(found)) if found == stop)
            );
            assert_eq!(at, stop + 1);
        }
        // Index has an open result domain. Do not silently flatten a list
        // returned at run time: the native action owner must reject it.
        let dynamic = action(indexed(GraphSetValue::List(vec![integer(7)])));
        assert!(validate(GraphEdgeUpsertBranch::Match, std::slice::from_ref(&dynamic)).is_ok());
        let result = dynamic
            .value
            .evaluate_with_control(input(), &mut |_| Ok::<_, ()>(()))
            .unwrap();
        assert!(matches!(result, GraphValue::List(_)));
    }
}
