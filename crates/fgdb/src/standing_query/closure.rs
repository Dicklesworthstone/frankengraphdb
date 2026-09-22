//! Recursive closure over a complete maintained row relation.
//!
//! The parent circuit owns source selection and row multiplicities. This node
//! projects two vertex columns, consolidates parallel support, and composes the
//! existing deletion-safe reachability kernel with a native row sink. It never
//! reopens graph storage or re-evaluates the parent's query on a commit.

use super::*;
use fgdb_delta_types::zset::ZSetUpdate;
use fgdb_delta_types::zset::reachability::{
    IncrementalReachability, ReachabilityError, ReachabilityUpdate,
};
use fgdb_delta_types::{LimbLimit, ZWeight};
use fgdb_gql::GraphSetColumnType;
use fgdb_gql::algebra::GraphValue;

const LIMBS: LimbLimit = LimbLimit::new(4);

pub(crate) struct State {
    pub(super) input: usize,
    pub(super) endpoints: [usize; 2],
    width: usize,
    columns: Vec<String>,
    operator: IncrementalReachability<VId>,
    rows: ZSet<GraphValueRow>,
    last_delta: Option<ZSet<GraphValueRow>>,
    pub(super) policy: GqlQueryPolicy,
    pub(super) frontier: CommitSeq,
    pub(super) stats: StandingQueryStats,
    pub(super) failure: Option<StandingQueryFailure>,
}

fn recursion_error(error: ReachabilityError<StandingQueryFailure>) -> StandingQueryFailure {
    match error {
        ReachabilityError::Delta(error) => zset_error(error),
        ReachabilityError::NegativeMultiplicity => StandingQueryFailure::InvalidDelta,
    }
}

fn vertex(value: &GraphValue) -> Result<Option<VId>, StandingQueryFailure> {
    match value {
        GraphValue::Vertex(id) => Ok(Some(*id)),
        value if value.is_null() => Ok(None),
        _ => Err(StandingQueryFailure::InvalidDelta),
    }
}

fn edge_delta(
    input: &ZSet<GraphValueRow>,
    width: usize,
    endpoints: [usize; 2],
    meter: &mut Meter<'_>,
) -> Result<ZSet<(VId, VId)>, StandingQueryFailure> {
    let mut output = ZSet::new();
    for (row, weight) in input.iter() {
        meter.charge(ZSetEvent::Work)?;
        if row.len() != width {
            return Err(StandingQueryFailure::InvalidDelta);
        }
        // Validate BOTH endpoints before skipping a NULL. Other columns are
        // opaque parent-owned payload, neither copied nor used as edge identity.
        let left = row
            .values()
            .get(endpoints[0])
            .ok_or(StandingQueryFailure::InvalidDelta)?;
        let right = row
            .values()
            .get(endpoints[1])
            .ok_or(StandingQueryFailure::InvalidDelta)?;
        meter.units(ZSetEvent::Work, 2)?;
        let left = vertex(left)?;
        let right = vertex(right)?;
        if let (Some(left), Some(right)) = (left, right) {
            let weight = weight
                .checked_clone(LIMBS)
                .map_err(|_| StandingQueryFailure::Arithmetic)?;
            output
                .accumulate((left, right), weight, LIMBS, &mut |event| {
                    meter.charge(event)
                })
                .map_err(zset_error)?;
        }
    }
    Ok(output)
}

impl State {
    pub(super) fn columns(&self) -> &[String] {
        &self.columns
    }
    pub(super) fn rows(&self) -> &ZSet<GraphValueRow> {
        &self.rows
    }
    pub(super) fn delta(&self) -> Option<&ZSet<GraphValueRow>> {
        self.last_delta.as_ref()
    }

