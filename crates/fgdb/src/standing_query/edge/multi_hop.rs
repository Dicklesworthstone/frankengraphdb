//! Fixed-hop GLA joins maintained from affected edge occurrences.
//!
//! ScanEdges followed by Expand forms a tree of binding slots, even when
//! VertexIdentity closes a graph cycle. Anchor a changed edge at each matching
//! atom and join outwards through relation/endpoint arrangements. A complete
//! binding belongs to its FIRST affected atom, so simultaneous changes and
//! self-joins emit it once, not once per changed edge. Old and final inputs are
//! enumerated separately: their difference includes every delta cross term.
//!
//! A single correlated OPTIONAL/EXISTS/NOT EXISTS child uses the same anchored
//! joins. Complete child occurrences feed the shared scope witness derivative;
//! partial paths never count as witnesses. Only registration/rebuild scans all
//! input edges/roots. Normal ticks inspect changed identities, incident edges of
//! changed vertex projections, and indexed join partners reached from anchors.
//! This is an in-memory derived arrangement, not graph storage, spill,
//! variable-length recursion, a durable subscription or arbitrary nested scopes.

use super::*;
use fgdb_gql::algebra::MAX_PATTERN_EDGES;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Atom {
    left: usize,
    right: usize,
    relation: RelationId,
    direction: GlaDirection,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct Shape {
    atoms: Vec<Atom>,
    width: usize,
    relations: BTreeSet<RelationId>,
    scope: Option<scoped::Shape>,
}

impl Shape {
    pub(super) fn of(query: &PreparedGraphAggregate) -> Option<Self> {
        if let Some(scope) = scoped::Shape::multi_hop(query) {
            // The shared scope recognizer validated every operator and the
            // frame boundary. Slot 0 is the outer root; slot 1 is its copied
            // child identity. The atoms form a connected tree over slots 1..
            // and slot 0 aliases slot 1 only when the full binding is emitted.
            let mut atoms = Vec::new();
            let mut width = 2;
            for op in scope.body(query) {
                if let GlaOperator::Expand {
                    source,
                    relation,
                    direction,
                } = op
                {
                    atoms.push(Atom {
                        left: source.ordinal() as usize,
                        right: width,
                        relation: *relation,
                        direction: *direction,
                    });
                    width += 1;
                }
            }
            if width != scope.width() || atoms.len() < 2 {
                return None;
            }
            let relations = atoms.iter().map(|atom| atom.relation).collect();
            return Some(Self {
                atoms,
                width,
                relations,
                scope: Some(scope),
            });
        }
        let operators = query.input_pattern().plan().operators();
        let GlaOperator::ScanEdges {
            relation,
            direction,
        } = operators.first()?
        else {
            return None;
        };
        let mut atoms = vec![Atom {
            left: 0,
            right: 1,
            relation: *relation,
            direction: *direction,
        }];
        let mut width = 2;
        let mut projected = false;
        for op in &operators[1..] {
            match op {
                GlaOperator::Expand {
                    source,
                    relation,
                    direction,
                } if !projected
                    && (source.ordinal() as usize) < width
                    && atoms.len() < MAX_PATTERN_EDGES =>
                {
                    atoms.push(Atom {
                        left: source.ordinal() as usize,
                        right: width,
                        relation: *relation,
                        direction: *direction,
                    });
                    width += 1;
                }
                GlaOperator::Select { slot, predicates }
                    if !projected && (slot.ordinal() as usize) < width =>
                {
                    if !predicates.iter().all(|predicate| {
                        matches!(
                            predicate,
                            VertexPredicate::HasLabel(_)
                                | VertexPredicate::IntegerProperty { .. }
                                | VertexPredicate::ScalarProperty { .. }
                                | VertexPredicate::PropertyNull { .. }
                        )
                    }) {
                        return None;
                    }
                }
                GlaOperator::VertexIdentity { left, right, .. }
                | GlaOperator::CompareProperties { left, right, .. }
                    if !projected
                        && (left.ordinal() as usize) < width
                        && (right.ordinal() as usize) < width => {}
                GlaOperator::SelectBoolean { expression }
                    if !projected && expression.supports_vertex_bindings(width) => {}
                GlaOperator::ProjectValues { columns } if !projected => {
                    if columns.iter().any(|column| {
                        !matches!(column,
                        ValueProjection::Vertex { slot } | ValueProjection::Property { slot, .. }
                            if (slot.ordinal() as usize) < width)
                    }) {
                        return None;
                    }
                    projected = true;
                }
                GlaOperator::OrderByValues
                | GlaOperator::Limit {
                    offset: 0,
                    count: None,
                } if projected => {}
                _ => return None,
            }
        }
        if !projected || atoms.len() < 2 {
            return None;
        }
        let relations = atoms.iter().map(|atom| atom.relation).collect();
        Some(Self {
            atoms,
            width,
            relations,
            scope: None,
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Edge {
    src: VId,
    relation: RelationId,
    dst: VId,
}

fn orientations(edge: Edge, direction: GlaDirection) -> [Option<Endpoints>; 2] {
    match direction {
        GlaDirection::Forward => [Some((edge.src, edge.dst)), None],
        GlaDirection::Reverse => [Some((edge.dst, edge.src)), None],
        GlaDirection::Undirected => [
            Some((edge.src, edge.dst)),
            (edge.src != edge.dst).then_some((edge.dst, edge.src)),
        ],
    }
}

#[derive(Default, PartialEq, Eq)]
struct Arrangement {
    edges: BTreeMap<EId, Edge>,
    incident: BTreeMap<(RelationId, VId), BTreeSet<EId>>,
}

impl Arrangement {
    fn insert(&mut self, eid: EId, edge: Edge) {
        self.edges.insert(eid, edge);
        self.incident
            .entry((edge.relation, edge.src))
            .or_default()
            .insert(eid);
        if edge.src != edge.dst {
            self.incident
                .entry((edge.relation, edge.dst))
                .or_default()
                .insert(eid);
        }
    }

    fn remove(&mut self, eid: EId) {
        if let Some(edge) = self.edges.remove(&eid) {
            for vid in [edge.src, edge.dst] {
                if let std::collections::btree_map::Entry::Occupied(mut entry) =
                    self.incident.entry((edge.relation, vid))
                {
                    entry.get_mut().remove(&eid);
                    if entry.get().is_empty() {
                        entry.remove();
                    }
                }
                if edge.src == edge.dst {
                    break;
                }
            }
        }
    }
}

#[derive(PartialEq, Eq)]
pub(super) struct State {
    shape: Shape,
    input: Arrangement,
    witnesses: BTreeMap<VId, u64>,
}

impl core::fmt::Debug for State {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("StandingMultiHopInput")
            .field("atoms", &self.shape.atoms.len())
            .field("edge_count", &self.input.edges.len())
            .field("data", &"[REDACTED]")
            .finish()
    }
}

pub(super) struct Patch {
    created: Arrangement,
    removed: BTreeSet<EId>,
    witnesses: BTreeMap<VId, u64>,
}

// Borrow the prospective graph without cloning the retained arrangement.
// Deletions shadow both old and newly created identities (including cascades).
struct Overlay<'a> {
    base: &'a Arrangement,
    created: &'a Arrangement,
    removed: &'a BTreeSet<EId>,
}

impl Overlay<'_> {
    fn get(&self, eid: EId) -> Option<Edge> {
        if self.removed.contains(&eid) {
            return None;
        }
        self.created
            .edges
            .get(&eid)
            .or_else(|| self.base.edges.get(&eid))
            .copied()
    }

    fn incident(&self, relation: RelationId, vid: VId) -> impl Iterator<Item = EId> + '_ {
        self.base
            .incident
            .get(&(relation, vid))
            .into_iter()
            .flatten()
            .chain(
                self.created
                    .incident
                    .get(&(relation, vid))
                    .into_iter()
                    .flatten(),
            )
            .copied()
    }
}

struct Enumeration<'a> {
    query: &'a PreparedGraphAggregate,
    shape: &'a Shape,
    graph: Overlay<'a>,
    vertices: &'a Vertices,
    vertex_patch: &'a VertexPatch,
    affected: Option<&'a BTreeSet<EId>>,
    sign: i128,
}

