//! Connected schema-bound patterns lowered to the shared GLA evaluator.
//! Projection may return a vertex-ID set or correlated distinct binding rows.
//! This is a typed Rust surface, not another text grammar or a bag/path engine.

mod property_comparison;
mod value_projection;

use super::{BindingSlot, GlaDirection, GlaOperator, GlaPlan, GraphBindingRow, VertexPredicate};
use fgdb_delta_types::{LabelId, RelationId};
use fgdb_types::VId;
use property_comparison::PropertyComparison;

pub const MAX_PATTERN_EDGES: usize = 64;
pub const MAX_PATTERN_VERTICES: usize = MAX_PATTERN_EDGES + 1;
pub const MAX_PATTERN_PREDICATES: usize = 256;
pub const MAX_PATTERN_IDENTITIES: usize = 64;
pub const MAX_PATTERN_NAME_BYTES: usize = 128;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PatternLimitDimension {
    Vertices,
    Edges,
    Predicates,
    Identities,
    Columns,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PatternBuildError {
    EmptyPattern,
    InvalidVariableName,
    DuplicateVariable,
    UnknownVariable,
    Disconnected,
    EmptyProjection,
    DuplicateProjection,
    InvalidColumnName,
    RequiresValueProjection,
    LimitExceeded {
        dimension: PatternLimitDimension,
        limit: usize,
        observed: usize,
    },
}
impl core::fmt::Display for PatternBuildError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::EmptyPattern => f.write_str("graph pattern requires a vertex"),
            Self::InvalidVariableName => f.write_str("invalid graph-pattern variable name"),
            Self::DuplicateVariable => f.write_str("graph-pattern variable is already declared"),
            Self::UnknownVariable => f.write_str("graph pattern references an undeclared variable"),
            Self::Disconnected => {
                f.write_str("graph-pattern edge constraints must connect every declared vertex")
            }
            Self::EmptyProjection => f.write_str("binding projection requires a column"),
            Self::DuplicateProjection => f.write_str("binding projection repeats a column"),
            Self::InvalidColumnName => f.write_str("invalid graph-pattern column name"),
            Self::RequiresValueProjection => f.write_str(
                "binding property comparisons require prepare_values or its scoped variants",
            ),
            Self::LimitExceeded {
                dimension,
                limit,
                observed,
            } => write!(
                f,
                "graph-pattern {dimension:?} limit exceeded: {observed} > {limit}"
            ),
        }
    }
}
impl core::error::Error for PatternBuildError {}

#[derive(Clone)]
struct Variable {
    name: String,
    predicates: Vec<VertexPredicate>,
}
#[derive(Clone, Copy)]
struct Edge {
    source: usize,
    destination: usize,
    relation: RelationId,
    direction: GlaDirection,
}
#[derive(Clone, Copy)]
struct Identity {
    left: usize,
    right: usize,
    equal: bool,
}

/// Bounded definition metadata. Mutators validate before changing the builder.
#[derive(Clone, Default)]
pub struct GraphPatternBuilder {
    variables: Vec<Variable>,
    edges: Vec<Edge>,
    identities: Vec<Identity>,
    property_comparisons: Vec<PropertyComparison>,
    predicate_count: usize,
}
impl core::fmt::Debug for GraphPatternBuilder {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("GraphPatternBuilder")
            .field("variables", &self.variables.len())
            .field("edges", &self.edges.len())
            .field("predicates", &self.predicate_count)
            .field("identities", &self.identities.len())
            .field("definition", &"[REDACTED]")
            .finish()
    }
}

/// Immutable logical definition and ordered output schema. Names are retained
/// as column metadata, never substituted into execution. The default preserves
/// the original single-vertex API; tuple preparations carry GraphBindingRow.
#[derive(Clone, PartialEq, Eq)]
pub struct PreparedGraphPattern<Row = VId> {
    logical: GlaPlan<Row>,
    variable_count: usize,
    edge_count: usize,
    columns: Vec<String>,
}
impl<Row> core::fmt::Debug for PreparedGraphPattern<Row> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PreparedGraphPattern")
            .field("variables", &self.variable_count)
            .field("edges", &self.edge_count)
            .field("columns", &self.columns.len())
            .field("definition", &"[REDACTED]")
            .finish()
    }
}
impl<Row> PreparedGraphPattern<Row> {
    #[must_use]
    pub fn plan(&self) -> &GlaPlan<Row> {
        &self.logical
    }
    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        self.logical.canonical_bytes()
    }
    /// Output order. A row's `get(i)` refers to `columns()[i]`.
    #[must_use]
    pub fn columns(&self) -> &[String] {
        &self.columns
    }
    #[must_use]
    pub fn required_vertex_label(&self) -> Option<LabelId> {
        if self.logical.scans_edges() {
            return None;
        }
        self.logical.operators().iter().find_map(|op| match op {
            GlaOperator::Select { slot, predicates } if slot.ordinal() == 0 => {
                predicates.iter().find_map(|p| match p {
                    VertexPredicate::HasLabel(label) => Some(*label),
                    _ => None,
                })
            }
            _ => None,
        })
    }
}

