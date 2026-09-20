//! Dependency-ordered set circuits over already maintained native row bags.
//!
//! Parents retain their own query semantics, policy and failure boundary. A
//! child consumes both accepted derivatives for one commit or neither. The
//! existing IncrementalSet owns all six bag laws; this module owns only the
//! registry edges, schema admission, final quota and prepared result sink.

use super::*;
use fgdb_delta_types::zset::set::{IncrementalSet, SetError, SetOperation, SetUpdate};
use fgdb_delta_types::zset::ZSetUpdate;
use fgdb_delta_types::{LimbLimit, ZWeight};
use fgdb_gql::{GraphSetBuildError, GraphSetColumnType};

const LIMBS: LimbLimit = LimbLimit::new(4);

pub(crate) struct State {
    pub(super) inputs: [usize; 2],
    columns: Vec<String>,
    types: Vec<GraphSetColumnType>,
    input: IncrementalSet<GraphValueRow>,
    rows: ZSet<GraphValueRow>,
    total: ZWeight,
    last_delta: Option<ZSet<GraphValueRow>>,
    pub(super) policy: GqlQueryPolicy,
    pub(super) frontier: CommitSeq,
    pub(super) stats: StandingQueryStats,
    pub(super) failure: Option<StandingQueryFailure>,
}

// Shared maintained-row interface for set and join circuit dependencies.
pub(super) fn columns(query: &StandingQuery) -> Option<&[String]> {
    match query {
        StandingQuery::Rows { output, .. } => Some(output.definition().columns()),
        StandingQuery::Set(query) => Some(&query.columns),
        StandingQuery::Join(query) => Some(query.columns()),
        StandingQuery::Projection(query) => Some(query.columns()),
        _ => None,
    }
}
pub(super) fn column_type(query: &StandingQuery, column: usize) -> Option<GraphSetColumnType> {
    match query {
        StandingQuery::Rows { output, .. } => output.definition().value_columns()
            .get(column).map(GraphSetColumnType::from),
        StandingQuery::Set(query) => query.types.get(column).copied(),
        StandingQuery::Join(query) => query.spec().column_types().nth(column),
        StandingQuery::Projection(query) => query.spec().column_types().get(column).copied(),
        _ => None,
    }
}
pub(super) fn rows(query: &StandingQuery) -> Option<&ZSet<GraphValueRow>> {
    match query {
        StandingQuery::Rows { output, .. } => Some(&output.rows),
        StandingQuery::Set(query) => Some(&query.rows),
        StandingQuery::Join(query) => Some(query.rows()),
        StandingQuery::Projection(query) => Some(query.rows()),
        _ => None,
    }
}
pub(super) fn delta(query: &StandingQuery) -> Option<&ZSet<GraphValueRow>> {
    match query {
        StandingQuery::Rows { output, .. } => output.last_delta.as_ref(),
        StandingQuery::Set(query) => query.last_delta.as_ref(),
        StandingQuery::Join(query) => query.delta(),
        StandingQuery::Projection(query) => query.delta(),
        _ => None,
    }
}
pub(super) fn input_at(
    sources: &[StandingQuery], index: usize, at: CommitSeq,
) -> Result<&StandingQuery, StandingQueryFailure> {
    let query = sources.get(index).ok_or(StandingQueryFailure::DependencyUnavailable)?;
    let (_, frontier, failure) = query.status();
    if failure.is_some() || frontier != at || rows(query).is_none() {
        return Err(StandingQueryFailure::DependencyUnavailable);
    }
    Ok(query)
}
fn set_error(error: SetError<StandingQueryFailure>) -> StandingQueryFailure {
    match error {
        SetError::Delta(error) => zset_error(error),
        SetError::NegativeMultiplicity { .. } => StandingQueryFailure::InvalidDelta,
    }
}
fn reserve_row(row: &GraphValueRow, meter: &mut Meter<'_>) -> Result<(), StandingQueryFailure> {
    meter.charge(ZSetEvent::ScratchEntry)?;
    for value in row.values() {
        meter.charge(ZSetEvent::Work)?;
        let units = value.payload_units().checked_add(1).ok_or(StandingQueryFailure::ScratchBudget)?;
        meter.units(ZSetEvent::ScratchEntry, units)?;
    }
    Ok(())
}
fn result_bound(total: &ZWeight, policy: GqlQueryPolicy) -> Result<(), StandingQueryFailure> {
    if total < &ZWeight::ZERO { return Err(StandingQueryFailure::InvalidDelta); }
    if policy.rows.max_result_rows().is_some_and(|limit|
        total > &ZWeight::from_i128(i128::from(limit))) {
        return Err(StandingQueryFailure::ResultBudget);
    }
    Ok(())
}

