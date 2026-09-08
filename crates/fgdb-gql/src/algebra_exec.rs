//! Scan-backed execution of the immutable bounded GLA pipeline.
//!
//! There is no BoundPlan match tree here. Sources admit one snapshot/overlay;
//! this module indexes requested relation/orientation pairs once, streams
//! binding rows through Select/Expand, and fuses the explicit terminal
//! Project/Distinct/OrderBy operators. No Cartesian intermediate is retained.

use crate::algebra::{GlaDirection, GlaOperator, GlaPlan, VertexPredicate};
use fgdb_delta_types::RelationId;
use fgdb_types::VId;
use std::collections::{BTreeMap, BTreeSet};

type Adjacency = BTreeMap<VId, Vec<VId>>;
type Index = BTreeMap<(RelationId, GlaDirection), Adjacency>;

fn build_index(
    operators: &[GlaOperator],
    edges: impl IntoIterator<Item = (VId, RelationId, VId)>,
) -> Index {
    let mut index = Index::new();
    for operator in operators {
        if let GlaOperator::ScanEdges { relation, direction }
        | GlaOperator::Expand {
            relation,
            direction,
            ..
        } = operator
        {
            index.entry((*relation, *direction)).or_default();
        }
    }
    // Only requested pairs are indexed. A same-relation two-hop uses one index.
    // Each input edge is admitted once, regardless of the number of expansions.
    for (source, relation, destination) in edges {
        for direction in [
            GlaDirection::Forward,
            GlaDirection::Reverse,
            GlaDirection::Undirected,
        ] {
            let Some(adjacency) = index.get_mut(&(relation, direction)) else {
                continue;
            };
            match direction {
                GlaDirection::Forward => adjacency.entry(source).or_default().push(destination),
                GlaDirection::Reverse => adjacency.entry(destination).or_default().push(source),
                GlaDirection::Undirected => {
                    adjacency.entry(source).or_default().push(destination);
                    if source != destination {
                        adjacency.entry(destination).or_default().push(source);
                    }
                }
            }
        }
    }
    for adjacency in index.values_mut() {
        for neighbors in adjacency.values_mut() {
            // Preserve parallel-edge multiplicity; only the final Distinct owns
            // duplicate elimination. Sorting makes visits input-order independent.
            neighbors.sort_unstable();
        }
    }
    index
}

struct Execution<F> {
    test_vertex: F,
    predicate_cache: BTreeMap<(usize, VId), bool>,
    projected: BTreeSet<VId>,
}

impl<F> Execution<F> {
    fn visit<E>(
        &mut self,
        operators: &[GlaOperator],
        ordinal: usize,
        bindings: &mut Vec<VId>,
        index: &Index,
    ) -> Result<(), E>
    where
        F: FnMut(VId, &[VertexPredicate]) -> Result<bool, E>,
    {
        let Some(operator) = operators.get(ordinal) else {
            return Ok(());
        };
        match operator {
            GlaOperator::Select { slot, predicates } => {
                let Some(vid) = bindings.get(slot.ordinal() as usize).copied() else {
                    return Ok(());
                };
                let key = (ordinal, vid);
                let keep = if let Some(keep) = self.predicate_cache.get(&key) {
                    *keep
                } else {
                    let keep = (self.test_vertex)(vid, predicates)?;
                    self.predicate_cache.insert(key, keep);
                    keep
                };
                if keep {
                    self.visit(operators, ordinal + 1, bindings, index)?;
                }
            }
            GlaOperator::VertexIdentity { left, right, equal } => {
                if let (Some(left), Some(right)) = (
                    bindings.get(left.ordinal() as usize),
                    bindings.get(right.ordinal() as usize),
                ) && (left == right) == *equal
                {
                    self.visit(operators, ordinal + 1, bindings, index)?;
                }
            }
            GlaOperator::Expand {
                source,
                relation,
                direction,
            } => {
                let Some(source) = bindings.get(source.ordinal() as usize).copied() else {
                    return Ok(());
                };
                if let Some(neighbors) = index
                    .get(&(*relation, *direction))
                    .and_then(|adjacency| adjacency.get(&source))
                {
                    for destination in neighbors {
                        bindings.push(*destination);
                        let result = self.visit(operators, ordinal + 1, bindings, index);
                        let _ = bindings.pop();
                        result?;
                    }
                }
            }
            GlaOperator::Project { slot } => {
                if let Some(vid) = bindings.get(slot.ordinal() as usize) {
                    self.projected.insert(*vid);
                }
            }
            // Only lower() can construct a GlaPlan. Scans occur at its root;
            // Project terminates its binding pipeline; the fixed distinct/order/
            // limit suffix is fused by the collector and execute() below.
            GlaOperator::Empty
            | GlaOperator::ScanVertices
            | GlaOperator::ScanEdges { .. }
            | GlaOperator::Distinct
            | GlaOperator::OrderByVertexId
            | GlaOperator::Limit { .. } => {}
        }
        Ok(())
    }
}

