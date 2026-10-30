//! Native per-vertex triangle counts and clustering coefficients over the
//! existing committed topology and exact DISTINCT triangle derivative. Only
//! vertex counters and native rows are retained here; the typed triangle-triple
//! API keeps its separate profile. Distinct degrees use the same authoritative
//! prepared edge support, including multiplicity-only and opposite-edge ticks.
//! Bootstrap still owns the kernel's transient complete triangle derivative.

use super::*;
use fgdb_delta_types::zset::ZSetUpdate;
use fgdb_delta_types::zset::committed::CommittedEdgeInput;
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

pub(crate) struct State {
    input: CommittedEdgeInput,
    triangles: IncrementalTriangles<VId>,
    counts: Counts,
    pub(super) relation: RelationId,
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
        if self.frontier != self.input.frontier() {
            return Err(StandingQueryFailure::InvalidDelta);
        }
        recursive::observe_batch(batch, meter)?;
        let input = self
            .input
            .prepare_committed_successor(cx, batch, LIMBS, &mut |event| meter.charge(event))
            .map_err(components::input_error)?;
        let vertices = components::vertex_delta(batch, meter)?;
        let edges = components::project(input.delta(), Some(self.relation), meter)?;
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

    fn from_snapshot(
        snapshot: &crate::Snapshot,
        relation: RelationId,
        output: Output,
        meter: &mut Meter<'_>,
    ) -> Result<Self, StandingQueryFailure> {
        let components::TopologyInput {
            input,
            vertices,
            edges,
        } = components::topology_input(snapshot, Some(relation), meter)?;
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
            input,
            triangles,
            counts,
            relation,
            policy: meter.policy,
            frontier: snapshot.frontier,
            stats: meter.stats,
            failure: None,
        })
    }
}

impl<V: Vfs + Clone> Database<V> {
    pub(super) fn prepare_standing_triangle_counts(
        &self,
        cx: &QueryCx,
        relation: RelationId,
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
