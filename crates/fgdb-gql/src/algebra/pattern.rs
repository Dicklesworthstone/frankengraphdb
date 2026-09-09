//! Connected, schema-bound graph patterns lowered to the existing GLA pipeline.
//!
//! This is a typed Rust surface, not another text grammar or executor. It
//! admits finite conjunctive patterns and returns the existing sorted/distinct
//! single-vertex projection. Edge occurrences may be reused; no TRAIL, path
//! value, optional match, Cartesian product, or general GQL bag claim is made.

use super::{BindingSlot, GlaDirection, GlaOperator, GlaPlan, VertexPredicate};
use fgdb_delta_types::{LabelId, RelationId};

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
}

/// Errors contain structural information, not names, IDs or predicate values.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PatternBuildError {
    EmptyPattern,
    InvalidVariableName,
    DuplicateVariable,
    UnknownVariable,
    Disconnected,
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
            Self::Disconnected => f.write_str("graph-pattern edge constraints must connect every declared vertex"),
            Self::LimitExceeded { dimension, limit, observed } => {
                write!(f, "graph-pattern {dimension:?} limit exceeded: {observed} > {limit}")
            }
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

/// Bounded preparation metadata. IDs have already been resolved by the caller;
/// preparation neither infers schema names nor grants access to a graph.
/// Each mutator validates before changing the builder. Declaration order and
/// edge order choose a deterministic plan, without statistics or adaptivity.
#[derive(Clone, Default)]
pub struct GraphPatternBuilder {
    variables: Vec<Variable>,
    edges: Vec<Edge>,
    identities: Vec<Identity>,
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

/// An immutable, connected logical definition ready for a pinned read. Names
/// do not survive lowering. The canonical transcript is application identity,
/// not a durable plan certificate, authorization token or prepared-session ID.
#[derive(Clone, PartialEq, Eq)]
pub struct PreparedGraphPattern {
    logical: GlaPlan,
    variable_count: usize,
    edge_count: usize,
}

impl core::fmt::Debug for PreparedGraphPattern {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PreparedGraphPattern")
            .field("variables", &self.variable_count)
            .field("edges", &self.edge_count)
            .field("definition", &"[REDACTED]")
            .finish()
    }
}

impl PreparedGraphPattern {
    #[must_use]
    pub fn plan(&self) -> &GlaPlan {
        &self.logical
    }

    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        self.logical.canonical_bytes()
    }

    /// A required positive label is a sound insertion witness for a node-only
    /// scan. No label means a deliberate all-vertex scan, not an empty plan.
    #[must_use]
    pub fn required_vertex_label(&self) -> Option<LabelId> {
        if self.logical.scans_edges() {
            return None;
        }
        self.logical.operators().iter().find_map(|op| match op {
            GlaOperator::Select { slot, predicates } if slot.ordinal() == 0 => {
                predicates.iter().find_map(|p| match p {
                    VertexPredicate::HasLabel(label) => Some(*label),
                    VertexPredicate::IntegerProperty { .. } => None,
                })
            }
            _ => None,
        })
    }
}