impl State {
    pub(super) fn operation(&self) -> SetOperation { self.input.operation() }

    fn prepare(
        &mut self, left: &ZSet<GraphValueRow>, right: &ZSet<GraphValueRow>, meter: &mut Meter<'_>,
    ) -> Result<Update<'_>, StandingQueryFailure> {
        // Reserve payload copies before the generic algebra can clone keys:
        // one input replacement and one possible output per changed input key.
        // Shared keys may be over-reserved; unchanged inputs are not visited.
        for side in [left, right] {
            for (row, _) in side.iter() {
                meter.charge(ZSetEvent::Work)?;
                reserve_row(row, meter)?;
                reserve_row(row, meter)?;
            }
        }
        let input = self.input.prepare(left, right, LIMBS, &mut |event| meter.charge(event))
            .map_err(set_error)?;
        let change = input.delta().total_weight(LIMBS, &mut |event| meter.charge(event))
            .map_err(zset_error)?;
        meter.charge(ZSetEvent::Work)?;
        let next_total = self.total.checked_add(&change, LIMBS)
            .map_err(|_| StandingQueryFailure::Arithmetic)?;
        // Final occurrence count, not distinct keys or an insertion-first prefix.
        result_bound(&next_total, meter.policy)?;
        for (row, _) in input.delta().iter() { reserve_row(row, meter)?; }
        let sink = self.rows.prepare_update(input.delta(), LIMBS, &mut |event| meter.charge(event))
            .map_err(zset_error)?;
        for (row, _) in input.delta().iter() {
            meter.charge(ZSetEvent::Work)?;
            if sink.weight(row).is_some_and(|weight| weight < &ZWeight::ZERO) {
                return Err(StandingQueryFailure::InvalidDelta);
            }
        }
        (meter.checkpoint)()?;
        Ok(Update { input, sink, total: &mut self.total, last_delta: &mut self.last_delta, next_total })
    }

    pub(super) fn maintain(
        &mut self, batch: &LogicalDeltaBatch, sources: &[StandingQuery], meter: &mut Meter<'_>,
    ) -> Result<(), StandingQueryFailure> {
        meter.charge(ZSetEvent::Work)?;
        let at = batch.commit_seq();
        if self.frontier.checked_successor().map_err(|_| StandingQueryFailure::InvalidDelta)? != at
            || batch.frontier() != at || batch.commit_marker_identity().commit_seq != at {
            return Err(StandingQueryFailure::InvalidDelta);
        }
        let left = input_at(sources, self.inputs[0], at)?;
        meter.charge(ZSetEvent::Work)?;
        let right = input_at(sources, self.inputs[1], at)?;
        let left = delta(left).ok_or(StandingQueryFailure::DependencyUnavailable)?;
        let right = delta(right).ok_or(StandingQueryFailure::DependencyUnavailable)?;
        meter.stats.delta_rows = u64::try_from(left.len()).ok()
            .and_then(|left| u64::try_from(right.len()).ok().and_then(|right| left.checked_add(right)))
            .ok_or(StandingQueryFailure::WorkBudget)?;
        self.prepare(left, right, meter)?.commit();
        Ok(())
    }
}

#[must_use = "dropping a set-view update preserves both accepted inputs and output"]
struct Update<'a> {
    input: SetUpdate<'a, GraphValueRow>,
    sink: ZSetUpdate<'a, GraphValueRow>,
    total: &'a mut ZWeight,
    last_delta: &'a mut Option<ZSet<GraphValueRow>>,
    next_total: ZWeight,
}
impl Update<'_> {
    fn commit(self) {
        let Self { input, sink, total, last_delta, next_total } = self;
        let delta = input.commit();
        sink.commit();
        *total = next_total;
        *last_delta = Some(delta);
    }
}

