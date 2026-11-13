//! Native per-vertex triangle counts and clustering coefficients over the
//! existing committed topology and exact DISTINCT triangle derivative. Only
//! vertex counters and native rows are retained here; the typed triangle-triple
//! API keeps its separate profile. Distinct degrees use the same authoritative
//! prepared edge support, including multiplicity-only and opposite-edge ticks.
//! Bootstrap still owns the kernel's transient complete triangle derivative.
//! A selected topology consumes its maintained parent's accepted row derivative;
//! endpoint support supplies vertex lifetime without rescanning graph storage.

use super::*;
use components::relation::{Definition, VertexSupport, project_rows, support_error};
use fgdb_delta_types::zset::ZSetUpdate;
use fgdb_delta_types::zset::committed::CommittedEdgeInput;
use fgdb_delta_types::zset::set::SetOperation;
use fgdb_delta_types::zset::triangles::{IncrementalTriangles, TriangleError, TriangleQuantifier};
use fgdb_delta_types::{LimbLimit, ZWeight};
use fgdb_gql::algebra::GraphValue;
use fgdb_types::{CanonicalF64, CanonicalScalar};
use std::collections::BTreeMap;

const LIMBS: LimbLimit = LimbLimit::new(4);
type Pair = (VId, VId);
type Triple = (VId, VId, VId);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Output {
    Triangles,
    Clustering,
}