    fn prepare(
        &mut self,
        delta: &ZSet<GraphValueRow>,
        meter: &mut Meter<'_>,
    ) -> Result<Update<'_>, StandingQueryFailure> {
        meter.charge(ZSetEvent::Work)?;
        let edges = edge_delta(delta, self.width, self.endpoints, meter)?;
        let input = self
            .operator
            .prepare(&edges, LIMBS, &mut |event| meter.charge(event))
            .map_err(recursion_error)?;
        let mut output = ZSet::new();
        let mut inserted = 0_u128;
        let mut removed = 0_u128;
        for ((source, destination), weight) in input.delta().iter() {
            meter.charge(ZSetEvent::Work)?;
            // Fixed-width native identities, never narrowed to scalar integers.
            meter.units(ZSetEvent::ScratchEntry, 3)?;
            let row = GraphValueRow::from_owned_values(vec![
                GraphValue::Vertex(*source),
                GraphValue::Vertex(*destination),
            ]);
            match weight.to_i128() {
                Some(1) if self.rows.weight(&row).is_none() => {
                    inserted = inserted
                        .checked_add(1)
                        .ok_or(StandingQueryFailure::Arithmetic)?;
                }
                Some(-1)
                    if self
                        .rows
                        .weight(&row)
                        .is_some_and(|old| old == &ZWeight::ONE) =>
                {
                    removed = removed
                        .checked_add(1)
                        .ok_or(StandingQueryFailure::Arithmetic)?;
                }
                _ => return Err(StandingQueryFailure::InvalidDelta),
            }
            let weight = weight
                .checked_clone(LIMBS)
                .map_err(|_| StandingQueryFailure::Arithmetic)?;
            output
                .accumulate(row, weight, LIMBS, &mut |event| meter.charge(event))
                .map_err(zset_error)?;
        }
        // Check the FINAL cardinality, not a key-ordered insertion-first prefix.
        let count = (self.rows.len() as u128)
            .checked_sub(removed)
            .and_then(|count| count.checked_add(inserted))
            .ok_or(StandingQueryFailure::InvalidDelta)?;
        if meter
            .policy
            .rows
            .max_result_rows()
            .is_some_and(|limit| count > u128::from(limit))
        {
            return Err(StandingQueryFailure::ResultBudget);
        }
        for _ in output.iter() {
            meter.charge(ZSetEvent::Work)?;
            meter.units(ZSetEvent::ScratchEntry, 3)?;
        }
        let sink = self
            .rows
            .prepare_update(&output, LIMBS, &mut |event| meter.charge(event))
            .map_err(zset_error)?;
        (meter.checkpoint)()?;
        Ok(Update {
            input,
            sink,
            output,
            last_delta: &mut self.last_delta,
        })
    }

    pub(super) fn maintain(
        &mut self,
        batch: &LogicalDeltaBatch,
        sources: &[StandingQuery],
        meter: &mut Meter<'_>,
    ) -> Result<(), StandingQueryFailure> {
        meter.charge(ZSetEvent::Work)?;
        let at = batch.commit_seq();
        if self.frontier.checked_successor().ok() != Some(at)
            || batch.frontier() != at
            || batch.commit_marker_identity().commit_seq != at
        {
            return Err(StandingQueryFailure::InvalidDelta);
        }
        let source = sets::input_at(sources, self.input, at)?;
        let delta = sets::delta(source).ok_or(StandingQueryFailure::DependencyUnavailable)?;
        meter.stats.delta_rows =
            u64::try_from(delta.len()).map_err(|_| StandingQueryFailure::WorkBudget)?;
        self.prepare(delta, meter)?.commit();
        Ok(())
    }
}

#[must_use = "dropping a closure update preserves edge support, recursive state and output"]
struct Update<'a> {
    input: ReachabilityUpdate<'a, VId>,
    sink: ZSetUpdate<'a, GraphValueRow>,
    output: ZSet<GraphValueRow>,
    last_delta: &'a mut Option<ZSet<GraphValueRow>>,
}
impl Update<'_> {
    fn commit(self) {
        let Self {
            input,
            sink,
            output,
            last_delta,
        } = self;
        let _ = input.commit();
        sink.commit();
        *last_delta = Some(output);
    }
}