impl<V: Vfs + Clone> Database<V> {
    /// Compose two existing standing row/set/join views using UNION, INTERSECT or
    /// EXCEPT with ALL/DISTINCT bag semantics. Operands keep their own DISTINCT,
    /// ordering and page BEFORE composition. Canonical full-row equality makes
    /// NULL equal NULL here; scalar and vertex domains are never coerced.
    /// Column positions/types must agree, even for empty inputs. Output names
    /// come from the left operand. The same handle may supply both operands.
    ///
    /// Shared parents are maintained once. Sets and joins may feed each other in
    /// an append-ordered acyclic circuit. Each commit uses both complete input
    /// derivatives at that exact sequence, or leaves this child unavailable.
    /// Parent/sibling success is not undone by a child's policy refusal. Repair
    /// unavailable parents before rebuilding dependents with the existing API.
    ///
    /// Initialization/rebuild read CURRENT maintained bags, not graph storage or
    /// history. max_snapshot_records bounds the sum of their compressed support
    /// sizes (a shared operand counts twice). max_result_rows bounds exact final
    /// occurrences without expanding duplicates. Each view has its own work/
    /// scratch allowance; these are logical events/payload units, not allocator
    /// bytes, total-circuit quotas or spill. Set delta_rows counts changed input
    /// tuples, not raw graph deltas; affected vertex/edge counters stay zero.
    ///
    /// This is a session-local typed composition API, not new query grammar,
    /// arbitrary relational circuits, ordered output paging, or a durable feed.
    pub fn register_standing_set(
        &mut self, cx: &QueryCx, left: &StandingQueryHandle, right: &StandingQueryHandle,
        operation: SetOperation, policy: GqlQueryPolicy,
    ) -> Result<StandingQueryHandle, StandingQueryError> {
        cx.checkpoint().map_err(StandingQueryError::Interrupted)?;
        if !Arc::ptr_eq(&self.handle_owner, &left.owner) || !Arc::ptr_eq(&self.handle_owner, &right.owner) {
            return Err(StandingQueryError::ForeignHandle);
        }
        for handle in [left, right] {
            let query = self.admitted_standing_query(cx, handle)?;
            if rows(query).is_none() { return Err(StandingQueryError::Unsupported); }
        }
        let query = self.prepare_standing_set(cx, [left.index, right.index], operation, policy,
            self.standing_queries.len())?;
        Ok(self.store_standing_query(StandingQuery::Set(Box::new(query))))
    }