fn check_next(
    count: usize,
    limit: usize,
    dimension: PatternLimitDimension,
) -> Result<(), PatternBuildError> {
    if count >= limit {
        return Err(PatternBuildError::LimitExceeded {
            dimension,
            limit,
            observed: count + 1,
        });
    }
    Ok(())
}
fn reverse(direction: GlaDirection) -> GlaDirection {
    match direction {
        GlaDirection::Forward => GlaDirection::Reverse,
        GlaDirection::Reverse => GlaDirection::Forward,
        GlaDirection::Undirected => GlaDirection::Undirected,
    }
}

impl GraphPatternBuilder {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
    fn variable(&self, name: &str) -> Result<usize, PatternBuildError> {
        self.variables
            .iter()
            .position(|var| var.name == name)
            .ok_or(PatternBuildError::UnknownVariable)
    }
    pub fn vertex(&mut self, name: &str) -> Result<&mut Self, PatternBuildError> {
        let bytes = name.as_bytes();
        if bytes.is_empty()
            || bytes.len() > MAX_PATTERN_NAME_BYTES
            || !(bytes[0].is_ascii_alphabetic() || bytes[0] == b'_')
            || !bytes
                .iter()
                .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
        {
            return Err(PatternBuildError::InvalidVariableName);
        }
        if self.variables.iter().any(|var| var.name == name) {
            return Err(PatternBuildError::DuplicateVariable);
        }
        check_next(
            self.variables.len(),
            MAX_PATTERN_VERTICES,
            PatternLimitDimension::Vertices,
        )?;
        self.variables.push(Variable {
            name: name.to_owned(),
            predicates: Vec::new(),
        });
        Ok(self)
    }
    pub fn filter(
        &mut self,
        variable: &str,
        predicate: VertexPredicate,
    ) -> Result<&mut Self, PatternBuildError> {
        let at = self.variable(variable)?;
        check_next(
            self.predicate_count,
            MAX_PATTERN_PREDICATES,
            PatternLimitDimension::Predicates,
        )?;
        self.variables[at].predicates.push(predicate);
        self.predicate_count += 1;
        Ok(self)
    }
    pub fn edge(
        &mut self,
        source: &str,
        relation: RelationId,
        direction: GlaDirection,
        destination: &str,
    ) -> Result<&mut Self, PatternBuildError> {
        let source = self.variable(source)?;
        let destination = self.variable(destination)?;
        check_next(
            self.edges.len(),
            MAX_PATTERN_EDGES,
            PatternLimitDimension::Edges,
        )?;
        self.edges.push(Edge {
            source,
            destination,
            relation,
            direction,
        });
        Ok(self)
    }
    pub fn identity(
        &mut self,
        left: &str,
        right: &str,
        equal: bool,
    ) -> Result<&mut Self, PatternBuildError> {
        let left = self.variable(left)?;
        let right = self.variable(right)?;
        check_next(
            self.identities.len(),
            MAX_PATTERN_IDENTITIES,
            PatternLimitDimension::Identities,
        )?;
        self.identities.push(Identity { left, right, equal });
        Ok(self)
    }
    fn select(&self, variable: usize, slot: BindingSlot, operators: &mut Vec<GlaOperator>) {
        if !self.variables[variable].predicates.is_empty() {
            operators.push(GlaOperator::Select {
                slot,
                predicates: self.variables[variable].predicates.clone(),
            });
        }
    }
    fn identities(
        &self,
        slots: &[Option<BindingSlot>],
        emitted: &mut [bool],
        operators: &mut Vec<GlaOperator>,
    ) {
        for (at, identity) in self.identities.iter().enumerate() {
            if !emitted[at]
                && let (Some(left), Some(right)) = (slots[identity.left], slots[identity.right])
            {
                operators.push(GlaOperator::VertexIdentity {
                    left,
                    right,
                    equal: identity.equal,
                });
                emitted[at] = true;
            }
        }
    }