impl<V: Vfs + Clone> Database<V> {
    /// Maintain one-or-more-step transitive closure of an existing row circuit.
    /// `endpoints` selects zero-based source/destination columns. Both must have
    /// the declared Vertex domain, even when the input is empty. NULL in either
    /// endpoint contributes no edge. Full-width identities are never coerced.
    /// Output columns are `source` and `destination`, each with multiplicity one.
    /// A self pair requires a nonempty cycle: no zero-hop identity is invented.
    ///
    /// Input filters, joins, UNION/EXCEPT, DISTINCT and windows execute BEFORE
    /// recursion. Several rows/occurrences supporting an edge are consolidated
    /// exactly; deleting one witness cannot remove the last remaining support.
    /// Deletions use the existing deletion-safe recursive kernel, not path-count
    /// subtraction or a cycle that supports itself after its seed disappeared.
    /// This is endpoint SET reachability, not WALK path counts, SHORTEST paths,
    /// a hop-length bound, or a new GQL grammar. Closure storage can be quadratic.
    ///
    /// The returned native handle supports cursors, bag/delta delivery and
    /// acknowledged replay subscriptions. Its rows may feed later sets, joins,
    /// projections, grouping and closure nodes in the same acyclic registry.
    /// The recursive feedback remains INSIDE this node; registry dependencies
    /// always name earlier nodes. All inputs for a tick must be current/healthy.
    /// A refused child never rolls back the durable commit or accepted siblings.
    ///
    /// Initialization/rebuild uses the CURRENT selected parent bag, not the
    /// graph or historical deltas. max_snapshot_records counts compressed parent
    /// support; max_result_rows counts final reachable pairs. Work/scratch cover
    /// projection, recursion and output preparation, per node, not total process
    /// bytes. Empty ticks still publish a derivative. Repair the parent first,
    /// then rebuild this node; rebuild starts a baseline, not a missing delta.
    /// This is session-local, without a durable catalog or spill arrangements.
    pub fn register_standing_closure(
        &mut self,
        cx: &QueryCx,
        source: &StandingQueryHandle,
        endpoints: [usize; 2],
        policy: GqlQueryPolicy,
    ) -> Result<StandingQueryHandle, StandingQueryError> {
        self.admitted_standing_query(cx, source)?;
        let state = self.prepare_standing_closure(
            cx,
            source.index,
            endpoints,
            policy,
            self.standing_queries.len(),
        )?;
        let layout = Arc::new(native::Layout::Rows {
            columns: state.columns.clone(),
        });
        cx.checkpoint().map_err(StandingQueryError::Interrupted)?;
        let mut handle = self.store_standing_query(StandingQuery::Closure(Box::new(state)));
        handle.native = Some(layout);
        Ok(handle)
    }

    pub(super) fn prepare_standing_closure(
        &self,
        cx: &QueryCx,
        input: usize,
        endpoints: [usize; 2],
        policy: GqlQueryPolicy,
        before: usize,
    ) -> Result<State, StandingQueryError> {
        cx.checkpoint().map_err(StandingQueryError::Interrupted)?;
        self.ensure_readable().map_err(StandingQueryError::Read)?;
        let sources = self
            .standing_queries
            .get(..before)
            .ok_or(StandingQueryError::UnknownHandle)?;
        let at = self.snapshot.frontier;
        cx.with_restriction(|| {
            let parent =
                sets::input_at(sources, input, at).map_err(StandingQueryError::Maintenance)?;
            let names = sets::columns(parent).ok_or(StandingQueryError::Unsupported)?;
            for column in endpoints {
                if column >= names.len()
                    || sets::column_type(parent, column) != Some(GraphSetColumnType::Vertex)
                {
                    return Err(StandingQueryError::Unsupported);
                }
            }
            let rows = sets::rows(parent).ok_or(StandingQueryError::Unsupported)?;
            if policy
                .rows
                .max_snapshot_records()
                .is_some_and(|limit| rows.len() as u128 > u128::from(limit))
            {
                return Err(StandingQueryError::Maintenance(
                    StandingQueryFailure::SnapshotBudget,
                ));
            }
            let mut checkpoint = || {
                cx.checkpoint()
                    .map_err(|_| StandingQueryFailure::Interrupted)
            };
            let mut meter = Meter {
                policy,
                stats: StandingQueryStats::default(),
                checkpoint: &mut checkpoint,
            };
            // Fixed metadata/state slots, including the native presentation copy.
            meter
                .units(ZSetEvent::ScratchEntry, 8)
                .map_err(StandingQueryError::Maintenance)?;
            let mut state = State {
                input,
                endpoints,
                width: names.len(),
                columns: vec!["source".into(), "destination".into()],
                operator: IncrementalReachability::new(),
                rows: ZSet::new(),
                last_delta: None,
                policy,
                frontier: at,
                stats: StandingQueryStats::default(),
                failure: None,
            };
            state
                .prepare(rows, &mut meter)
                .map_err(StandingQueryError::Maintenance)?
                .commit();
            state.last_delta = None;
            state.stats = meter.stats;
            Ok(state)
        })
    }

    /// Borrow the current canonical pair set in native vertex cells. No ordered
    /// page is attached; downstream ranking may select a page explicitly.
    pub fn standing_closure<'a>(
        &'a self,
        cx: &QueryCx,
        handle: &StandingQueryHandle,
    ) -> Result<StandingQueryView<'a, GraphValueRow>, StandingQueryError> {
        let StandingQuery::Closure(state) = self.admitted_standing_query(cx, handle)? else {
            return Err(StandingQueryError::Unsupported);
        };
        Ok(StandingQueryView {
            rows: &state.rows,
            ordered: None,
            frontier: state.frontier,
            stats: &state.stats,
        })
    }
}

#[cfg(test)]
mod tests;