    pub(super) fn prepare_standing_set(
        &self, cx: &QueryCx, inputs: [usize; 2], operation: SetOperation,
        policy: GqlQueryPolicy, before: usize,
    ) -> Result<State, StandingQueryError> {
        cx.checkpoint().map_err(StandingQueryError::Interrupted)?;
        self.ensure_readable().map_err(StandingQueryError::Read)?;
        let sources = self.standing_queries.get(..before).ok_or(StandingQueryError::UnknownHandle)?;
        let at = self.snapshot.frontier;
        cx.with_restriction(|| {
            let left = input_at(sources, inputs[0], at).map_err(StandingQueryError::Maintenance)?;
            let right = input_at(sources, inputs[1], at).map_err(StandingQueryError::Maintenance)?;
            let left_names = columns(left).ok_or(StandingQueryError::Unsupported)?;
            let right_names = columns(right).ok_or(StandingQueryError::Unsupported)?;
            if left_names.len() != right_names.len() {
                return Err(StandingQueryError::SetSchema(GraphSetBuildError::ColumnCount {
                    left: left_names.len(), right: right_names.len(),
                }));
            }
            let mut checkpoint = || cx.checkpoint().map_err(|_| StandingQueryFailure::Interrupted);
            let mut meter = Meter { policy, stats: StandingQueryStats::default(), checkpoint: &mut checkpoint };
            let mut names = Vec::new();
            let mut types = Vec::new();
            for (column, name) in left_names.iter().enumerate() {
                meter.charge(ZSetEvent::Work).map_err(StandingQueryError::Maintenance)?;
                let l = column_type(left, column).ok_or(StandingQueryError::Unsupported)?;
                let r = column_type(right, column).ok_or(StandingQueryError::Unsupported)?;
                if l != r {
                    return Err(StandingQueryError::SetSchema(GraphSetBuildError::ColumnType {
                        column, left: l, right: r,
                    }));
                }
                meter.units(ZSetEvent::ScratchEntry, 2 + name.len().div_ceil(64))
                    .map_err(StandingQueryError::Maintenance)?;
                names.push(name.clone()); types.push(l);
            }
            let left = rows(left).ok_or(StandingQueryError::Unsupported)?;
            let right = rows(right).ok_or(StandingQueryError::Unsupported)?;
            let records = (left.len() as u128) + (right.len() as u128);
            if policy.rows.max_snapshot_records().is_some_and(|limit| records > u128::from(limit)) {
                return Err(StandingQueryError::Maintenance(StandingQueryFailure::SnapshotBudget));
            }
            meter.charge(ZSetEvent::ScratchEntry).map_err(StandingQueryError::Maintenance)?;
            let mut query = State {
                inputs, columns: names, types, input: IncrementalSet::new(operation), rows: ZSet::new(),
                total: ZWeight::ZERO, last_delta: None, policy, frontier: at,
                stats: StandingQueryStats::default(), failure: None,
            };
            query.prepare(left, right, &mut meter).map_err(StandingQueryError::Maintenance)?.commit();
            query.last_delta = None; // New baseline; never reinterpret it as a successor.
            query.stats = meter.stats;
            Ok(query)
        })
    }

    /// Borrow the current compressed set result. Native key order is canonical;
    /// ordered_rows() is None and ALL multiplicities are not expanded.
    pub fn standing_set<'a>(
        &'a self, cx: &QueryCx, handle: &StandingQueryHandle,
    ) -> Result<StandingQueryView<'a, GraphValueRow>, StandingQueryError> {
        let StandingQuery::Set(query) = self.admitted_standing_query(cx, handle)? else {
            return Err(StandingQueryError::Unsupported);
        };
        Ok(StandingQueryView { rows: &query.rows, ordered: None, frontier: query.frontier, stats: &query.stats })
    }

    /// Exact latest-tick multiplicity derivative, with the same one-tick and
    /// baseline-versus-empty contract as standing_row_delta. No ACK/backlog.
    pub fn standing_set_delta<'a>(
        &'a self, cx: &QueryCx, handle: &StandingQueryHandle,
    ) -> Result<Option<StandingQueryView<'a, GraphValueRow>>, StandingQueryError> {
        let StandingQuery::Set(query) = self.admitted_standing_query(cx, handle)? else {
            return Err(StandingQueryError::Unsupported);
        };
        Ok(query.last_delta.as_ref().map(|rows| StandingQueryView {
            rows, ordered: None, frontier: query.frontier, stats: &query.stats,
        }))
    }

    /// Borrow the left operand's column names without inspecting result rows.
    pub fn standing_set_columns<'a>(
        &'a self, cx: &QueryCx, handle: &StandingQueryHandle,
    ) -> Result<&'a [String], StandingQueryError> {
        let StandingQuery::Set(query) = self.admitted_standing_query(cx, handle)? else {
            return Err(StandingQueryError::Unsupported);
        };
        Ok(&query.columns)
    }

    /// Borrow the exact accepted occurrence count, without scanning or expanding.
    pub fn standing_set_total<'a>(
        &'a self, cx: &QueryCx, handle: &StandingQueryHandle,
    ) -> Result<&'a ZWeight, StandingQueryError> {
        let StandingQuery::Set(query) = self.admitted_standing_query(cx, handle)? else {
            return Err(StandingQueryError::Unsupported);
        };
        Ok(&query.total)
    }
}

#[cfg(test)]
mod tests;