impl Output {
    fn value(self, counters: Counters) -> Result<CanonicalScalar, StandingQueryFailure> {
        match self {
            Self::Triangles => Ok(CanonicalScalar::Int(
                i64::try_from(counters.triangles).map_err(|_| StandingQueryFailure::Arithmetic)?,
            )),
            Self::Clustering => {
                // Preserve Prism's exact-count profile: widen both integer
                // products before converting each once, then perform one
                // division. In particular, do not multiply a rounded ratio.
                let score = if counters.triangles == 0 || counters.degree < 2 {
                    0.0
                } else {
                    let degree = u128::from(counters.degree);
                    let denominator = degree
                        .checked_mul(degree - 1)
                        .ok_or(StandingQueryFailure::Arithmetic)?;
                    let numerator = u128::from(counters.triangles) * 2;
                    if numerator > denominator {
                        return Err(StandingQueryFailure::InvalidDelta);
                    }
                    numerator as f64 / denominator as f64
                };
                Ok(CanonicalScalar::Float(CanonicalF64::new(score)))
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Counters {
    triangles: u64,
    degree: u64,
    // Physical selected-edge incidence, including both ends of self-loops.
    // This is liveness support, not the projected simple-graph degree.
    incidences: i128,
}

#[derive(Debug, PartialEq, Eq)]
struct Counts {
    vertices: BTreeMap<VId, Counters>,
    output: Output,
    columns: [String; 2],
    rows: ZSet<GraphValueRow>,
    last_delta: Option<ZSet<GraphValueRow>>,
}

struct Change {
    live: bool,
    triangles: i128,
    degree: i128,
    incidences: i128,
    next: Option<Counters>,
}

fn changed<'a>(
    changes: &'a mut BTreeMap<VId, Change>,
    current: &BTreeMap<VId, Counters>,
    vertex: VId,
    meter: &mut Meter<'_>,
) -> Result<&'a mut Change, StandingQueryFailure> {
    meter.charge(ZSetEvent::Work)?;
    if let std::collections::btree_map::Entry::Vacant(entry) = changes.entry(vertex) {
        // The staged key plus a possible retained vertex entry are reserved
        // before either allocation. Counter payloads have fixed size.
        meter.units(ZSetEvent::ScratchEntry, 2)?;
        let old = current.get(&vertex);
        entry.insert(Change {
            live: old.is_some(),
            triangles: old.map_or(0, |value| i128::from(value.triangles)),
            degree: old.map_or(0, |value| i128::from(value.degree)),
            incidences: old.map_or(0, |value| value.incidences),
            next: None,
        });
    }
    changes
        .get_mut(&vertex)
        .ok_or(StandingQueryFailure::InvalidDelta)
}

fn row(
    vertex: VId,
    value: CanonicalScalar,
    meter: &mut Meter<'_>,
) -> Result<GraphValueRow, StandingQueryFailure> {
    meter.units(ZSetEvent::ScratchEntry, 3)?;
    Ok(GraphValueRow::from_owned_values(vec![
        GraphValue::Vertex(vertex),
        GraphValue::Scalar(value),
    ]))
}

impl Counts {
    fn new(output: Output, meter: &mut Meter<'_>) -> Result<Self, StandingQueryFailure> {
        // Fixed row metadata plus the native handle's presentation copy;
        // scratch units count entries, as in the component row sink.
        meter.units(ZSetEvent::ScratchEntry, 8)?;
        Ok(Self {
            vertices: BTreeMap::new(),
            output,
            columns: [
                "vertex".into(),
                match output {
                    Output::Triangles => "triangles",
                    Output::Clustering => "score",
                }
                .into(),
            ],
            rows: ZSet::new(),
            last_delta: None,
        })
    }

    fn prepare<'a>(
        &mut self,
        vertices: &ZSet<VId>,
        edges: &ZSet<Pair>,
        support: impl Iterator<Item = (&'a Pair, bool, bool)>,
        triangles: &ZSet<Triple>,
        meter: &mut Meter<'_>,
    ) -> Result<CountsUpdate<'_>, StandingQueryFailure> {
        meter.charge(ZSetEvent::Work)?;
        if self.rows.len() != self.vertices.len() {
            return Err(StandingQueryFailure::InvalidDelta);
        }
        let mut changes = BTreeMap::new();
        for (vertex, weight) in vertices.iter() {
            let change = changed(&mut changes, &self.vertices, *vertex, meter)?;
            match weight.to_i128() {
                Some(1) if !change.live => change.live = true,
                Some(-1) if change.live => change.live = false,
                _ => return Err(StandingQueryFailure::InvalidDelta),
            }
        }
        for ((source, target), weight) in edges.iter() {
            meter.charge(ZSetEvent::Work)?;
            let weight = weight.to_i128().ok_or(StandingQueryFailure::Arithmetic)?;
            for vertex in [*source, *target] {
                let change = changed(&mut changes, &self.vertices, vertex, meter)?;
                change.incidences = change
                    .incidences
                    .checked_add(weight)
                    .ok_or(StandingQueryFailure::Arithmetic)?;
            }
        }
        for ((source, target), old, next) in support {
            meter.charge(ZSetEvent::Work)?;
            if source == target || old == next {
                continue;
            }
            let weight = if next { 1 } else { -1 };
            for vertex in [*source, *target] {
                let change = changed(&mut changes, &self.vertices, vertex, meter)?;
                change.degree = change
                    .degree
                    .checked_add(weight)
                    .ok_or(StandingQueryFailure::Arithmetic)?;
            }
        }
        for ((a, b, c), weight) in triangles.iter() {
            meter.charge(ZSetEvent::Work)?;
            if !(a < b && b < c) {
                return Err(StandingQueryFailure::InvalidDelta);
            }
            let weight = match weight.to_i128() {
                Some(weight @ (1 | -1)) => weight,
                _ => return Err(StandingQueryFailure::InvalidDelta),
            };
            for vertex in [*a, *b, *c] {
                let change = changed(&mut changes, &self.vertices, vertex, meter)?;
                change.triangles = change
                    .triangles
                    .checked_add(weight)
                    .ok_or(StandingQueryFailure::Arithmetic)?;
            }
        }
        let mut count = self.vertices.len();
        let mut output = ZSet::new();
        for (vertex, change) in &mut changes {
            // Include the eventual fixed-size map publication under this
            // preparation allowance; commit performs no recoverable work.
            meter.units(ZSetEvent::Work, 2)?;
            if change.incidences < 0 || change.triangles < 0 || change.degree < 0 {
                return Err(StandingQueryFailure::InvalidDelta);
            }
            if !change.live
                && (change.incidences != 0 || change.triangles != 0 || change.degree != 0)
            {
                // Selected-edge endpoints and incomplete selected-edge
                // cascades refuse, including vertices with zero triangles.
                return Err(StandingQueryFailure::InvalidDelta);
            }
            let old = self.vertices.get(vertex);
            change.next = if change.live {
                Some(Counters {
                    triangles: u64::try_from(change.triangles)
                        .map_err(|_| StandingQueryFailure::Arithmetic)?,
                    degree: u64::try_from(change.degree)
                        .map_err(|_| StandingQueryFailure::Arithmetic)?,
                    incidences: change.incidences,
                })
            } else {
                None
            };
            match (old.is_some(), change.live) {
                (false, true) => {
                    count = count
                        .checked_add(1)
                        .ok_or(StandingQueryFailure::Arithmetic)?;
                }
                (true, false) => {
                    count = count
                        .checked_sub(1)
                        .ok_or(StandingQueryFailure::InvalidDelta)?;
                }
                _ => {}
            }
            let old_value = old.map(|value| self.output.value(*value)).transpose()?;
            let next_value = change
                .next
                .map(|value| self.output.value(value))
                .transpose()?;
            if old_value == next_value {
                continue;
            }
            if let Some(old) = old_value {
                let old_row = row(*vertex, old, meter)?;
                if self.rows.weight(&old_row) != Some(&ZWeight::ONE) {
                    return Err(StandingQueryFailure::InvalidDelta);
                }
                output
                    .accumulate(old_row, ZWeight::from_i128(-1), LIMBS, &mut |event| {
                        meter.charge(event)
                    })
                    .map_err(zset_error)?;
            }
            if let Some(next) = next_value {
                let next_row = row(*vertex, next, meter)?;
                if self.rows.weight(&next_row).is_some() {
                    return Err(StandingQueryFailure::InvalidDelta);
                }
                output
                    .accumulate(next_row, ZWeight::ONE, LIMBS, &mut |event| {
                        meter.charge(event)
                    })
                    .map_err(zset_error)?;
            }
        }
        // A count replacement is one final vertex row. Triangle support size
        // and insertion-before-retraction prefixes are not result rows.
        components::result_bound(count, meter.policy)?;
        for _ in output.iter() {
            meter.charge(ZSetEvent::Work)?;
            meter.units(ZSetEvent::ScratchEntry, 3)?;
        }
        let sink = self
            .rows
            .prepare_update(&output, LIMBS, &mut |event| meter.charge(event))
            .map_err(zset_error)?;
        for (key, _) in output.iter() {
            meter.charge(ZSetEvent::Work)?;
            if sink
                .weight(key)
                .is_some_and(|weight| weight != &ZWeight::ONE)
            {
                return Err(StandingQueryFailure::InvalidDelta);
            }
        }
        (meter.checkpoint)()?;
        Ok(CountsUpdate {
            vertices: &mut self.vertices,
            changes,
            sink,
            output,
            last_delta: &mut self.last_delta,
        })
    }
}

#[must_use = "dropping a triangle-count update preserves the accepted counters and rows"]
struct CountsUpdate<'a> {
    vertices: &'a mut BTreeMap<VId, Counters>,
    changes: BTreeMap<VId, Change>,
    sink: ZSetUpdate<'a, GraphValueRow>,
    output: ZSet<GraphValueRow>,
    last_delta: &'a mut Option<ZSet<GraphValueRow>>,
}

