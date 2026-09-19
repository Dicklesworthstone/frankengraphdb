//! Incident-indexed maintenance for admitted fixed-hop GLA inputs.
//!
//! These are query-owned input arrangements, never graph storage or recovery
//! authority. Bootstrap uses the database's ordinary Strata edge read. Later
//! ticks visit changed EIds and edges incident to changed endpoint projections.
//! One-hop and scoped inputs retain their specialized maintainer; connected
//! positive multi-hop plans use affected-binding joins over all input relations.

mod multi_hop;
mod scoped;

use super::*;
use fgdb_delta_types::RelationId;
use fgdb_gql::algebra::GlaDirection;
use fgdb_types::EId;

type Endpoints = (VId, VId);
type Vertices = BTreeMap<VId, VertexState>;
type VertexPatch = BTreeMap<VId, Option<VertexState>>;

#[derive(Debug, PartialEq, Eq)]
pub(super) struct State {
    input: Input,
}

#[derive(Debug, PartialEq, Eq)]
enum Input {
    OneHop(OneHopState),
    MultiHop(multi_hop::State),
}

pub(super) struct Patch {
    input: InputPatch,
}

enum InputPatch {
    OneHop(OneHopPatch),
    MultiHop(multi_hop::Patch),
}

impl State {
    pub(super) fn for_definition(query: &PreparedGraphAggregate) -> Option<Self> {
        let input = if let Some(state) = multi_hop::State::for_definition(query) {
            Input::MultiHop(state)
        } else {
            Input::OneHop(OneHopState::for_definition(query)?)
        };
        Some(Self { input })
    }

    pub(super) fn seed(
        &mut self,
        query: &PreparedGraphAggregate,
        row: &fgdb_strata::AdjacencyEntry,
        vertices: &Vertices,
        output: &mut Vec<grouped::Contribution>,
        meter: &mut Meter<'_>,
    ) -> Result<(), StandingQueryFailure> {
        match &mut self.input {
            Input::OneHop(state) => state.seed(query, row, vertices, output, meter),
            Input::MultiHop(state) => state.seed(row, vertices, meter),
        }
    }

    pub(super) fn finish_seed(
        &self,
        query: &PreparedGraphAggregate,
        vertices: &Vertices,
        output: &mut Vec<grouped::Contribution>,
        meter: &mut Meter<'_>,
    ) -> Result<(), StandingQueryFailure> {
        match &self.input {
            Input::OneHop(state) => state.finish_seed(query, vertices, output, meter),
            Input::MultiHop(state) => state.finish_seed(query, vertices, output, meter),
        }
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
        let input = match &self.input {
            Input::OneHop(state) => InputPatch::OneHop(
                state.prepare(query, batch, vertices, staged, output, meter)?,
            ),
            Input::MultiHop(state) => InputPatch::MultiHop(
                state.prepare(query, batch, vertices, staged, output, meter)?,
            ),
        };
        Ok(Patch { input })
    }

    pub(super) fn publish(&mut self, patch: Patch) {
        match (&mut self.input, patch.input) {
            (Input::OneHop(state), InputPatch::OneHop(patch)) => state.publish(patch),
            (Input::MultiHop(state), InputPatch::MultiHop(patch)) => state.publish(patch),
            // Both variants are private. StandingQuery owns the immutable
            // definition and the complete prepare/publish interval exclusively.
            _ => unreachable!("a prepared standing input cannot change its physical shape"),
        }
    }

    #[cfg(test)]
    pub(super) fn has_scope(&self) -> bool {
        match &self.input {
            Input::OneHop(state) => state.scope.is_some(),
            Input::MultiHop(_) => false,
        }
    }
}

#[derive(PartialEq, Eq)]
struct OneHopState {
    relation: RelationId,
    direction: GlaDirection,
    scope: Option<scoped::Shape>,
    witnesses: BTreeMap<VId, u64>,
    edges: BTreeMap<EId, Endpoints>,
    incident: BTreeMap<VId, BTreeSet<EId>>,
}

impl core::fmt::Debug for OneHopState {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("StandingEdgeInput")
            .field("edge_count", &self.edges.len())
            .field("data", &"[REDACTED]")
            .finish()
    }
}

struct OneHopPatch {
    created: BTreeMap<EId, Endpoints>,
    removed: BTreeSet<EId>,
    witnesses: BTreeMap<VId, u64>,
}

/// Additional compiled input shapes beyond the flat scan admission path.
/// Positive multi-hop joins do not acquire OPTIONAL or probe semantics.
pub(super) fn supports_scoped(query: &PreparedGraphAggregate) -> bool {
    scoped::Shape::of(query).is_some() || multi_hop::Shape::of(query).is_some()
}