impl GlaPlan {
    /// Whether admission needs the edge table rather than the vertex table.
    #[must_use]
    pub fn scans_edges(&self) -> bool {
        matches!(self.operators().first(), Some(GlaOperator::ScanEdges { .. }))
    }

    /// Execute over already-admitted, immutable inputs. `test_vertex` evaluates
    /// the entire conjunction against ONE vertex version and propagates its
    /// original error. It is called at most once per Select/vertex pair.
    ///
    /// The caller owns snapshot admission, authorization, resource limits and
    /// transaction read-dependency tracking. These in-memory scratch indexes
    /// are not a spill-capable or authorized Strata/FreeJoin implementation.
    /// No result is returned if any requested predicate read fails.
    pub fn execute<E>(
        &self,
        vertices: impl IntoIterator<Item = VId>,
        edges: impl IntoIterator<Item = (VId, RelationId, VId)>,
        test_vertex: impl FnMut(VId, &[VertexPredicate]) -> Result<bool, E>,
    ) -> Result<Vec<VId>, E> {
        let operators = self.operators();
        let index = if self.scans_edges() {
            build_index(operators, edges)
        } else {
            Index::new()
        };
        let mut execution = Execution {
            test_vertex,
            predicate_cache: BTreeMap::new(),
            projected: BTreeSet::new(),
        };
        let mut bindings = Vec::new();
        match operators.first() {
            Some(GlaOperator::ScanVertices) => {
                for vid in vertices {
                    bindings.clear();
                    bindings.push(vid);
                    execution.visit(operators, 1, &mut bindings, &index)?;
                }
            }
            Some(GlaOperator::ScanEdges { relation, direction }) => {
                if let Some(adjacency) = index.get(&(*relation, *direction)) {
                    for (source, destinations) in adjacency {
                        for destination in destinations {
                            bindings.clear();
                            bindings.extend([*source, *destination]);
                            execution.visit(operators, 1, &mut bindings, &index)?;
                        }
                    }
                }
            }
            _ => {}
        }
        let (offset, count) = match operators.last() {
            Some(GlaOperator::Limit { offset, count }) => (
                usize::try_from(*offset).unwrap_or(usize::MAX),
                count
                    .and_then(|count| usize::try_from(count).ok())
                    .unwrap_or(usize::MAX),
            ),
            _ => (0, usize::MAX),
        };
        Ok(execution
            .projected
            .into_iter()
            .skip(offset)
            .take(count)
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BoundPlan, EdgeDirection, RelationBind, ReturnProjection};
    use fgdb_delta_types::{LabelId, PropertyKeyId};
    use fgdb_types::CanonicalScalar;
    use std::cell::Cell;

    fn bound(two_hops: bool) -> BoundPlan {
        let bind = RelationBind::new()
            .with_relation("R", RelationId(1))
            .with_relation("S", RelationId(2));
        bind.bind(if two_hops {
            "MATCH (a)-[:R]->(b)-[:S]->(c) RETURN c"
        } else {
            "MATCH (a)-[:R]->(b) RETURN b"
        })
        .unwrap()
    }

    fn orient(
        source: VId,
        destination: VId,
        direction: EdgeDirection,
        two: bool,
    ) -> Vec<(VId, VId)> {
        match direction {
            EdgeDirection::Incoming if two => vec![(destination, source)],
            EdgeDirection::Undirected if source != destination => {
                vec![(source, destination), (destination, source)]
            }
            _ => vec![(source, destination)],
        }
    }

    // Independent nested-loop bag oracle; it never uses the index, lowered
    // operators, predicate cache, or execution collector.
    fn reference(plan: &BoundPlan, edges: &[(VId, RelationId, VId)]) -> Vec<VId> {
        let mut bag = Vec::new();
        for &(source, relation, destination) in edges {
            if Some(relation) != plan.relation {
                continue;
            }
            for (a, b) in orient(
                source,
                destination,
                plan.direction,
                plan.hop2_relation.is_some(),
            ) {
                if plan.neq.is_some() && a == b || plan.eq.is_some() && a != b {
                    continue;
                }
                if let Some(hop2) = plan.hop2_relation {
                    for &(source2, relation2, destination2) in edges {
                        if relation2 != hop2 {
                            continue;
                        }
                        for (via, c) in orient(source2, destination2, plan.direction, true) {
                            if b == via {
                                bag.push(match plan.projection {
                                    ReturnProjection::Source => a,
                                    ReturnProjection::Destination => b,
                                    ReturnProjection::Hop2Destination => c,
                                });
                            }
                        }
                    }
                } else {
                    bag.push(if plan.projection == ReturnProjection::Source {
                        a
                    } else {
                        b
                    });
                }
            }
        }
        bag.sort_unstable();
        bag.dedup();
        bag.into_iter()
            .skip(plan.skip.unwrap_or(0) as usize)
            .take(plan.limit.unwrap_or(u64::MAX) as usize)
            .collect()
    }

    #[test]
    fn exhaustive_small_multigraph_matches_independent_path_enumeration() {
        let universe = [
            (VId(1), RelationId(1), VId(1)),
            (VId(1), RelationId(1), VId(2)),
            (VId(1), RelationId(1), VId(2)),
            (VId(2), RelationId(1), VId(3)),
            (VId(2), RelationId(2), VId(1)),
            (VId(2), RelationId(2), VId(3)),
            (VId(3), RelationId(2), VId(3)),
        ];
        for mask in 0..(1_u32 << universe.len()) {
            let edges: Vec<_> = universe
                .iter()
                .enumerate()
                .filter(|(i, _)| mask & (1_u32 << *i) != 0)
                .map(|(_, edge)| *edge)
                .collect();
            for two_hops in [false, true] {
                for direction in [
                    EdgeDirection::Outgoing,
                    EdgeDirection::Incoming,
                    EdgeDirection::Undirected,
                ] {
                    for projection in [
                        ReturnProjection::Source,
                        ReturnProjection::Destination,
                        ReturnProjection::Hop2Destination,
                    ] {
                        let mut plan = bound(two_hops);
                        plan.direction = direction;
                        plan.projection = projection;
                        for identity in 0..3 {
                            plan.eq = (identity == 1).then(|| ("a".into(), "b".into()));
                            plan.neq = (identity == 2).then(|| ("a".into(), "b".into()));
                            let actual = GlaPlan::lower(&plan)
                                .execute([], edges.iter().copied(), |_, _| Ok::<_, ()>(true))
                                .unwrap();
                            assert_eq!(actual, reference(&plan, &edges), "mask={mask}, plan={plan:?}");
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn input_is_consumed_once_and_predicates_are_memoized() {
        let mut plan = bound(false);
        plan.dst_prop = Some((PropertyKeyId(4), 7));
        let admitted = Cell::new(0);
        let reads = Cell::new(0);
        let edges = (0..100).map(|_| {
            admitted.set(admitted.get() + 1);
            (VId(1), RelationId(1), VId(2))
        });
        let rows = GlaPlan::lower(&plan)
            .execute([], edges, |vid, predicates| {
                reads.set(reads.get() + 1);
                assert_eq!(vid, VId(2));
                Ok::<_, ()>(predicates.iter().all(|predicate| {
                    predicate.matches(&[], &[(PropertyKeyId(4), CanonicalScalar::Int(7))])
                }))
            })
            .unwrap();
        assert_eq!(rows, vec![VId(2)]);
        assert_eq!(admitted.get(), 100);
        assert_eq!(reads.get(), 1);
    }

    #[test]
    fn a_late_source_failure_never_returns_partial_results() {
        let mut plan = bound(false);
        plan.dst_label = Some(LabelId(1));
        let result = GlaPlan::lower(&plan).execute(
            [],
            [
                (VId(1), RelationId(1), VId(2)),
                (VId(1), RelationId(1), VId(3)),
            ],
            |vid, _| {
                if vid == VId(3) {
                    Err("unreadable vertex")
                } else {
                    Ok(true)
                }
            },
        );
        assert_eq!(result, Err("unreadable vertex"));
    }

    #[test]
    fn distinct_and_order_precede_skip_and_limit() {
        let mut plan = bound(false);
        plan.skip = Some(1);
        plan.limit = Some(1);
        let edges = [
            (VId(1), RelationId(1), VId(3)),
            (VId(1), RelationId(1), VId(2)),
            (VId(1), RelationId(1), VId(2)),
        ];
        assert_eq!(
            GlaPlan::lower(&plan)
                .execute([], edges, |_, _| Ok::<_, ()>(true))
                .unwrap(),
            vec![VId(3)]
        );
        plan.skip = Some(u64::MAX);
        assert!(
            GlaPlan::lower(&plan)
                .execute([], edges, |_, _| Ok::<_, ()>(true))
                .unwrap()
                .is_empty()
        );
    }
}
