//! Factor parallel topology occurrences before the SAME GLA binding visitor.
//!
//! For a positive pattern, a complete vertex assignment has one independent
//! edge choice per edge atom. Its bag weight is the product of the admitted
//! endpoint-triple multiplicities, even when an atom/variable repeats. COUNT
//! adds that weight; COUNT DISTINCT and vertex MIN/MAX use positive support.
//! No edge-identity/trail semantics are claimed by this vertex-only profile.
//!
//! Eligibility is structural, not adaptive: no predicates, property columns,
//! property-pair reads or scoped operators can be skipped. A relation used
//! undirected must be exclusively undirected; opposite stored directions then
//! contribute to ONE unordered pair. Mixed directed/undirected uses fall back.
//! Sources are consumed once. Preprocessing, retained keys, multiplicity reads
//! and downstream execution share the original control/error path. This is a
//! bounded factorization law, not general FreeJoin, a COLT, or spill storage.

mod tree;

use super::*;
use crate::algebra::GlaDirection;
use core::num::NonZeroU64;

type TopologyKey = (VId, RelationId, VId);
type VisitError<E, C> = GqlQueryError<GraphAggregateError<E>, C>;

/// None proves a POSITIVE multiplicity exceeds u64::MAX. It is never a capped
/// numeric answer. Overflow matters only when a completed witness contributes
/// to COUNT: a doomed path or support-only summary must not fail prematurely.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct Multiplicity(Option<NonZeroU64>);

impl Multiplicity {
    pub(super) const ONE: Self = Self(Some(NonZeroU64::MIN));

    pub(super) fn exact_count(self) -> Option<u64> {
        self.0.map(NonZeroU64::get)
    }

    fn increment(self) -> Self {
        Self(self.exact_count().and_then(|n| n.checked_add(1)).and_then(NonZeroU64::new))
    }

    fn product(self, other: Self) -> Self {
        Self(self.exact_count().zip(other.exact_count())
            .and_then(|(a, b)| a.checked_mul(b)).and_then(NonZeroU64::new))
    }

    fn sum(self, other: Self) -> Self {
        Self(self.exact_count().zip(other.exact_count())
            .and_then(|(a, b)| a.checked_add(b)).and_then(NonZeroU64::new))
    }
}

#[derive(Clone, Copy)]
struct EdgeAccess {
    source: usize,
    destination: usize,
    relation: RelationId,
    direction: GlaDirection,
}

fn eligible(aggregate: &PreparedGraphAggregate) -> bool {
    let operators = aggregate.input.plan().operators();
    if !matches!(operators.first(), Some(GlaOperator::ScanEdges { .. }))
        || aggregate.aggregates.iter().any(|a| !matches!(a.function,
            GraphAggregateFunction::CountRows | GraphAggregateFunction::Count
            | GraphAggregateFunction::CountDistinct | GraphAggregateFunction::Min
            | GraphAggregateFunction::Max))
    {
        return false;
    }
    operators.iter().all(|operator| match operator {
        GlaOperator::ScanEdges { relation, direction }
        | GlaOperator::Expand { relation, direction, .. } => {
            operators.iter().all(|other| match other {
                GlaOperator::ScanEdges { relation: other_relation, direction: other_direction }
                | GlaOperator::Expand { relation: other_relation, direction: other_direction, .. }
                    if relation == other_relation => {
                    (*direction == GlaDirection::Undirected)
                        == (*other_direction == GlaDirection::Undirected)
                }
                _ => true,
            })
        }
        GlaOperator::VertexIdentity { .. }
        | GlaOperator::OrderByValues
        | GlaOperator::Limit { offset: 0, count: None } => true,
        GlaOperator::ProjectValues { columns } => columns.iter()
            .all(|column| matches!(column, ValueProjection::Vertex { .. })),
        _ => false,
    })
}

fn normalized(source: VId, relation: RelationId, destination: VId, undirected: bool) -> TopologyKey {
    if undirected && source > destination {
        (destination, relation, source)
    } else {
        (source, relation, destination)
    }
}