impl CountsUpdate<'_> {
    fn commit(self) {
        for (vertex, change) in self.changes {
            if let Some(next) = change.next {
                self.vertices.insert(vertex, next);
            } else {
                self.vertices.remove(&vertex);
            }
        }
        self.sink.commit();
        *self.last_delta = Some(self.output);
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Input {
    Committed(CommittedEdgeInput),
    Selected(VertexSupport),
}

pub(crate) struct State {
    input: Input,
    triangles: IncrementalTriangles<VId>,
    counts: Counts,
    // Without a selected parent, None retains the whole-graph selector,
    // including future relation types.
    pub(super) relation: Option<RelationId>,
    pub(super) selected: Option<Definition>,
    pub(super) policy: GqlQueryPolicy,
    pub(super) frontier: CommitSeq,
    pub(super) stats: StandingQueryStats,
    pub(super) failure: Option<StandingQueryFailure>,
}

fn triangle_error(error: TriangleError<StandingQueryFailure>) -> StandingQueryFailure {
    match error {
        TriangleError::Delta(error) => zset_error(error),
        TriangleError::NegativeMultiplicity => StandingQueryFailure::InvalidDelta,
    }
}

impl State {
    pub(super) fn output(&self) -> Output {
        self.counts.output
    }

    pub(super) fn columns(&self) -> &[String] {
        &self.counts.columns
    }

    pub(super) fn rows(&self) -> &ZSet<GraphValueRow> {
        &self.counts.rows
    }

    pub(super) fn delta(&self) -> Option<&ZSet<GraphValueRow>> {
        self.counts.last_delta.as_ref()
    }

    pub(super) fn maintain(
        &mut self,
        cx: &CommitCx,
        batch: &LogicalDeltaBatch,
        meter: &mut Meter<'_>,
    ) -> Result<(), StandingQueryFailure> {
        let Input::Committed(source) = &mut self.input else {
            return Err(StandingQueryFailure::InvalidDelta);
        };
        if self.selected.is_some() || self.frontier != source.frontier() {
            return Err(StandingQueryFailure::InvalidDelta);
        }
        recursive::observe_batch(batch, meter)?;
        let input = source
            .prepare_committed_successor(cx, batch, LIMBS, &mut |event| meter.charge(event))
            .map_err(components::input_error)?;
        let vertices = components::vertex_delta(batch, meter)?;
        let edges = components::project(input.delta(), self.relation, meter)?;
        let triangles = self
            .triangles
            .prepare(&edges, LIMBS, &mut |event| meter.charge(event))
            .map_err(triangle_error)?;
        let counts = self.counts.prepare(
            &vertices,
            &edges,
            triangles.edge_presence_updates(),
            triangles.delta(),
            meter,
        )?;
        (meter.checkpoint)()?;
        let _ = triangles.commit();
        counts.commit();
        let _ = input.commit();
        Ok(())
    }

    fn apply_selected(
        &mut self,
        delta: &ZSet<GraphValueRow>,
        meter: &mut Meter<'_>,
    ) -> Result<(), StandingQueryFailure> {
        let definition = self.selected.ok_or(StandingQueryFailure::InvalidDelta)?;
        let Input::Selected(support) = &mut self.input else {
            return Err(StandingQueryFailure::InvalidDelta);
        };
        let projection = project_rows(delta, definition, meter)?;
        let vertices = support
            .prepare(&projection.vertices, &ZSet::new(), LIMBS, &mut |event| {
                meter.charge(event)
            })
            .map_err(support_error)?;
        let triangles = self
            .triangles
            .prepare(&projection.edges, LIMBS, &mut |event| meter.charge(event))
            .map_err(triangle_error)?;
        let counts = self.counts.prepare(
            vertices.delta(),
            &projection.edges,
            triangles.edge_presence_updates(),
            triangles.delta(),
            meter,
        )?;
        // Parent support, topology, counters and rows advance together. Every
        // fallible admission and cancellation point precedes publication.
        (meter.checkpoint)()?;
        let _ = triangles.commit();
        counts.commit();
        let _ = vertices.commit();
        Ok(())
    }

    pub(super) fn maintain_with_sources(
        &mut self,
        cx: &CommitCx,
        batch: &LogicalDeltaBatch,
        sources: &[StandingQuery],
        meter: &mut Meter<'_>,
    ) -> Result<(), StandingQueryFailure> {
        let Some(definition) = self.selected else {
            return self.maintain(cx, batch, meter);
        };
        meter.charge(ZSetEvent::Work)?;
        let at = batch.commit_seq();
        if self.frontier.checked_successor().ok() != Some(at)
            || batch.frontier() != at
            || batch.commit_marker_identity().commit_seq != at
        {
            return Err(StandingQueryFailure::InvalidDelta);
        }
        let source = sets::input_at(sources, definition.input, at)?;
        if !definition.matches_schema(source) {
            return Err(StandingQueryFailure::DependencyUnavailable);
        }
        let delta = sets::delta(source).ok_or(StandingQueryFailure::DependencyUnavailable)?;
        meter.stats.delta_rows =
            u64::try_from(delta.len()).map_err(|_| StandingQueryFailure::WorkBudget)?;
        self.apply_selected(delta, meter)
    }

    fn from_selected(
        source: &ZSet<GraphValueRow>,
        definition: Definition,
        output: Output,
        frontier: CommitSeq,
        meter: &mut Meter<'_>,
    ) -> Result<Self, StandingQueryFailure> {
        let projection = project_rows(source, definition, meter)?;
        let mut support = VertexSupport::new(SetOperation::UnionDistinct);
        let vertices = support
            .prepare(&projection.vertices, &ZSet::new(), LIMBS, &mut |event| {
                meter.charge(event)
            })
            .map_err(support_error)?;
        let mut triangles = IncrementalTriangles::new(TriangleQuantifier::Distinct);
        let pending = triangles
            .prepare(&projection.edges, LIMBS, &mut |event| meter.charge(event))
            .map_err(triangle_error)?;
        let mut counts = Counts::new(output, meter)?;
        let sink = counts.prepare(
            vertices.delta(),
            &projection.edges,
            pending.edge_presence_updates(),
            pending.delta(),
            meter,
        )?;
        (meter.checkpoint)()?;
        let _ = pending.commit();
        sink.commit();
        let _ = vertices.commit();
        counts.last_delta = None;
        Ok(Self {
            input: Input::Selected(support),
            triangles,
            counts,
            relation: None,
            selected: Some(definition),
            policy: meter.policy,
            frontier,
            stats: meter.stats,
            failure: None,
        })
    }

    fn from_snapshot(
        snapshot: &crate::Snapshot,
        relation: Option<RelationId>,
        output: Output,
        meter: &mut Meter<'_>,
    ) -> Result<Self, StandingQueryFailure> {
        let components::TopologyInput {
            input,
            vertices,
            edges,
        } = components::topology_input(snapshot, relation, meter)?;
        let mut triangles = IncrementalTriangles::new(TriangleQuantifier::Distinct);
        let pending = triangles
            .prepare(&edges, LIMBS, &mut |event| meter.charge(event))
            .map_err(triangle_error)?;
        let mut counts = Counts::new(output, meter)?;
        let sink = counts.prepare(
            &vertices,
            &edges,
            pending.edge_presence_updates(),
            pending.delta(),
            meter,
        )?;
        (meter.checkpoint)()?;
        let _ = pending.commit();
        sink.commit();
        counts.last_delta = None;
        Ok(Self {
            input: Input::Committed(input),
            triangles,
            counts,
            relation,
            selected: None,
            policy: meter.policy,
            frontier: snapshot.frontier,
            stats: meter.stats,
            failure: None,
        })
    }
}

impl<V: Vfs + Clone> Database<V> {
    /// Maintain per-vertex triangle counts over an earlier maintained row source.
    /// `endpoints` selects two zero-based Vertex columns, checked even on empty
    /// input. The vertex population is exactly the non-NULL identities supported
    /// by those columns. Two non-NULL endpoints contribute an undirected edge
    /// per row occurrence; one contributes an isolate; two NULLs contribute
    /// nothing. Parallel/opposite occurrences collapse to simple adjacency,
    /// and self-pairs retain a vertex without contributing triangles or degree.
    /// Counted support retires a vertex or edge only after its last witness.
    ///
    /// Native columns are `vertex, triangles`, one row per selected vertex,
    /// including isolates with count zero. The handle supports native queries,
    /// cursors, signed deltas, row-circuit composition and subscription delivery.
    /// Parent filters/joins/pages execute before this topology projection, so
    /// property-only changes can alter triangles without changing a base edge.
    ///
    /// Initialization/rebuild consumes the current parent bag; each later commit
    /// consumes its complete accepted derivative, never a graph rescan. A failed
    /// parent or rebuilt baseline is not an empty delta. Repair the independently
    /// owned parent first, then this view; rebuild preserves the endpoint columns.
    /// Failure fences this view without undoing durable writes or healthy siblings.
    ///
    /// `max_snapshot_records` bounds compressed parent rows at initialization;
    /// `max_result_rows` bounds final distinct vertices. Work/scratch cover
    /// projection, support, kernel and sinks cumulatively per node; the parent
    /// keeps its own policy. State is session-local and in-memory, with no durable
    /// registration, spill/byte bound or new authorization boundary.
    pub fn register_standing_triangle_counts_from_rows(
        &mut self,
        cx: &QueryCx,
        source: &StandingQueryHandle,
        endpoints: [usize; 2],
        policy: GqlQueryPolicy,
    ) -> Result<StandingQueryHandle, StandingQueryError> {
        self.register_selected_triangle_counts(cx, source, endpoints, Output::Triangles, policy)
    }

    /// Maintain native clustering coefficients over a maintained row source,
    /// with the same endpoint, NULL, support, budget and rebuild contract as
    /// [`Self::register_standing_triangle_counts_from_rows`]. Native columns are
    /// `vertex, score`. A vertex with distinct degree below two has score zero;
    /// otherwise its score is `2 * triangles / (degree * (degree - 1))`, with
    /// exact integer products converted once before the native binary64 division.
    /// Self-pairs retain isolates and never enter that degree or numerator.
    pub fn register_standing_clustering_coefficients_from_rows(
        &mut self,
        cx: &QueryCx,
        source: &StandingQueryHandle,
        endpoints: [usize; 2],
        policy: GqlQueryPolicy,
    ) -> Result<StandingQueryHandle, StandingQueryError> {
        self.register_selected_triangle_counts(cx, source, endpoints, Output::Clustering, policy)
    }

    fn register_selected_triangle_counts(
        &mut self,
        cx: &QueryCx,
        source: &StandingQueryHandle,
        endpoints: [usize; 2],
        output: Output,
        policy: GqlQueryPolicy,
    ) -> Result<StandingQueryHandle, StandingQueryError> {
        let definition = self.admit_standing_topology_source(cx, source, endpoints)?;
        let state =
            self.prepare_standing_selected_triangle_counts(cx, definition, output, policy)?;
        // The fixed presentation copy was admitted by Counts::new. This is
        // another view of one registry node, not another maintained evaluator.
        let layout = Arc::new(native::Layout::Rows {
            columns: state.columns().to_vec(),
        });
        cx.checkpoint().map_err(StandingQueryError::Interrupted)?;
        let mut handle = self.store_standing_query(StandingQuery::TriangleCounts(Box::new(state)));
        handle.native = Some(layout);
        Ok(handle)
    }

    pub(super) fn prepare_standing_selected_triangle_counts(
        &self,
        cx: &QueryCx,
        definition: Definition,
        output: Output,
        policy: GqlQueryPolicy,
    ) -> Result<State, StandingQueryError> {
        cx.checkpoint().map_err(StandingQueryError::Interrupted)?;
        self.ensure_readable().map_err(StandingQueryError::Read)?;
        cx.with_restriction(|| {
            let mut checkpoint = || {
                cx.checkpoint()
                    .map_err(|_| StandingQueryFailure::Interrupted)
            };
            let mut meter = Meter {
                policy,
                stats: StandingQueryStats::default(),
                checkpoint: &mut checkpoint,
            };
            meter
                .charge(ZSetEvent::Work)
                .map_err(StandingQueryError::Maintenance)?;
            let parent = sets::input_at(
                &self.standing_queries,
                definition.input,
                self.registration_frontier(),
            )
            .map_err(StandingQueryError::Maintenance)?;
            if !definition.matches_schema(parent) {
                return Err(StandingQueryError::Maintenance(
                    StandingQueryFailure::DependencyUnavailable,
                ));
            }
            let rows = sets::rows(parent).ok_or(StandingQueryError::Maintenance(
                StandingQueryFailure::DependencyUnavailable,
            ))?;
            if policy
                .rows
                .max_snapshot_records()
                .is_some_and(|limit| rows.len() as u128 > u128::from(limit))
            {
                return Err(StandingQueryError::Maintenance(
                    StandingQueryFailure::SnapshotBudget,
                ));
            }
            State::from_selected(
                rows,
                definition,
                output,
                self.registration_frontier(),
                &mut meter,
            )
            .map_err(StandingQueryError::Maintenance)
        })
    }

    pub(super) fn prepare_standing_triangle_counts(
        &self,
        cx: &QueryCx,
        relation: Option<RelationId>,
        output: Output,
        policy: GqlQueryPolicy,
    ) -> Result<State, StandingQueryError> {
        cx.checkpoint().map_err(StandingQueryError::Interrupted)?;
        self.ensure_readable().map_err(StandingQueryError::Read)?;
        cx.with_restriction(|| {
            let mut checkpoint = || {
                cx.checkpoint()
                    .map_err(|_| StandingQueryFailure::Interrupted)
            };
            let mut meter = Meter {
                policy,
                stats: StandingQueryStats::default(),
                checkpoint: &mut checkpoint,
            };
            State::from_snapshot(&self.snapshot, relation, output, &mut meter)
                .map_err(StandingQueryError::Maintenance)
        })
    }
}

#[cfg(test)]
mod tests;