    /// Retains the original sorted/distinct single-column result contract.
    pub fn prepare(
        &self,
        projection: &str,
        offset: u64,
        count: Option<u64>,
    ) -> Result<PreparedGraphPattern, PatternBuildError> {
        if self.variables.is_empty() {
            return Err(PatternBuildError::EmptyPattern);
        }
        let projection_at = self.variable(projection)?;
        self.require_identity_projection()?;
        let (mut operators, slots) = self.compile()?;
        operators.extend([
            GlaOperator::Project {
                slot: slots[projection_at],
            },
            GlaOperator::Distinct,
            GlaOperator::OrderByVertexId,
            GlaOperator::Limit { offset, count },
        ]);
        Ok(PreparedGraphPattern {
            logical: GlaPlan::from_operators(operators),
            variable_count: self.variables.len(),
            edge_count: self.edges.len(),
            columns: vec![projection.to_owned()],
        })
    }

    /// Prepare 1..=65 unique named columns from each complete matching binding.
    /// Tuple-wide DISTINCT and lexicographic order precede pagination. This is
    /// not an independent query per column, a Cartesian product, or bag output.
    pub fn prepare_bindings(
        &self,
        projections: &[&str],
        offset: u64,
        count: Option<u64>,
    ) -> Result<PreparedGraphPattern<GraphBindingRow>, PatternBuildError> {
        if self.variables.is_empty() {
            return Err(PatternBuildError::EmptyPattern);
        }
        if projections.is_empty() {
            return Err(PatternBuildError::EmptyProjection);
        }
        if projections.len() > MAX_PATTERN_VERTICES {
            return Err(PatternBuildError::LimitExceeded {
                dimension: PatternLimitDimension::Columns,
                limit: MAX_PATTERN_VERTICES,
                observed: projections.len(),
            });
        }
        let mut projected = Vec::new();
        for name in projections {
            let at = self.variable(name)?;
            if projected.contains(&at) {
                return Err(PatternBuildError::DuplicateProjection);
            }
            projected.push(at);
        }
        self.require_identity_projection()?;
        let (mut operators, slots) = self.compile()?;
        operators.extend([
            GlaOperator::ProjectBindings {
                slots: projected.into_iter().map(|at| slots[at]).collect(),
            },
            GlaOperator::Distinct,
            GlaOperator::OrderByBindings,
            GlaOperator::Limit { offset, count },
        ]);
        Ok(PreparedGraphPattern {
            logical: GlaPlan::from_operators(operators),
            variable_count: self.variables.len(),
            edge_count: self.edges.len(),
            columns: projections.iter().map(|name| (*name).to_owned()).collect(),
        })
    }