fn check_next(count: usize, limit: usize, dimension: PatternLimitDimension) -> Result<(), PatternBuildError> {
    if count >= limit {
        return Err(PatternBuildError::LimitExceeded { dimension, limit, observed: count + 1 });
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
        self.variables.iter().position(|var| var.name == name)
            .ok_or(PatternBuildError::UnknownVariable)
    }

    pub fn vertex(&mut self, name: &str) -> Result<&mut Self, PatternBuildError> {
        let bytes = name.as_bytes();
        if bytes.is_empty() || bytes.len() > MAX_PATTERN_NAME_BYTES
            || !(bytes[0].is_ascii_alphabetic() || bytes[0] == b'_')
            || !bytes.iter().all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
        {
            return Err(PatternBuildError::InvalidVariableName);
        }
        if self.variables.iter().any(|var| var.name == name) {
            return Err(PatternBuildError::DuplicateVariable);
        }
        check_next(self.variables.len(), MAX_PATTERN_VERTICES, PatternLimitDimension::Vertices)?;
        self.variables.push(Variable { name: name.to_owned(), predicates: Vec::new() });
        Ok(self)
    }

    pub fn filter(&mut self, variable: &str, predicate: VertexPredicate) -> Result<&mut Self, PatternBuildError> {
        let at = self.variable(variable)?;
        check_next(self.predicate_count, MAX_PATTERN_PREDICATES, PatternLimitDimension::Predicates)?;
        self.variables[at].predicates.push(predicate);
        self.predicate_count += 1;
        Ok(self)
    }

    /// Forward means source -> destination; Reverse means source <- destination.
    /// Each edge may choose its own direction. Reusing a variable is an identity
    /// constraint; distinct variable names may still bind the same vertex.
    pub fn edge(
        &mut self,
        source: &str,
        relation: RelationId,
        direction: GlaDirection,
        destination: &str,
    ) -> Result<&mut Self, PatternBuildError> {
        let source = self.variable(source)?;
        let destination = self.variable(destination)?;
        check_next(self.edges.len(), MAX_PATTERN_EDGES, PatternLimitDimension::Edges)?;
        self.edges.push(Edge { source, destination, relation, direction });
        Ok(self)
    }

    /// Equality/inequality between any declared vertices, not only adjacent ones.
    pub fn identity(&mut self, left: &str, right: &str, equal: bool) -> Result<&mut Self, PatternBuildError> {
        let left = self.variable(left)?;
        let right = self.variable(right)?;
        check_next(self.identities.len(), MAX_PATTERN_IDENTITIES, PatternLimitDimension::Identities)?;
        self.identities.push(Identity { left, right, equal });
        Ok(self)
    }

    fn select(&self, variable: usize, slot: BindingSlot, operators: &mut Vec<GlaOperator>) {
        if !self.variables[variable].predicates.is_empty() {
            operators.push(GlaOperator::Select { slot, predicates: self.variables[variable].predicates.clone() });
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
                operators.push(GlaOperator::VertexIdentity { left, right, equal: identity.equal });
                emitted[at] = true;
            }
        }
    }

    /// Compile connected positive edge constraints into Scan/Expand/Select and
    /// explicit identities, then Project/Distinct/Order/Limit. No path occurrence
    /// is truncated to fit a limit. A zero count is a legitimate empty result.
    ///
    /// At most 64 edge atoms bound expansion depth, independently of graph size.
    /// Unsupported disconnected patterns refuse rather than dropping a component
    /// or silently introducing a Cartesian product. This is not FreeJoin, TRAIL,
    /// shortest-path selection, or the complete GQL pattern grammar.
    pub fn prepare(
        &self,
        projection: &str,
        offset: u64,
        count: Option<u64>,
    ) -> Result<PreparedGraphPattern, PatternBuildError> {
        if self.variables.is_empty() {
            return Err(PatternBuildError::EmptyPattern);
        }
        let projection = self.variable(projection)?;
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
            operators.push(GlaOperator::ScanEdges { relation: first.relation, direction: first.direction });
            slots[first.source] = Some(BindingSlot(0));
            if first.source == first.destination {
                operators.push(GlaOperator::VertexIdentity {
                    left: BindingSlot(0), right: BindingSlot(1), equal: true,
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
            let mut next_slot = 2_u32;
            for _ in 1..self.edges.len() {
                let at = self.edges.iter().enumerate().position(|(at, edge)| {
                    !consumed[at] && (slots[edge.source].is_some() || slots[edge.destination].is_some())
                }).ok_or(PatternBuildError::Disconnected)?;
                let edge = self.edges[at];
                let (source, target, direction) = if let Some(source) = slots[edge.source] {
                    (source, edge.destination, edge.direction)
                } else {
                    (slots[edge.destination].expect("a connected endpoint was found"), edge.source, reverse(edge.direction))
                };
                let appended = BindingSlot(next_slot);
                next_slot += 1;
                operators.push(GlaOperator::Expand { source, relation: edge.relation, direction });
                let previous = slots[target];
                if let Some(representative) = previous {
                    operators.push(GlaOperator::VertexIdentity { left: representative, right: appended, equal: true });
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
        operators.extend([
            GlaOperator::Project { slot: slots[projection].expect("all variables are connected") },
            GlaOperator::Distinct,
            GlaOperator::OrderByVertexId,
            GlaOperator::Limit { offset, count },
        ]);
        Ok(PreparedGraphPattern {
            logical: GlaPlan { operators }, variable_count: self.variables.len(), edge_count: self.edges.len(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{GqlQueryError, GqlQueryPolicy};
    use fgdb_types::VId;
    use std::collections::BTreeSet;

    fn builder(names: &[&str]) -> GraphPatternBuilder {
        let mut builder = GraphPatternBuilder::new();
        for name in names { builder.vertex(name).unwrap(); }
        builder
    }

    #[test]
    fn long_paths_branching_cycles_and_reversed_binding_use_existing_operators() {
        let mut b = builder(&["a", "b", "c", "d"]);
        b.edge("a", RelationId(1), GlaDirection::Forward, "b").unwrap();
        // Neither endpoint is initially bound: the planner must defer this atom.
        b.edge("c", RelationId(2), GlaDirection::Reverse, "d").unwrap();
        b.edge("c", RelationId(1), GlaDirection::Reverse, "b").unwrap();
        b.edge("d", RelationId(2), GlaDirection::Forward, "a").unwrap();
        b.edge("a", RelationId(1), GlaDirection::Forward, "c").unwrap();
        b.identity("a", "d", false).unwrap();
        let query = b.prepare("d", 0, None).unwrap();
        let edges = [
            (VId(1), RelationId(1), VId(2)),
            (VId(2), RelationId(1), VId(3)),
            (VId(4), RelationId(2), VId(3)),
            (VId(4), RelationId(2), VId(1)),
            (VId(1), RelationId(1), VId(3)),
        ];
        assert_eq!(query.plan().execute([], edges, |_, _| Ok::<_, ()>(true)).unwrap(), vec![VId(4)]);
        assert_eq!(query.plan().operators().iter().filter(|op| matches!(op, GlaOperator::Expand { .. })).count(), 4);
    }

    // Exhaustive assignment oracle: no GLA slots, orientation helper, index,
    // variable visitation order, or lowering code is shared with production.
    fn oracle(b: &GraphPatternBuilder, edges: &[(VId, RelationId, VId)], projected: usize) -> Vec<VId> {
        let mut result = BTreeSet::new();
        for bits in 0..(1_usize << b.variables.len()) {
            let assignment: Vec<_> = (0..b.variables.len()).map(|i| VId(1 + ((bits >> i) & 1) as u128)).collect();
            let identities = b.identities.iter().all(|test| (assignment[test.left] == assignment[test.right]) == test.equal);
            let matches = b.edges.iter().all(|atom| edges.iter().any(|&(s, r, d)| {
                if r != atom.relation { return false; }
                let left = assignment[atom.source];
                let right = assignment[atom.destination];
                match atom.direction {
                    GlaDirection::Forward => s == left && d == right,
                    GlaDirection::Reverse => d == left && s == right,
                    GlaDirection::Undirected => (s == left && d == right) || (s == right && d == left),
                }
            }));
            if identities && matches { result.insert(assignment[projected]); }
        }
        result.into_iter().collect()
    }

    #[test]
    fn connected_conjunctions_match_exhaustive_assignment_oracle() {
        let universe: Vec<_> = [RelationId(1), RelationId(2)].into_iter().flat_map(|r| {
            [VId(1), VId(2)].into_iter().flat_map(move |s| [VId(1), VId(2)].into_iter().map(move |d| (s, r, d)))
        }).collect();
        for mask in 0..256_usize {
            let mut edges: Vec<_> = universe.iter().enumerate().filter(|(i, _)| mask & (1 << i) != 0).map(|(_, e)| *e).collect();
            // Parallel edges must not change this single-column set contract.
            if let Some(first) = edges.first().copied() { edges.push(first); }
            for direction in [GlaDirection::Forward, GlaDirection::Reverse, GlaDirection::Undirected] {
                for shape in 0..3 {
                    let mut b = builder(&["a", "b", "c", "d"]);
                    b.edge("a", RelationId(1), direction, "b").unwrap();
                    b.edge("c", RelationId(2), direction, "d").unwrap();
                    b.edge("b", RelationId(1), GlaDirection::Reverse, "c").unwrap();
                    if shape >= 1 { b.edge("d", RelationId(2), direction, "a").unwrap(); }
                    if shape == 2 { b.identity("a", "c", false).unwrap(); b.edge("b", RelationId(1), direction, "b").unwrap(); }
                    for (projected, name) in ["a", "b", "c", "d"].into_iter().enumerate() {
                        let query = b.prepare(name, 0, None).unwrap();
                        let actual = query.plan().execute([], edges.iter().copied(), |_, _| Ok::<_, ()>(true)).unwrap();
                        assert_eq!(actual, oracle(&b, &edges, projected), "mask={mask}, shape={shape}, direction={direction:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn validation_is_bounded_and_rejected_edits_leave_the_builder_unchanged() {
        let mut b = builder(&["a"]);
        let original = b.prepare("a", 0, None).unwrap();
        assert_eq!(b.vertex("a").unwrap_err(), PatternBuildError::DuplicateVariable);
        assert_eq!(b.vertex("$bad").unwrap_err(), PatternBuildError::InvalidVariableName);
        assert_eq!(b.edge("a", RelationId(1), GlaDirection::Forward, "missing").unwrap_err(), PatternBuildError::UnknownVariable);
        assert_eq!(b.prepare("a", 0, None).unwrap(), original);
        assert_eq!(builder(&["a", "b"]).prepare("a", 0, None), Err(PatternBuildError::Disconnected));
        assert_eq!(GraphPatternBuilder::new().prepare("a", 0, None), Err(PatternBuildError::EmptyPattern));
        for _ in 0..MAX_PATTERN_EDGES { b.edge("a", RelationId(1), GlaDirection::Forward, "a").unwrap(); }
        let before = b.prepare("a", 0, None).unwrap();
        assert!(matches!(b.edge("a", RelationId(1), GlaDirection::Forward, "a"), Err(PatternBuildError::LimitExceeded { dimension: PatternLimitDimension::Edges, .. })));
        assert_eq!(b.prepare("a", 0, None).unwrap(), before);
    }

    #[test]
    fn maximum_length_path_and_node_scan_keep_exact_ordered_pagination() {
        let mut b = GraphPatternBuilder::new();
        for i in 0..=MAX_PATTERN_EDGES { b.vertex(&format!("n{i}")).unwrap(); }
        for i in 0..MAX_PATTERN_EDGES { b.edge(&format!("n{i}"), RelationId(1), GlaDirection::Forward, &format!("n{}", i + 1)).unwrap(); }
        let query = b.prepare(&format!("n{MAX_PATTERN_EDGES}"), 0, None).unwrap();
        let edges = (0..MAX_PATTERN_EDGES).map(|i| (VId(i as u128), RelationId(1), VId(i as u128 + 1)));
        assert_eq!(query.plan().execute([], edges, |_, _| Ok::<_, ()>(true)).unwrap(), vec![VId(MAX_PATTERN_EDGES as u128)]);
        let b = builder(&["a"]);
        let query = b.prepare("a", 1, Some(1)).unwrap();
        assert_eq!(query.plan().execute([VId(3), VId(1), VId(1), VId(2)], [], |_, _| Ok::<_, ()>(true)).unwrap(), vec![VId(2)]);
        assert_eq!(query.required_vertex_label(), None);
    }

    #[test]
    fn renamed_patterns_preserve_transcripts_but_identity_and_filters_do_not() {
        let make = |names: [&str; 3]| {
            let mut b = builder(&names);
            b.edge(names[0], RelationId(1), GlaDirection::Forward, names[1]).unwrap();
            b.edge(names[1], RelationId(2), GlaDirection::Reverse, names[2]).unwrap();
            b.edge(names[2], RelationId(1), GlaDirection::Undirected, names[0]).unwrap();
            b.prepare(names[2], 0, None).unwrap()
        };
        assert_eq!(make(["a", "b", "c"]).canonical_bytes(), make(["x", "y", "z"]).canonical_bytes());
        let mut b = builder(&["sensitive_name"]);
        let original = b.prepare("sensitive_name", 0, None).unwrap();
        b.filter("sensitive_name", VertexPredicate::HasLabel(LabelId(7))).unwrap();
        let filtered = b.prepare("sensitive_name", 0, None).unwrap();
        assert_eq!(filtered.required_vertex_label(), Some(LabelId(7)));
        assert_ne!(filtered.canonical_bytes(), original.canonical_bytes());
        assert!(!format!("{b:?} {filtered:?}").contains("sensitive_name"));
    }

    #[test]
    fn general_patterns_share_governed_limits_and_every_interruption_checkpoint() {
        let mut b = builder(&["a", "b", "c", "d"]);
        for (s, d) in [("a", "b"), ("b", "c"), ("c", "d"), ("d", "a")] { b.edge(s, RelationId(1), GlaDirection::Undirected, d).unwrap(); }
        let query = b.prepare("d", 0, Some(1)).unwrap();
        let edges = [(VId(1), RelationId(1), VId(2)), (VId(2), RelationId(1), VId(3))];
        let wide = GqlQueryPolicy::new(2, 1, u64::MAX, u64::MAX);
        let mut calls = 0;
        let measured = query.plan().execute_governed(2, [], edges, |_, _| Ok::<_, ()>(true), wide, || { calls += 1; Ok::<_, usize>(()) }).unwrap();
        assert_eq!(measured.value, vec![VId(1)]);
        for stop in 1..=calls {
            let mut at = 0;
            let result = query.plan().execute_governed(2, [], edges, |_, _| Ok::<_, ()>(true), wide, || {
                at += 1; if at == stop { Err(stop) } else { Ok(()) }
            });
            assert!(matches!(result, Err(GqlQueryError::Interrupted(value)) if value == stop));
            assert_eq!(at, stop);
        }
        let exact = GqlQueryPolicy::new(2, 1, measured.evaluator.work_units, measured.evaluator.scratch_entries);
        assert_eq!(query.plan().execute_governed(2, [], edges, |_, _| Ok::<_, ()>(true), exact, || Ok::<_, ()>(())).unwrap(), measured);
        let short = GqlQueryPolicy::new(2, 1, measured.evaluator.work_units - 1, measured.evaluator.scratch_entries);
        assert!(matches!(query.plan().execute_governed(2, [], edges, |_, _| Ok::<_, ()>(true), short, || Ok::<_, ()>(())), Err(GqlQueryError::Evaluator(_))));
    }
}