fn touch(
    set: &mut BTreeSet<EId>,
    eid: EId,
    meter: &mut Meter<'_>,
) -> Result<(), StandingQueryFailure> {
    meter.charge(ZSetEvent::Work)?;
    if !set.contains(&eid) {
        meter.charge(ZSetEvent::ScratchEntry)?;
        set.insert(eid);
    }
    Ok(())
}

fn reserve_edge(pair: Endpoints, meter: &mut Meter<'_>) -> Result<(), StandingQueryFailure> {
    // Conservative logical reservations: one identity, plus an incident-group
    // key and set member per distinct endpoint. Existing groups may reuse the
    // reservation. Every allocation at publication was admitted beforehand.
    meter.units(ZSetEvent::ScratchEntry, if pair.0 == pair.1 { 3 } else { 5 })
}

fn vertex<'a>(
    vertices: &'a Vertices,
    patch: &'a VertexPatch,
    vid: VId,
) -> Result<&'a VertexState, StandingQueryFailure> {
    // A staged deletion is not a missing patch: never resurrect the old state.
    match patch.get(&vid) {
        Some(state) => state.as_ref(),
        None => vertices.get(&vid),
    }.ok_or(StandingQueryFailure::InvalidDelta)
}

impl OneHopState {
    fn for_definition(query: &PreparedGraphAggregate) -> Option<Self> {
        let scope = scoped::Shape::of(query);
        let (relation, direction) = match query.input_pattern().plan().operators().first()? {
            GlaOperator::ScanEdges { relation, direction } => (*relation, *direction),
            _ => {
                let scope = scope?;
                (scope.relation, scope.direction)
            }
        };
        Some(Self {
            relation, direction, scope, witnesses: BTreeMap::new(),
            edges: BTreeMap::new(), incident: BTreeMap::new(),
        })
    }