    /// Both output shapes share this connected-pattern compiler. Projection
    /// consumes its complete variable/slot map without changing traversal.
    fn compile(&self) -> Result<(Vec<GlaOperator>, Vec<BindingSlot>), PatternBuildError> {
        if self.variables.is_empty() {
            return Err(PatternBuildError::EmptyPattern);
        }
        let mut slots = vec![None; self.variables.len()];
        let mut emitted = vec![false; self.identities.len()];
        let mut operators = Vec::new();
        if self.edges.is_empty() {
            if self.variables.len() != 1 {
                return Err(PatternBuildError::Disconnected);
            }
            operators.push(GlaOperator::ScanVertices);
            slots[0] = Some(BindingSlot(0));
            self.identities(&slots, &mut emitted, &mut operators);
            self.select(0, BindingSlot(0), &mut operators);
        } else {
            let first = self.edges[0];
            operators.push(GlaOperator::ScanEdges {
                relation: first.relation,
                direction: first.direction,
            });
            slots[first.source] = Some(BindingSlot(0));
            if first.source == first.destination {
                operators.push(GlaOperator::VertexIdentity {
                    left: BindingSlot(0),
                    right: BindingSlot(1),
                    equal: true,
                });
            } else {
                slots[first.destination] = Some(BindingSlot(1));
            }
            self.identities(&slots, &mut emitted, &mut operators);
            self.select(first.source, BindingSlot(0), &mut operators);
            if first.source != first.destination {
                self.select(first.destination, BindingSlot(1), &mut operators);
            }
            let mut consumed = vec![false; self.edges.len()];
            consumed[0] = true;
            for next_slot in 2..=self.edges.len() as u32 {
                let at = self
                    .edges
                    .iter()
                    .enumerate()
                    .position(|(at, edge)| {
                        !consumed[at]
                            && (slots[edge.source].is_some() || slots[edge.destination].is_some())
                    })
                    .ok_or(PatternBuildError::Disconnected)?;
                let edge = self.edges[at];
                let (source, target, direction) = if let Some(source) = slots[edge.source] {
                    (source, edge.destination, edge.direction)
                } else {
                    (
                        slots[edge.destination].expect("a connected endpoint was found"),
                        edge.source,
                        reverse(edge.direction),
                    )
                };
                let appended = BindingSlot(next_slot);
                operators.push(GlaOperator::Expand {
                    source,
                    relation: edge.relation,
                    direction,
                });
                let previous = slots[target];
                if let Some(representative) = previous {
                    operators.push(GlaOperator::VertexIdentity {
                        left: representative,
                        right: appended,
                        equal: true,
                    });
                } else {
                    slots[target] = Some(appended);
                }
                self.identities(&slots, &mut emitted, &mut operators);
                if previous.is_none() {
                    self.select(target, appended, &mut operators);
                }
                consumed[at] = true;
            }
            if slots.iter().any(Option::is_none) {
                return Err(PatternBuildError::Disconnected);
            }
        }
        debug_assert!(emitted.iter().all(|emitted| *emitted));
        let slots: Vec<_> = slots
            .into_iter()
            .map(|slot| slot.expect("all variables are connected"))
            .collect();
        self.emit_property_comparisons(&slots, &mut operators);
        Ok((operators, slots))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{GqlQueryError, GqlQueryPolicy};
    use std::collections::BTreeSet;

    fn builder(names: &[&str]) -> GraphPatternBuilder {
        let mut builder = GraphPatternBuilder::new();
        for name in names {
            builder.vertex(name).unwrap();
        }
        builder
    }

    #[test]
    fn long_paths_branching_cycles_and_reversed_binding_use_existing_operators() {
        let mut b = builder(&["a", "b", "c", "d"]);
        b.edge("a", RelationId(1), GlaDirection::Forward, "b")
            .unwrap();
        b.edge("c", RelationId(2), GlaDirection::Reverse, "d")
            .unwrap();
        b.edge("c", RelationId(1), GlaDirection::Reverse, "b")
            .unwrap();
        b.edge("d", RelationId(2), GlaDirection::Forward, "a")
            .unwrap();
        b.edge("a", RelationId(1), GlaDirection::Forward, "c")
            .unwrap();
        b.identity("a", "d", false).unwrap();
        let query = b.prepare("d", 0, None).unwrap();
        let edges = [
            (VId(1), RelationId(1), VId(2)),
            (VId(2), RelationId(1), VId(3)),
            (VId(4), RelationId(2), VId(3)),
            (VId(4), RelationId(2), VId(1)),
            (VId(1), RelationId(1), VId(3)),
        ];
        assert_eq!(
            query
                .plan()
                .execute([], edges, |_, _| Ok::<_, ()>(true))
                .unwrap(),
            vec![VId(4)]
        );
        assert_eq!(
            query
                .plan()
                .operators()
                .iter()
                .filter(|op| matches!(op, GlaOperator::Expand { .. }))
                .count(),
            4
        );
    }

    // Complete assignment oracle; no GLA slots, indexes or scheduling code.
    fn assignments(b: &GraphPatternBuilder, edges: &[(VId, RelationId, VId)]) -> Vec<Vec<VId>> {
        let mut result = Vec::new();
        for bits in 0..(1_usize << b.variables.len()) {
            let assignment: Vec<_> = (0..b.variables.len())
                .map(|i| VId(1 + ((bits >> i) & 1) as u128))
                .collect();
            let identities = b
                .identities
                .iter()
                .all(|test| (assignment[test.left] == assignment[test.right]) == test.equal);
            let matches = b.edges.iter().all(|atom| {
                edges.iter().any(|&(s, r, d)| {
                    if r != atom.relation {
                        return false;
                    }
                    let left = assignment[atom.source];
                    let right = assignment[atom.destination];
                    match atom.direction {
                        GlaDirection::Forward => s == left && d == right,
                        GlaDirection::Reverse => d == left && s == right,
                        GlaDirection::Undirected => {
                            (s == left && d == right) || (s == right && d == left)
                        }
                    }
                })
            });
            if identities && matches {
                result.push(assignment);
            }
        }
        result
    }
    fn oracle(
        b: &GraphPatternBuilder,
        edges: &[(VId, RelationId, VId)],
        projected: usize,
    ) -> Vec<VId> {
        assignments(b, edges)
            .into_iter()
            .map(|row| row[projected])
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    #[test]
    fn connected_conjunctions_match_exhaustive_assignment_oracle() {
        let universe: Vec<_> = [RelationId(1), RelationId(2)]
            .into_iter()
            .flat_map(|r| {
                [VId(1), VId(2)]
                    .into_iter()
                    .flat_map(move |s| [VId(1), VId(2)].into_iter().map(move |d| (s, r, d)))
            })
            .collect();
        for mask in 0..256_usize {
            let mut edges: Vec<_> = universe
                .iter()
                .enumerate()
                .filter(|(i, _)| mask & (1 << i) != 0)
                .map(|(_, e)| *e)
                .collect();
            if let Some(first) = edges.first().copied() {
                edges.push(first);
            }
            for direction in [
                GlaDirection::Forward,
                GlaDirection::Reverse,
                GlaDirection::Undirected,
            ] {
                for shape in 0..3 {
                    let mut b = builder(&["a", "b", "c", "d"]);
                    b.edge("a", RelationId(1), direction, "b").unwrap();
                    b.edge("c", RelationId(2), direction, "d").unwrap();
                    b.edge("b", RelationId(1), GlaDirection::Reverse, "c")
                        .unwrap();
                    if shape >= 1 {
                        b.edge("d", RelationId(2), direction, "a").unwrap();
                    }
                    if shape == 2 {
                        b.identity("a", "c", false).unwrap();
                        b.edge("b", RelationId(1), direction, "b").unwrap();
                    }
                    for (projected, name) in ["a", "b", "c", "d"].into_iter().enumerate() {
                        let query = b.prepare(name, 0, None).unwrap();
                        let actual = query
                            .plan()
                            .execute([], edges.iter().copied(), |_, _| Ok::<_, ()>(true))
                            .unwrap();
                        assert_eq!(
                            actual,
                            oracle(&b, &edges, projected),
                            "mask={mask}, shape={shape}, direction={direction:?}"
                        );
                    }
                    // Tuple verification uses the same independent assignments,
                    // not a zip of the scalar oracle's independent projections.
                    for columns in [
                        vec!["a", "c"],
                        vec!["d", "b", "a"],
                        vec!["a", "b", "c", "d"],
                    ] {
                        let positions: Vec<_> = columns
                            .iter()
                            .map(|name| b.variable(name).unwrap())
                            .collect();
                        let expected: Vec<_> = assignments(&b, &edges)
                            .into_iter()
                            .map(|row| positions.iter().map(|at| row[*at]).collect::<Vec<_>>())
                            .collect::<BTreeSet<_>>()
                            .into_iter()
                            .skip(1)
                            .take(3)
                            .collect();
                        let query = b.prepare_bindings(&columns, 1, Some(3)).unwrap();
                        let actual = query
                            .plan()
                            .execute([], edges.iter().copied(), |_, _| Ok::<_, ()>(true))
                            .unwrap();
                        assert_eq!(
                            actual
                                .iter()
                                .map(|row| row.values().to_vec())
                                .collect::<Vec<_>>(),
                            expected,
                            "tuple mask={mask}, shape={shape}, direction={direction:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn validation_is_bounded_and_rejected_edits_leave_the_builder_unchanged() {
        let mut b = builder(&["a"]);
        let original = b.prepare("a", 0, None).unwrap();
        assert_eq!(
            b.vertex("a").unwrap_err(),
            PatternBuildError::DuplicateVariable
        );
        assert_eq!(
            b.vertex("$bad").unwrap_err(),
            PatternBuildError::InvalidVariableName
        );
        assert_eq!(
            b.edge("a", RelationId(1), GlaDirection::Forward, "missing")
                .unwrap_err(),
            PatternBuildError::UnknownVariable
        );
        assert_eq!(b.prepare("a", 0, None).unwrap(), original);
        assert_eq!(
            builder(&["a", "b"]).prepare("a", 0, None),
            Err(PatternBuildError::Disconnected)
        );
        assert_eq!(
            GraphPatternBuilder::new().prepare("a", 0, None),
            Err(PatternBuildError::EmptyPattern)
        );
        for _ in 0..MAX_PATTERN_EDGES {
            b.edge("a", RelationId(1), GlaDirection::Forward, "a")
                .unwrap();
        }
        let before = b.prepare("a", 0, None).unwrap();
        assert!(matches!(
            b.edge("a", RelationId(1), GlaDirection::Forward, "a"),
            Err(PatternBuildError::LimitExceeded {
                dimension: PatternLimitDimension::Edges,
                ..
            })
        ));
        assert_eq!(b.prepare("a", 0, None).unwrap(), before);
    }

    #[test]
    fn maximum_length_path_and_node_scan_keep_exact_ordered_pagination() {
        let mut b = GraphPatternBuilder::new();
        for i in 0..=MAX_PATTERN_EDGES {
            b.vertex(&format!("n{i}")).unwrap();
        }
        for i in 0..MAX_PATTERN_EDGES {
            b.edge(
                &format!("n{i}"),
                RelationId(1),
                GlaDirection::Forward,
                &format!("n{}", i + 1),
            )
            .unwrap();
        }
        let query = b
            .prepare(&format!("n{MAX_PATTERN_EDGES}"), 0, None)
            .unwrap();
        let edges =
            (0..MAX_PATTERN_EDGES).map(|i| (VId(i as u128), RelationId(1), VId(i as u128 + 1)));
        assert_eq!(
            query
                .plan()
                .execute([], edges, |_, _| Ok::<_, ()>(true))
                .unwrap(),
            vec![VId(MAX_PATTERN_EDGES as u128)]
        );
        let b = builder(&["a"]);
        let query = b.prepare("a", 1, Some(1)).unwrap();
        assert_eq!(
            query
                .plan()
                .execute([VId(3), VId(1), VId(1), VId(2)], [], |_, _| Ok::<_, ()>(
                    true
                ))
                .unwrap(),
            vec![VId(2)]
        );
        assert_eq!(query.required_vertex_label(), None);
    }

    #[test]
    fn renamed_patterns_preserve_transcripts_but_identity_and_filters_do_not() {
        let make = |names: [&str; 3]| {
            let mut b = builder(&names);
            b.edge(names[0], RelationId(1), GlaDirection::Forward, names[1])
                .unwrap();
            b.edge(names[1], RelationId(2), GlaDirection::Reverse, names[2])
                .unwrap();
            b.edge(names[2], RelationId(1), GlaDirection::Undirected, names[0])
                .unwrap();
            b.prepare(names[2], 0, None).unwrap()
        };
        assert_eq!(
            make(["a", "b", "c"]).canonical_bytes(),
            make(["x", "y", "z"]).canonical_bytes()
        );
        let mut b = builder(&["sensitive_name"]);
        let original = b.prepare("sensitive_name", 0, None).unwrap();
        b.filter("sensitive_name", VertexPredicate::HasLabel(LabelId(7)))
            .unwrap();
        let filtered = b.prepare("sensitive_name", 0, None).unwrap();
        assert_eq!(filtered.required_vertex_label(), Some(LabelId(7)));
        assert_ne!(filtered.canonical_bytes(), original.canonical_bytes());
        assert!(!format!("{b:?} {filtered:?}").contains("sensitive_name"));
    }

    #[test]
    fn general_patterns_share_governed_limits_and_every_interruption_checkpoint() {
        let mut b = builder(&["a", "b", "c", "d"]);
        for (s, d) in [("a", "b"), ("b", "c"), ("c", "d"), ("d", "a")] {
            b.edge(s, RelationId(1), GlaDirection::Undirected, d)
                .unwrap();
        }
        let query = b.prepare("d", 0, Some(1)).unwrap();
        let edges = [
            (VId(1), RelationId(1), VId(2)),
            (VId(2), RelationId(1), VId(3)),
        ];
        let wide = GqlQueryPolicy::new(2, 1, u64::MAX, u64::MAX);
        let mut calls = 0;
        let measured = query
            .plan()
            .execute_governed(
                2,
                [],
                edges,
                |_, _| Ok::<_, ()>(true),
                wide,
                || {
                    calls += 1;
                    Ok::<_, usize>(())
                },
            )
            .unwrap();
        assert_eq!(measured.value, vec![VId(1)]);
        for stop in 1..=calls {
            let mut at = 0;
            let result = query.plan().execute_governed(
                2,
                [],
                edges,
                |_, _| Ok::<_, ()>(true),
                wide,
                || {
                    at += 1;
                    if at == stop { Err(stop) } else { Ok(()) }
                },
            );
            assert!(matches!(result, Err(GqlQueryError::Interrupted(value)) if value == stop));
            assert_eq!(at, stop);
        }
        let exact = GqlQueryPolicy::new(
            2,
            1,
            measured.evaluator.work_units,
            measured.evaluator.scratch_entries,
        );
        assert_eq!(
            query
                .plan()
                .execute_governed(
                    2,
                    [],
                    edges,
                    |_, _| Ok::<_, ()>(true),
                    exact,
                    || Ok::<_, ()>(())
                )
                .unwrap(),
            measured
        );
        let short = GqlQueryPolicy::new(
            2,
            1,
            measured.evaluator.work_units - 1,
            measured.evaluator.scratch_entries,
        );
        assert!(matches!(
            query.plan().execute_governed(
                2,
                [],
                edges,
                |_, _| Ok::<_, ()>(true),
                short,
                || Ok::<_, ()>(())
            ),
            Err(GqlQueryError::Evaluator(_))
        ));
    }

    #[test]
    fn tuple_schema_and_column_order_are_immutable_and_validated() {
        let mut b = builder(&["a", "b"]);
        b.edge("a", RelationId(1), GlaDirection::Forward, "b")
            .unwrap();
        let tuple = b.prepare_bindings(&["b", "a"], 0, None).unwrap();
        assert_eq!(tuple.columns(), &["b".to_owned(), "a".to_owned()]);
        assert_ne!(
            tuple.canonical_bytes(),
            b.prepare_bindings(&["a", "b"], 0, None)
                .unwrap()
                .canonical_bytes()
        );
        assert_ne!(
            b.prepare("a", 0, None).unwrap().canonical_bytes(),
            b.prepare_bindings(&["a"], 0, None)
                .unwrap()
                .canonical_bytes()
        );
        assert_eq!(
            b.prepare_bindings(&[], 0, None).unwrap_err(),
            PatternBuildError::EmptyProjection
        );
        assert_eq!(
            b.prepare_bindings(&["a", "a"], 0, None).unwrap_err(),
            PatternBuildError::DuplicateProjection
        );
        assert_eq!(
            b.prepare_bindings(&["missing"], 0, None).unwrap_err(),
            PatternBuildError::UnknownVariable
        );
        assert!(matches!(
            b.prepare_bindings(&["a"; MAX_PATTERN_VERTICES + 1], 0, None),
            Err(PatternBuildError::LimitExceeded {
                dimension: PatternLimitDimension::Columns,
                ..
            })
        ));
        let frozen = tuple.canonical_bytes();
        b.identity("a", "b", false).unwrap();
        assert_eq!(tuple.canonical_bytes(), frozen);
    }

    #[test]
    fn all_sixty_five_columns_are_retained_without_a_lossy_scalar_conversion() {
        let names: Vec<_> = (0..=MAX_PATTERN_EDGES).map(|i| format!("n{i}")).collect();
        let refs: Vec<_> = names.iter().map(String::as_str).collect();
        let mut b = builder(&refs);
        for i in 0..MAX_PATTERN_EDGES {
            b.edge(refs[i], RelationId(1), GlaDirection::Forward, refs[i + 1])
                .unwrap();
        }
        let query = b.prepare_bindings(&refs, 0, None).unwrap();
        let rows = query
            .plan()
            .execute(
                [],
                (0..MAX_PATTERN_EDGES).map(|i| (VId(i as u128), RelationId(1), VId(i as u128 + 1))),
                |_, _| Ok::<_, ()>(true),
            )
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].values(),
            &(0..=MAX_PATTERN_EDGES)
                .map(|i| VId(i as u128))
                .collect::<Vec<_>>()
        );
    }
}