impl Enumeration<'_> {
    fn run(
        &self,
        counts: &mut BTreeMap<VId, i128>,
        output: &mut Vec<grouped::Contribution>,
        meter: &mut Meter<'_>,
    ) -> Result<(), StandingQueryFailure> {
        meter.units(
            ZSetEvent::ScratchEntry,
            2 + self.shape.width + self.shape.atoms.len(),
        )?;
        let mut binding = vec![None; self.shape.width];
        let mut selected = vec![None; self.shape.atoms.len()];
        for anchor in 0..self.shape.atoms.len() {
            meter.charge(ZSetEvent::Work)?;
            if let Some(affected) = self.affected {
                for &eid in affected {
                    self.anchor(
                        anchor,
                        eid,
                        &mut binding,
                        &mut selected,
                        counts,
                        output,
                        meter,
                    )?;
                }
            } else {
                // Bootstrap owns each binding at atom zero, regardless of EId.
                for &eid in self
                    .graph
                    .base
                    .edges
                    .keys()
                    .chain(self.graph.created.edges.keys())
                {
                    self.anchor(
                        anchor,
                        eid,
                        &mut binding,
                        &mut selected,
                        counts,
                        output,
                        meter,
                    )?;
                }
                break;
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn anchor(
        &self,
        anchor: usize,
        eid: EId,
        binding: &mut [Option<VId>],
        selected: &mut [Option<EId>],
        counts: &mut BTreeMap<VId, i128>,
        output: &mut Vec<grouped::Contribution>,
        meter: &mut Meter<'_>,
    ) -> Result<(), StandingQueryFailure> {
        meter.charge(ZSetEvent::Work)?;
        let Some(edge) = self.graph.get(eid) else {
            return Ok(());
        };
        let atom = self.shape.atoms[anchor];
        if atom.relation != edge.relation {
            return Ok(());
        }
        for (left, right) in orientations(edge, atom.direction).into_iter().flatten() {
            meter.charge(ZSetEvent::Work)?;
            binding[atom.left] = Some(left);
            binding[atom.right] = Some(right);
            selected[anchor] = Some(eid);
            self.walk(anchor, binding, selected, counts, output, meter)?;
            selected[anchor] = None;
            binding[atom.left] = None;
            binding[atom.right] = None;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn walk(
        &self,
        anchor: usize,
        binding: &mut [Option<VId>],
        selected: &mut [Option<EId>],
        counts: &mut BTreeMap<VId, i128>,
        output: &mut Vec<grouped::Contribution>,
        meter: &mut Meter<'_>,
    ) -> Result<(), StandingQueryFailure> {
        let mut pending = false;
        for (index, atom) in self.shape.atoms.iter().enumerate() {
            meter.charge(ZSetEvent::Work)?;
            if selected[index].is_some() {
                continue;
            }
            pending = true;
            let (known, vacant) = match (binding[atom.left], binding[atom.right]) {
                (Some(_), None) => (atom.left, atom.right),
                (None, Some(_)) => (atom.right, atom.left),
                _ => continue,
            };
            let vid = binding[known].ok_or(StandingQueryFailure::InvalidDelta)?;
            for eid in self.graph.incident(atom.relation, vid) {
                meter.charge(ZSetEvent::Work)?;
                // Even a removed candidate consumes work before it is skipped.
                let Some(edge) = self.graph.get(eid) else {
                    continue;
                };
                // The earliest affected atom owns this entire occurrence.
                // Prune before following its remaining join partners.
                if index < anchor && self.affected.is_some_and(|set| set.contains(&eid)) {
                    continue;
                }
                for (left, right) in orientations(edge, atom.direction).into_iter().flatten() {
                    meter.charge(ZSetEvent::Work)?;
                    if (if known == atom.left { left } else { right }) != vid {
                        continue;
                    }
                    binding[vacant] = Some(if vacant == atom.left { left } else { right });
                    selected[index] = Some(eid);
                    self.walk(anchor, binding, selected, counts, output, meter)?;
                    selected[index] = None;
                    binding[vacant] = None;
                }
            }
            return Ok(());
        }
        if pending {
            return Err(StandingQueryFailure::InvalidDelta);
        }
        meter.units(ZSetEvent::ScratchEntry, 1 + binding.len())?;
        let mut row = Vec::new();
        for (slot, vid) in binding.iter().enumerate() {
            meter.charge(ZSetEvent::Work)?;
            // The outer slot is an alias, not another scanned vertex or atom.
            // Resolve it in this same old OR final source generation.
            let vid = if slot == 0 && self.shape.scope.is_some() {
                binding[1]
            } else {
                *vid
            };
            let vid = vid.ok_or(StandingQueryFailure::InvalidDelta)?;
            row.push((vid, vertex(self.vertices, self.vertex_patch, vid)?));
        }
        // Reuse typed GLA predicate/projectors and the scope derivative. NULL
        // extension and semi/anti presence happen once per root after all paths.
        match self.shape.scope {
            Some(scope) => {
                scope.binding_contribution(self.query, &row, self.sign, counts, output, meter)
            }
            None => grouped::binding_contributions(self.query, &row, self.sign, output, meter),
        }
    }
}

impl State {
    pub(super) fn for_definition(query: &PreparedGraphAggregate) -> Option<Self> {
        Some(Self {
            shape: Shape::of(query)?,
            input: Arrangement::default(),
            witnesses: BTreeMap::new(),
        })
    }

    #[cfg(test)]
    pub(super) fn has_scope(&self) -> bool {
        self.shape.scope.is_some()
    }

    pub(super) fn seed(
        &mut self,
        row: &fgdb_strata::AdjacencyEntry,
        vertices: &Vertices,
        meter: &mut Meter<'_>,
    ) -> Result<(), StandingQueryFailure> {
        meter.charge(ZSetEvent::Work)?;
        if !self.shape.relations.contains(&row.relation) {
            return Ok(());
        }
        if self.input.edges.contains_key(&row.eid)
            || !vertices.contains_key(&row.src)
            || !vertices.contains_key(&row.dst)
        {
            return Err(StandingQueryFailure::InvalidDelta);
        }
        reserve_edge((row.src, row.dst), meter)?;
        self.input.insert(
            row.eid,
            Edge {
                src: row.src,
                relation: row.relation,
                dst: row.dst,
            },
        );
        Ok(())
    }

    pub(super) fn finish_seed(
        &mut self,
        query: &PreparedGraphAggregate,
        vertices: &Vertices,
        output: &mut Vec<grouped::Contribution>,
        meter: &mut Meter<'_>,
    ) -> Result<(), StandingQueryFailure> {
        let empty = Arrangement::default();
        let removed = BTreeSet::new();
        let vertex_patch = BTreeMap::new();
        let mut counts = BTreeMap::new();
        Enumeration {
            query,
            shape: &self.shape,
            graph: Overlay {
                base: &self.input,
                created: &empty,
                removed: &removed,
            },
            vertices,
            vertex_patch: &vertex_patch,
            affected: None,
            sign: 1,
        }
        .run(&mut counts, output, meter)?;
        if let Some(scope) = self.shape.scope {
            let mut witnesses = BTreeMap::new();
            for (root, count) in counts {
                meter.charge(ZSetEvent::Work)?;
                let count = u64::try_from(count).map_err(|_| StandingQueryFailure::Arithmetic)?;
                if count != 0 {
                    meter.charge(ZSetEvent::ScratchEntry)?;
                    witnesses.insert(root, count);
                }
            }
            // Bootstrap emits each root once; there was no previous NULL or
            // anti row to retract. Isolated roots are essential here.
            for (&vid, state) in vertices {
                scope.root_contribution(
                    query,
                    vid,
                    Some(state),
                    witnesses.get(&vid).copied().unwrap_or(0),
                    1,
                    output,
                    meter,
                )?;
            }
            (meter.checkpoint)()?;
            self.witnesses = witnesses;
        }
        Ok(())
    }

    pub(super) fn prepare(
        &self,
        query: &PreparedGraphAggregate,
        batch: &LogicalDeltaBatch,
        vertices: &Vertices,
        staged: &VertexPatch,
        output: &mut Vec<grouped::Contribution>,
        meter: &mut Meter<'_>,
    ) -> Result<Patch, StandingQueryFailure> {
        let mut affected = BTreeSet::new();
        for &vid in staged.keys() {
            for &relation in &self.shape.relations {
                meter.charge(ZSetEvent::Work)?;
                if let Some(edges) = self.input.incident.get(&(relation, vid)) {
                    for &eid in edges {
                        touch(&mut affected, eid, meter)?;
                    }
                }
            }
        }
        let mut created = Arrangement::default();
        let mut removed = BTreeSet::new();
        // Coordinates are canonical ordering, not an execution schedule. A
        // cascade in one coordinate may name a creation in a later relation.
        for entry in batch.coordinate_entries() {
            meter.charge(ZSetEvent::Work)?;
            if entry.graph != crate::GRAPH || entry.branch != crate::BRANCH {
                continue;
            }
            for row in &entry.rows {
                meter.charge(ZSetEvent::Work)?;
                if let DeltaRow::CreateEdge {
                    eid,
                    src,
                    relation,
                    dst,
                    ..
                } = row
                {
                    if *relation != entry.relation {
                        return Err(StandingQueryFailure::InvalidDelta);
                    }
                    if !self.shape.relations.contains(relation) {
                        continue;
                    }
                    if self.input.edges.contains_key(eid) || created.edges.contains_key(eid) {
                        return Err(StandingQueryFailure::InvalidDelta);
                    }
                    // Reserve both the private overlay and later publication.
                    reserve_edge((*src, *dst), meter)?;
                    reserve_edge((*src, *dst), meter)?;
                    created.insert(
                        *eid,
                        Edge {
                            src: *src,
                            relation: *relation,
                            dst: *dst,
                        },
                    );
                    touch(&mut affected, *eid, meter)?;
                }
            }
        }
        for entry in batch.coordinate_entries() {
            meter.charge(ZSetEvent::Work)?;
            if entry.graph != crate::GRAPH || entry.branch != crate::BRANCH {
                continue;
            }
            for row in &entry.rows {
                meter.charge(ZSetEvent::Work)?;
                match row {
                    DeltaRow::DeleteEdge { eid, .. } => {
                        let edge = created.edges.get(eid).or_else(|| self.input.edges.get(eid));
                        if let Some(edge) = edge {
                            if edge.relation != entry.relation {
                                return Err(StandingQueryFailure::InvalidDelta);
                            }
                            touch(&mut removed, *eid, meter)?;
                            touch(&mut affected, *eid, meter)?;
                        } else if self.shape.relations.contains(&entry.relation) {
                            return Err(StandingQueryFailure::InvalidDelta);
                        }
                    }
                    DeltaRow::DeleteVertex {
                        vid,
                        sorted_retired_incident_edges,
                        ..
                    } => {
                        for &eid in sorted_retired_incident_edges {
                            meter.charge(ZSetEvent::Work)?;
                            if let Some(edge) = created
                                .edges
                                .get(&eid)
                                .or_else(|| self.input.edges.get(&eid))
                            {
                                if edge.src != *vid && edge.dst != *vid {
                                    return Err(StandingQueryFailure::InvalidDelta);
                                }
                                touch(&mut removed, eid, meter)?;
                                touch(&mut affected, eid, meter)?;
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        // Count invalidating identities. Join partner visits are charged as
        // work; they need not themselves be in the invalidation set.
        meter.stats.affected_edges =
            u64::try_from(affected.len()).map_err(|_| StandingQueryFailure::WorkBudget)?;
        let final_graph = Overlay {
            base: &self.input,
            created: &created,
            removed: &removed,
        };
        for &eid in &affected {
            meter.charge(ZSetEvent::Work)?;
            if let Some(edge) = final_graph.get(eid) {
                // Refuse incomplete cascades even when this edge currently
                // has NO complete multi-hop match and would emit no result.
                vertex(vertices, staged, edge.src)?;
                vertex(vertices, staged, edge.dst)?;
            }
        }
        let mut witness_changes = BTreeMap::new();
        if self.shape.scope.is_some() {
            // No-edge roots still create/delete/move OPTIONAL or anti rows.
            for &vid in staged.keys() {
                scoped::change(&mut witness_changes, vid, 0, meter)?;
            }
        }
        if !affected.is_empty() {
            let empty = Arrangement::default();
            let no_removals = BTreeSet::new();
            let old_vertices = BTreeMap::new();
            Enumeration {
                query,
                shape: &self.shape,
                graph: Overlay {
                    base: &self.input,
                    created: &empty,
                    removed: &no_removals,
                },
                vertices,
                vertex_patch: &old_vertices,
                affected: Some(&affected),
                sign: -1,
            }
            .run(&mut witness_changes, output, meter)?;
            Enumeration {
                query,
                shape: &self.shape,
                graph: final_graph,
                vertices,
                vertex_patch: staged,
                affected: Some(&affected),
                sign: 1,
            }
            .run(&mut witness_changes, output, meter)?;
        }
        let witnesses = match self.shape.scope {
            Some(scope) => scope.finish_roots(
                query,
                &self.witnesses,
                witness_changes,
                vertices,
                staged,
                output,
                meter,
            )?,
            None => BTreeMap::new(),
        };
        (meter.checkpoint)()?;
        Ok(Patch {
            created,
            removed,
            witnesses,
        })
    }

    pub(super) fn publish(&mut self, patch: Patch) {
        for &eid in &patch.removed {
            self.input.remove(eid);
        }
        for (eid, edge) in patch.created.edges {
            if !patch.removed.contains(&eid) {
                self.input.insert(eid, edge);
            }
        }
        for (root, count) in patch.witnesses {
            if count == 0 {
                self.witnesses.remove(&root);
            } else {
                self.witnesses.insert(root, count);
            }
        }
    }
}

#[cfg(test)]
#[path = "multi_hop/scoped_tests.rs"]
mod scoped_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DatabaseKeys, WriteBatch};
    use asupersync::lab::run_async_under_lab;
    use fgdb_gql::GraphAggregate;
    use fgdb_gql::algebra::{GraphColumn, GraphPatternBuilder, GraphValue};
    use fgdb_types::{DatabaseSecurityNamespaceId, PurposeContexts};

    fn definition() -> PreparedGraphAggregate {
        let mut builder = GraphPatternBuilder::new();
        builder
            .vertex("a")
            .unwrap()
            .vertex("b")
            .unwrap()
            .vertex("c")
            .unwrap();
        builder
            .edge("a", RelationId(1), GlaDirection::Forward, "b")
            .unwrap();
        builder
            .edge("b", RelationId(2), GlaDirection::Forward, "c")
            .unwrap();
        let input = builder
            .prepare_values(
                &[
                    GraphColumn::vertex("group", "a"),
                    GraphColumn::property("value", "c", PropertyKeyId(1)),
                    GraphColumn::vertex("endpoint", "c"),
                ],
                0,
                None,
            )
            .unwrap()
            .with_duplicates();
        PreparedGraphAggregate::prepare(
            input,
            &[0],
            &[
                GraphAggregate::min("minimum", 1),
                GraphAggregate::count_rows("count"),
                GraphAggregate::sum_int("sum", 1),
                GraphAggregate::count_distinct("endpoints", 2),
            ],
            0,
            None,
        )
        .unwrap()
    }

    fn advance(
        query: &mut StandingQuery,
        batch: &LogicalDeltaBatch,
        checkpoint: &mut dyn FnMut() -> Result<(), StandingQueryFailure>,
    ) -> Result<StandingQueryStats, StandingQueryFailure> {
        let mut meter = Meter {
            policy: query.policy,
            stats: StandingQueryStats::default(),
            checkpoint,
        };
        query.maintain(batch, &mut meter)?;
        query.frontier = batch.commit_seq();
        Ok(meter.stats)
    }

    fn seeded(batches: &[LogicalDeltaBatch]) -> StandingQuery {
        let definition = definition();
        assert!(eligible(&definition));
        let edges = super::super::State::for_definition(&definition);
        let mut query = StandingQuery {
            definition,
            edges,
            policy: GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000),
            vertices: BTreeMap::new(),
            aggregate: IncrementalAggregate::new(),
            rows: ZSet::new(),
            last_delta: None,
            frontier: CommitSeq::ORIGIN,
            stats: StandingQueryStats::default(),
            failure: None,
        };
        for batch in batches {
            advance(&mut query, batch, &mut || Ok(())).unwrap();
        }
        query
    }

    fn same_state(actual: &StandingQuery, expected: &StandingQuery) {
        assert_eq!(actual.vertices, expected.vertices);
        assert_eq!(actual.edges, expected.edges);
        assert_eq!(actual.aggregate, expected.aggregate);
        assert_eq!(actual.rows, expected.rows);
        assert_eq!(actual.last_delta, expected.last_delta);
        assert_eq!(actual.frontier, expected.frontier);
        assert_eq!(actual.failure, expected.failure);
    }

    #[test]
    fn every_multi_hop_checkpoint_and_budget_refusal_preserves_all_state_and_retries() {
        let ((), report) = run_async_under_lab(0x6a34, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let keys = DatabaseKeys::new([1; 32], DatabaseSecurityNamespaceId([2; 32]), [3; 32]);
            let mut db = Database::open_memory(&commit, keys).await.unwrap();
            let mut r = WriteBatch::new(RelationId(1));
            for id in 1..=5 {
                r.create_vertex(
                    VId(id),
                    vec![],
                    vec![(PropertyKeyId(1), CanonicalScalar::Int(id as i64))],
                );
            }
            for (eid, src, dst) in [(1, 1, 2), (2, 1, 2), (3, 2, 3), (4, 4, 2)] {
                r.add_edge(EId(eid), VId(src), VId(dst), vec![]);
            }
            let first = db.write(&commit, r).await.unwrap();
            let mut s = WriteBatch::new(RelationId(2));
            for (eid, src, dst) in [(11, 2, 3), (12, 2, 3), (13, 3, 4), (14, 4, 5)] {
                s.add_edge(EId(eid), VId(src), VId(dst), vec![]);
            }
            let second = db.write(&commit, s).await.unwrap();
            let batches = [
                db.delta_index().unwrap().get(first).unwrap().clone(),
                db.delta_index().unwrap().get(second).unwrap().clone(),
            ];
            let mut change = WriteBatch::new(RelationId(1));
            change.delete_vertex(VId(2));
            change.set_vertex_property(VId(3), PropertyKeyId(1), Some(CanonicalScalar::Int(7)));
            change.set_vertex_property(VId(4), PropertyKeyId(1), Some(CanonicalScalar::Int(9)));
            change.add_edge(EId(5), VId(1), VId(3), vec![]);
            let at = db.write(&commit, change).await.unwrap();
            let delta = db.delta_index().unwrap().get(at).unwrap().clone();
            let original = seeded(&batches);
            let mut complete = seeded(&batches);
            let mut calls = 0;
            let stats = advance(&mut complete, &delta, &mut || {
                calls += 1;
                Ok(())
            })
            .unwrap();
            // Only 1 -R-> 3 -S-> 4 survives, despite simultaneous input
            // creation, cross-relation cascades and two property changes.
            assert_eq!(complete.rows.len(), 1);
            let (row, weight) = complete.rows.iter().next().unwrap();
            assert_eq!(weight, &fgdb_delta_types::ZWeight::ONE);
            assert_eq!(row.keys(), &[GraphValue::Vertex(VId(1))]);
            assert_eq!(
                row.get(0).unwrap().as_value(),
                Some(&GraphValue::Scalar(CanonicalScalar::Int(9)))
            );
            assert_eq!(row.get(1).unwrap().as_count(), Some(1));
            assert_eq!(row.get(2).unwrap().as_integer(), Some(9));
            assert_eq!(row.get(3).unwrap().as_count(), Some(1));

            for stop in 1..=calls {
                let mut candidate = seeded(&batches);
                let mut seen = 0;
                let result = advance(&mut candidate, &delta, &mut || {
                    seen += 1;
                    if seen == stop {
                        Err(StandingQueryFailure::Interrupted)
                    } else {
                        Ok(())
                    }
                });
                assert_eq!(result, Err(StandingQueryFailure::Interrupted));
                assert_eq!(seen, stop);
                same_state(&candidate, &original);
                advance(&mut candidate, &delta, &mut || Ok(())).unwrap();
                same_state(&candidate, &complete);
            }
            for reason in [
                StandingQueryFailure::WorkBudget,
                StandingQueryFailure::ScratchBudget,
                StandingQueryFailure::ResultBudget,
            ] {
                let mut candidate = seeded(&batches);
                match reason {
                    StandingQueryFailure::WorkBudget => {
                        candidate.policy.evaluator.max_work_units =
                            stats.work_units.checked_sub(1).unwrap();
                    }
                    StandingQueryFailure::ScratchBudget => {
                        candidate.policy.evaluator.max_scratch_entries =
                            stats.scratch_entries.checked_sub(1).unwrap();
                    }
                    _ => candidate.policy = GqlQueryPolicy::new(100_000, 0, 10_000_000, 10_000_000),
                }
                assert_eq!(advance(&mut candidate, &delta, &mut || Ok(())), Err(reason));
                same_state(&candidate, &original);
                candidate.policy = original.policy;
                advance(&mut candidate, &delta, &mut || Ok(())).unwrap();
                same_state(&candidate, &complete);
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn incomplete_cascade_is_rejected_even_without_any_complete_join_binding() {
        let ((), report) = run_async_under_lab(0x6a35, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let keys = DatabaseKeys::new([1; 32], DatabaseSecurityNamespaceId([2; 32]), [3; 32]);
            let mut db = Database::open_memory(&commit, keys).await.unwrap();
            let mut seed = WriteBatch::new(RelationId(1));
            seed.create_vertex(VId(1), vec![], vec![]);
            seed.create_vertex(VId(2), vec![], vec![]);
            seed.add_edge(EId(1), VId(1), VId(2), vec![]);
            let basis = db.write(&commit, seed).await.unwrap();
            let batches = [db.delta_index().unwrap().get(basis).unwrap().clone()];
            let mut deletion = WriteBatch::new(RelationId(1));
            deletion.delete_vertex(VId(2));
            let at = db.write(&commit, deletion).await.unwrap();
            let delta = db.delta_index().unwrap().get(at).unwrap().clone();
            let mut entries = delta.coordinate_entries().to_vec();
            let mut cleared = 0;
            for entry in &mut entries {
                for row in &mut entry.rows {
                    if let DeltaRow::DeleteVertex {
                        sorted_retired_incident_edges,
                        ..
                    } = row
                    {
                        cleared += sorted_retired_incident_edges.len();
                        sorted_retired_incident_edges.clear();
                    }
                }
            }
            assert_eq!(cleared, 1);
            let malformed = LogicalDeltaBatch::from_parts_for_test(
                entries,
                *delta.source_template_digest(),
                delta.commit_marker_identity(),
                delta.commit_seq(),
                delta.frontier(),
            );
            let original = seeded(&batches);
            assert!(original.rows.is_empty()); // no relation-2 edge ever existed
            let mut candidate = seeded(&batches);
            assert_eq!(
                advance(&mut candidate, &malformed, &mut || Ok(())),
                Err(StandingQueryFailure::InvalidDelta)
            );
            same_state(&candidate, &original);
            advance(&mut candidate, &delta, &mut || Ok(())).unwrap();
            assert_eq!(candidate.frontier, at);
            assert!(candidate.rows.is_empty());
            assert_eq!(candidate.vertices.len(), 1);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }
}