/// Ordinary/weighted inputs converge on visit_value_bindings, not a second
/// matcher. The callback receives one exact weight or an explicit above-count
/// state. Its accumulator decides whether that state matters to its function.
#[allow(clippy::too_many_arguments)]
pub(super) fn visit_bindings<'a, E, C, F, R, M, P>(
    aggregate: &PreparedGraphAggregate,
    vertices: impl IntoIterator<Item = VId>,
    edges: impl IntoIterator<Item = (VId, RelationId, VId)>,
    test_vertex: F,
    property: R,
    mut control: M,
    mut visit: P,
) -> Result<(), VisitError<E, C>>
where
    F: FnMut(VId, &[VertexPredicate]) -> Result<bool, VisitError<E, C>>,
    R: FnMut(VId, PropertyKeyId) -> Result<Option<&'a CanonicalScalar>, VisitError<E, C>>,
    M: FnMut(GlaExecutionEvent) -> Result<(), VisitError<E, C>>,
    P: FnMut(&[ValueProjection], &[Option<VId>], &mut R, &mut M, Multiplicity)
        -> Result<(), VisitError<E, C>>,
{
    let plan = aggregate.input.plan();
    if !eligible(aggregate) {
        return plan.visit_value_bindings(vertices, edges, test_vertex, property, control,
            |columns, bindings, property, control| {
                visit(columns, bindings, property, control, Multiplicity::ONE)
            });
    }

    let mut accesses = Vec::new();
    let mut undirected = BTreeSet::new();
    let mut next_slot = 2;
    for operator in plan.operators() {
        control(GlaExecutionEvent::Work)?;
        let access = match operator {
            GlaOperator::ScanEdges { relation, direction } => EdgeAccess {
                source: 0, destination: 1, relation: *relation, direction: *direction,
            },
            GlaOperator::Expand { source, relation, direction } => {
                let access = EdgeAccess {
                    source: source.ordinal() as usize, destination: next_slot,
                    relation: *relation, direction: *direction,
                };
                next_slot += 1;
                access
            }
            _ => continue,
        };
        control(GlaExecutionEvent::ScratchEntry)?;
        accesses.push(access);
        if access.direction == GlaDirection::Undirected && !undirected.contains(&access.relation) {
            control(GlaExecutionEvent::ScratchEntry)?;
            undirected.insert(access.relation);
        }
    }

    let mut topology: BTreeMap<TopologyKey, Multiplicity> = BTreeMap::new();
    for (source, relation, destination) in edges {
        control(GlaExecutionEvent::Work)?;
        let key = normalized(source, relation, destination, undirected.contains(&relation));
        if let Some(count) = topology.get_mut(&key) {
            *count = count.increment();
        } else {
            // One key/weight entry, before retaining the record. The original
            // snapshot-record allowance was already checked against the full
            // source, not this smaller derived relation.
            control(GlaExecutionEvent::ScratchEntry)?;
            topology.insert(key, Multiplicity::ONE);
        }
    }

    // Remove only a compiler-proved unprojected forest. Messages sum distinct
    // child assignments and multiply independent branches, without generating
    // their Cartesian product. Cyclic/projected core bindings still use GLA.
    // Both partitions preserve original edge order. All slots are remapped:
    // removed ones occupy a separate tail, so early branches cannot alias the
    // compact IDs of retained later variables in the completion-message maps.
    let reduced = plan.aggregate_core(&mut control)?;
    let mut branches = Vec::new();
    if let Some(core) = &reduced {
        let mut retained = 0;
        for at in 0..accesses.len() {
            control(GlaExecutionEvent::Work)?;
            let mut access = accesses[at];
            let unavailable = || GqlQueryError::Source(GraphAggregateError::MultiplicityUnavailable);
            access.source = core.slot_map.get(access.source).ok_or_else(unavailable)?.ordinal() as usize;
            access.destination = core.slot_map.get(access.destination).ok_or_else(unavailable)?.ordinal() as usize;
            if access.destination >= core.retained_width {
                control(GlaExecutionEvent::ScratchEntry)?;
                branches.push(access);
            } else {
                debug_assert!(access.source < core.retained_width);
                accesses[retained] = access;
                retained += 1;
            }
        }
        accesses.truncate(retained);
    }
    let forest = tree::Forest::build(&branches, &topology, &mut control)?;
    let execution_plan = reduced.as_ref().map_or(plan, |core| &core.plan);
    execution_plan.visit_value_bindings(vertices, topology.keys().copied(), test_vertex, property, control,
        |columns, bindings, property, control| {
            let Some(mut weight) = forest.completion(bindings, control)? else {
                return Ok(());
            };
            for access in &accesses {
                control(GlaExecutionEvent::Work)?;
                let unavailable = || GqlQueryError::Source(GraphAggregateError::MultiplicityUnavailable);
                let source = bindings.get(access.source).copied().flatten().ok_or_else(unavailable)?;
                let destination = bindings.get(access.destination).copied().flatten().ok_or_else(unavailable)?;
                let key = match access.direction {
                    GlaDirection::Forward => (source, access.relation, destination),
                    GlaDirection::Reverse => (destination, access.relation, source),
                    GlaDirection::Undirected => normalized(source, access.relation, destination, true),
                };
                let factor = topology.get(&key).copied().ok_or_else(unavailable)?;
                weight = weight.product(factor);
            }
            visit(columns, bindings, property, control, weight)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{GqlParameters, GraphSymbol, GraphSymbolKind, PreparedGraphAggregateText};

    fn query(text: &str) -> PreparedGraphAggregate {
        PreparedGraphAggregateText::prepare(text, |kind, _| match kind {
            GraphSymbolKind::Relation => Some(GraphSymbol::Relation(RelationId(1))),
            GraphSymbolKind::Property => Some(GraphSymbol::Property(PropertyKeyId(1))),
            GraphSymbolKind::Label => Some(GraphSymbol::Label(fgdb_delta_types::LabelId(1))),
        }).unwrap().bind_parameters(&GqlParameters::new()).unwrap()
    }

    #[test]
    fn positive_count_weights_never_wrap_or_become_zero() {
        let max = Multiplicity(NonZeroU64::new(u64::MAX));
        assert_eq!(max.exact_count(), Some(u64::MAX));
        assert_eq!(max.increment().exact_count(), None);
        assert_eq!(max.product(Multiplicity::ONE).exact_count(), Some(u64::MAX));
        let two = Multiplicity::ONE.increment();
        assert_eq!(max.product(two).exact_count(), None);
        let mut weight = Multiplicity::ONE;
        for _ in 0..63 { weight = weight.product(two); }
        assert_eq!(weight.exact_count(), Some(1_u64 << 63));
        weight = weight.product(two);
        assert_eq!(weight.exact_count(), None);
        assert_eq!(weight.product(Multiplicity::ONE).exact_count(), None);
    }

    #[test]
    fn only_the_registered_topology_support_and_count_profile_is_factored() {
        for text in [
            "MATCH (a)-[:R]->(b) RETURN a,COUNT(*) AS n,COUNT(b) AS m,COUNT(DISTINCT b) AS d,MIN(b) AS lo,MAX(b) AS hi GROUP BY a",
            "MATCH (a)-[:R]-(b)-[:R]-(a) RETURN COUNT(*) AS n",
            "MATCH (a)-[:R]->(b)<-[:R]-(c) WHERE a<>c RETURN COUNT(*) AS n",
        ] { assert!(eligible(&query(text)), "{text}"); }
        for text in [
            "MATCH (a) RETURN COUNT(*) AS n",
            "MATCH (a)-[:R]->(b) WHERE a.n=1 RETURN COUNT(*) AS n",
            "MATCH (a:L)-[:R]->(b) RETURN COUNT(*) AS n",
            "MATCH (a)-[:R]->(b) WHERE a.n=b.n RETURN COUNT(*) AS n",
            "MATCH (a)-[:R]->(b) RETURN COUNT(b.n) AS n",
            "MATCH (a)-[:R]->(b) RETURN SUM(b) AS n",
            "MATCH (a)-[:R]->(b)-[:R]-(c) RETURN COUNT(*) AS n",
            "MATCH (a)-[:R]->(b) OPTIONAL MATCH (b)-[:R]->(c) RETURN COUNT(*) AS n",
            "MATCH (a)-[:R]->(b) WHERE EXISTS { MATCH (b)-[:R]->(c) } RETURN COUNT(*) AS n",
        ] { assert!(!eligible(&query(text)), "{text}"); }
    }
}