    fn finish_seed(
        &self,
        query: &PreparedGraphAggregate,
        vertices: &Vertices,
        output: &mut Vec<grouped::Contribution>,
        meter: &mut Meter<'_>,
    ) -> Result<(), StandingQueryFailure> {
        if let Some(scope) = self.scope {
            for (&vid, state) in vertices {
                meter.charge(ZSetEvent::Work)?;
                scope.root_contribution(query, vid, Some(state),
                    self.witnesses.get(&vid).copied().unwrap_or(0), 1, output, meter)?;
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn contribute(
        &self,
        query: &PreparedGraphAggregate,
        pair: Endpoints,
        vertices: &Vertices,
        patch: &VertexPatch,
        sign: i128,
        output: &mut Vec<grouped::Contribution>,
        meter: &mut Meter<'_>,
    ) -> Result<(), StandingQueryFailure> {
        meter.charge(ZSetEvent::Work)?;
        let (src, dst) = pair;
        let source = vertex(vertices, patch, src)?;
        let target = vertex(vertices, patch, dst)?;
        let forward = [(src, source), (dst, target)];
        let reverse = [(dst, target), (src, source)];
        match self.direction {
            GlaDirection::Forward => grouped::binding_contributions(query, &forward, sign, output, meter),
            GlaDirection::Reverse => grouped::binding_contributions(query, &reverse, sign, output, meter),
            GlaDirection::Undirected => {
                grouped::binding_contributions(query, &forward, sign, output, meter)?;
                // One undirected self-loop is one edge occurrence, not two.
                if src != dst {
                    grouped::binding_contributions(query, &reverse, sign, output, meter)?;
                }
                Ok(())
            }
        }
    }

    fn insert(&mut self, eid: EId, pair: Endpoints) {
        self.edges.insert(eid, pair);
        self.incident.entry(pair.0).or_default().insert(eid);
        if pair.0 != pair.1 {
            self.incident.entry(pair.1).or_default().insert(eid);
        }
    }

    fn seed(
        &mut self,
        query: &PreparedGraphAggregate,
        row: &fgdb_strata::AdjacencyEntry,
        vertices: &Vertices,
        output: &mut Vec<grouped::Contribution>,
        meter: &mut Meter<'_>,
    ) -> Result<(), StandingQueryFailure> {
        meter.charge(ZSetEvent::Work)?;
        if row.relation != self.relation { return Ok(()); }
        if self.edges.contains_key(&row.eid) { return Err(StandingQueryFailure::InvalidDelta); }
        let pair = (row.src, row.dst);
        reserve_edge(pair, meter)?;
        if let Some(scope) = self.scope {
            let mut changes = BTreeMap::new();
            scope.contribute(query, pair, vertices, &BTreeMap::new(), 1, &mut changes, output, meter)?;
            for (vid, change) in changes {
                let count = i128::from(self.witnesses.get(&vid).copied().unwrap_or(0))
                    .checked_add(change).and_then(|n| u64::try_from(n).ok())
                    .ok_or(StandingQueryFailure::InvalidDelta)?;
                if !self.witnesses.contains_key(&vid) {
                    meter.charge(ZSetEvent::ScratchEntry)?;
                }
                self.witnesses.insert(vid, count);
            }
        } else {
            self.contribute(query, pair, vertices, &BTreeMap::new(), 1, output, meter)?;
        }
        self.insert(row.eid, pair);
        Ok(())
    }

    fn prepare(
        &self,
        query: &PreparedGraphAggregate,
        batch: &LogicalDeltaBatch,
        vertices: &Vertices,
        staged: &VertexPatch,
        output: &mut Vec<grouped::Contribution>,
        meter: &mut Meter<'_>,
    ) -> Result<OneHopPatch, StandingQueryFailure> {
        let mut affected = BTreeSet::new();
        for vid in staged.keys() {
            meter.charge(ZSetEvent::Work)?;
            if let Some(edges) = self.incident.get(vid) {
                for &eid in edges { touch(&mut affected, eid, meter)?; }
            }
        }
        let mut created = BTreeMap::new();
        let mut removed = BTreeSet::new();
        // Canonical coordinate order is not an execution schedule. A later
        // coordinate can create an edge named by an earlier cascade, so stage
        // every relevant creation before inspecting deletions.
        for entry in batch.coordinate_entries() {
            meter.charge(ZSetEvent::Work)?;
            if entry.graph != crate::GRAPH || entry.branch != crate::BRANCH { continue; }
            for row in &entry.rows {
                meter.charge(ZSetEvent::Work)?;
                if let DeltaRow::CreateEdge { eid, src, relation, dst, .. } = row {
                    if *relation != entry.relation { return Err(StandingQueryFailure::InvalidDelta); }
                    if *relation != self.relation { continue; }
                    if self.edges.contains_key(eid) || created.contains_key(eid) {
                        return Err(StandingQueryFailure::InvalidDelta);
                    }
                    let pair = (*src, *dst);
                    meter.charge(ZSetEvent::ScratchEntry)?;
                    reserve_edge(pair, meter)?;
                    created.insert(*eid, pair);
                    touch(&mut affected, *eid, meter)?;
                }
            }
        }
        for entry in batch.coordinate_entries() {
            meter.charge(ZSetEvent::Work)?;
            if entry.graph != crate::GRAPH || entry.branch != crate::BRANCH { continue; }
            for row in &entry.rows {
                meter.charge(ZSetEvent::Work)?;
                match row {
                    DeltaRow::DeleteEdge { eid, .. } => {
                        let known = created.contains_key(eid) || self.edges.contains_key(eid);
                        if entry.relation != self.relation {
                            if known { return Err(StandingQueryFailure::InvalidDelta); }
                            continue;
                        }
                        if !known { return Err(StandingQueryFailure::InvalidDelta); }
                        touch(&mut removed, *eid, meter)?;
                        touch(&mut affected, *eid, meter)?;
                    }
                    DeltaRow::DeleteVertex { vid, sorted_retired_incident_edges, .. } => {
                        for &eid in sorted_retired_incident_edges {
                            meter.charge(ZSetEvent::Work)?;
                            let pair = created.get(&eid).or_else(|| self.edges.get(&eid));
                            // A cascade spans relations, including those outside
                            // this fixed query input. Only retained EIds matter.
                            if let Some(&(src, dst)) = pair {
                                if src != *vid && dst != *vid {
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
        meter.stats.affected_edges = u64::try_from(affected.len())
            .map_err(|_| StandingQueryFailure::WorkBudget)?;
        let empty = BTreeMap::new();
        let mut witness_changes = BTreeMap::new();
        if self.scope.is_some() {
            // Roots with no incident edges still create/delete/move null rows.
            for &vid in staged.keys() {
                scoped::change(&mut witness_changes, vid, 0, meter)?;
            }
        }
        for eid in affected {
            meter.charge(ZSetEvent::Work)?;
            if let Some(&pair) = self.edges.get(&eid) {
                if let Some(scope) = self.scope {
                    scope.contribute(query, pair, vertices, &empty, -1, &mut witness_changes, output, meter)?;
                } else {
                    self.contribute(query, pair, vertices, &empty, -1, output, meter)?;
                }
            }
            if !removed.contains(&eid) {
                let pair = created.get(&eid).or_else(|| self.edges.get(&eid))
                    .copied().ok_or(StandingQueryFailure::InvalidDelta)?;
                // Also refuses incomplete cascades: every old incident edge is
                // in `affected`, and no surviving edge can reference a staged
                // deletion. New edges must resolve both final endpoints too.
                if let Some(scope) = self.scope {
                    scope.contribute(query, pair, vertices, staged, 1, &mut witness_changes, output, meter)?;
                } else {
                    self.contribute(query, pair, vertices, staged, 1, output, meter)?;
                }
            }
        }
        let witnesses = match self.scope {
            Some(scope) => scope.finish_roots(query, &self.witnesses, witness_changes,
                vertices, staged, output, meter)?,
            None => BTreeMap::new(),
        };
        (meter.checkpoint)()?;
        Ok(OneHopPatch { created, removed, witnesses })
    }

    fn publish(&mut self, patch: OneHopPatch) {
        for (vid, count) in patch.witnesses {
            if count == 0 { self.witnesses.remove(&vid); }
            else { self.witnesses.insert(vid, count); }
        }
        for &eid in &patch.removed {
            if let Some((src, dst)) = self.edges.remove(&eid) {
                for vid in [src, dst] {
                    if let std::collections::btree_map::Entry::Occupied(mut entry) = self.incident.entry(vid) {
                        entry.get_mut().remove(&eid);
                        if entry.get().is_empty() { entry.remove(); }
                    }
                    if src == dst { break; }
                }
            }
        }
        for (eid, pair) in patch.created {
            if !patch.removed.contains(&eid) { self.insert(eid, pair); }
        }
    }
}

/// Admit the ordinary full Strata edge read used only for initialization and
/// rebuild. It returns properties too, so charge those payloads even though
/// fixed-hop inputs retain only identities, relations and endpoints. These
/// logical reservations do not claim byte-accurate accounting or spill.
pub(super) fn admit_snapshot(
    snapshot: &crate::Snapshot,
    records: &mut u64,
    meter: &mut Meter<'_>,
) -> Result<(), StandingQueryFailure> {
    for block in &snapshot.blocks {
        for _ in block {
            *records = records.checked_add(1).ok_or(StandingQueryFailure::SnapshotBudget)?;
            if meter.policy.rows.max_snapshot_records().is_some_and(|limit| *records > limit) {
                return Err(StandingQueryFailure::SnapshotBudget);
            }
            meter.charge(ZSetEvent::Work)?;
            meter.units(ZSetEvent::ScratchEntry, 4)?;
        }
    }
    for props in snapshot.block_props.iter().flatten() {
        for row in &props.rows {
            meter.charge(ZSetEvent::ScratchEntry)?;
            for (_, value) in row {
                meter.charge(ZSetEvent::Work)?;
                meter.units(ZSetEvent::ScratchEntry, scalar_units(value))?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use asupersync::lab::run_async_under_lab;
    use crate::{DatabaseKeys, WriteBatch};
    use fgdb_gql::algebra::{GraphColumn, GraphPatternBuilder};
    use fgdb_gql::GraphAggregate;
    use fgdb_types::{DatabaseSecurityNamespaceId, PurposeContexts};

    fn definition() -> PreparedGraphAggregate {
        let mut builder = GraphPatternBuilder::new();
        builder.vertex("a").unwrap().vertex("b").unwrap();
        builder.edge("a", RelationId(1), GlaDirection::Undirected, "b").unwrap();
        builder.compare_properties("a", PropertyKeyId(1),
            fgdb_gql::algebra::IntegerComparison::LessOrEqual, "b", PropertyKeyId(1)).unwrap();
        let input = builder.prepare_values(&[
            GraphColumn::vertex("group", "a"),
            GraphColumn::property("amount", "b", PropertyKeyId(1)),
        ], 0, None).unwrap().with_duplicates();
        PreparedGraphAggregate::prepare(input, &[0], &[
            GraphAggregate::count_rows("count"), GraphAggregate::sum_int("sum", 1),
        ], 0, None).unwrap()
    }

    fn seeded(batch: &LogicalDeltaBatch) -> StandingQuery {
        let definition = definition();
        let edges = State::for_definition(&definition);
        let policy = GqlQueryPolicy::new(100_000, 10_000, 10_000_000, 10_000_000);
        let mut query = StandingQuery {
            definition, edges, policy, vertices: BTreeMap::new(),
            aggregate: IncrementalAggregate::new(), rows: ZSet::new(),
            frontier: CommitSeq::ORIGIN, stats: StandingQueryStats::default(), failure: None,
        };
        let mut checkpoint = || Ok(());
        let mut meter = Meter { policy, stats: StandingQueryStats::default(), checkpoint: &mut checkpoint };
        query.maintain(batch, &mut meter).unwrap();
        query.frontier = batch.commit_seq();
        query
    }

    fn assert_unchanged(actual: &StandingQuery, expected: &StandingQuery) {
        assert_eq!(actual.vertices, expected.vertices);
        assert_eq!(actual.edges, expected.edges);
        assert_eq!(actual.aggregate, expected.aggregate);
        assert_eq!(actual.rows, expected.rows);
        assert_eq!(actual.frontier, expected.frontier);
    }

    #[test]
    fn every_one_hop_checkpoint_aborts_all_arrangements_and_retries() {
        let ((), report) = run_async_under_lab(0x7e05, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.commit();
            let keys = DatabaseKeys::new([1; 32], DatabaseSecurityNamespaceId([2; 32]), [3; 32]);
            let mut db = Database::open_memory(&cx, keys).await.unwrap();
            let mut batch = WriteBatch::new(RelationId(1));
            for id in 1..=3 {
                batch.create_vertex(VId(id), vec![], vec![(PropertyKeyId(1), CanonicalScalar::Int(id as i64))]);
            }
            for (eid, src, dst) in [(1, 1, 2), (2, 1, 2), (3, 2, 2), (4, 2, 3)] {
                batch.add_edge(EId(eid), VId(src), VId(dst), vec![]);
            }
            let first = db.write(&cx, batch).await.unwrap();
            let initial = db.delta_index().unwrap().get(first).unwrap().clone();
            let mut next = WriteBatch::new(RelationId(1));
            next.delete_vertex(VId(2));
            next.set_vertex_property(VId(1), PropertyKeyId(1), Some(CanonicalScalar::Int(7)));
            next.set_vertex_property(VId(3), PropertyKeyId(1), Some(CanonicalScalar::Int(9)));
            next.add_edge(EId(5), VId(1), VId(3), vec![]);
            let last = db.write(&cx, next).await.unwrap();
            let delta = db.delta_index().unwrap().get(last).unwrap().clone();
            let before = seeded(&initial);
            let mut successful = seeded(&initial);
            let policy = successful.policy;
            let mut checkpoints = 0;
            {
                let mut checkpoint = || { checkpoints += 1; Ok(()) };
                let mut meter = Meter { policy, stats: StandingQueryStats::default(), checkpoint: &mut checkpoint };
                successful.maintain(&delta, &mut meter).unwrap();
                assert_eq!(meter.stats.affected_edges, 5);
            }
            for stop in 1..=checkpoints {
                let mut candidate = seeded(&initial);
                let mut seen = 0;
                {
                    let mut checkpoint = || {
                        seen += 1;
                        if seen == stop { Err(StandingQueryFailure::Interrupted) } else { Ok(()) }
                    };
                    let mut meter = Meter { policy, stats: StandingQueryStats::default(), checkpoint: &mut checkpoint };
                    assert_eq!(candidate.maintain(&delta, &mut meter), Err(StandingQueryFailure::Interrupted));
                }
                assert_eq!(seen, stop);
                assert_unchanged(&candidate, &before);
                let mut checkpoint = || Ok(());
                let mut meter = Meter { policy, stats: StandingQueryStats::default(), checkpoint: &mut checkpoint };
                candidate.maintain(&delta, &mut meter).unwrap();
                assert_unchanged(&candidate, &successful);
            }

            // A malformed deletion must not leave retained dangling edges or
            // silently reinterpret an incomplete cascade as a valid retraction.
            let mut entries = delta.coordinate_entries().to_vec();
            for entry in &mut entries {
                for row in &mut entry.rows {
                    if let DeltaRow::DeleteVertex { sorted_retired_incident_edges, .. } = row {
                        sorted_retired_incident_edges.clear();
                    }
                }
            }
            let malformed = LogicalDeltaBatch::from_parts_for_test(
                entries, *delta.source_template_digest(), delta.commit_marker_identity(),
                delta.commit_seq(), delta.frontier(),
            );
            let mut candidate = seeded(&initial);
            let mut checkpoint = || Ok(());
            let mut meter = Meter { policy, stats: StandingQueryStats::default(), checkpoint: &mut checkpoint };
            assert_eq!(candidate.maintain(&malformed, &mut meter), Err(StandingQueryFailure::InvalidDelta));
            assert_unchanged(&candidate, &before);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }
}
