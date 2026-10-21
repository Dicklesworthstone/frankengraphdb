//! Exact net changes between two complete query-result bags.
//!
//! This is the snapshot-evaluation physical path for result differencing, not
//! replay of intervening writes. A host admits/pins both endpoints and executes
//! the SAME bound query at each. This operator only adapts its completed rows
//! and subtracts exact Z-sets. It neither reads a graph nor grants authority.

use crate::algebra::{GraphValue, GraphValueRow, MAX_PATTERN_VERTICES};
use crate::{
    GlaExecutionEvent, GlaExecutionLimits, GlaExecutionStats, GlaLimitDimension, GlaLimitExceeded,
    GqlBudgetDimension, GqlExecutionBudget, GqlExecutionStats, GqlQueryError, GqlQueryExecution,
    GqlQueryPolicy, GraphAggregateRow, GraphAggregateTextSlot, GraphAggregateValue,
};
use fgdb_delta_types::{LimbLimit, ZSet, ZSetError, ZSetEvent, ZWeight};
use fgdb_types::CommitSeq;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiffEndpoint {
    Before,
    After,
}
impl DiffEndpoint {
    pub fn sequence(self, before: CommitSeq, after: CommitSeq) -> CommitSeq {
        match self {
            Self::Before => before,
            Self::After => after,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum GraphDiffError<E> {
    Definition(E),
    Endpoint {
        endpoint: DiffEndpoint,
        source: E,
    },
    /// Explicit historical selectors would override one or both endpoints.
    TemporalSelector,
    ColumnCount {
        observed: usize,
    },
    RowWidth {
        endpoint: DiffEndpoint,
        expected: usize,
        observed: usize,
    },
    AggregateLayout {
        endpoint: DiffEndpoint,
    },
    InvalidValue {
        endpoint: DiffEndpoint,
        column: usize,
    },
    Arithmetic,
}
impl<E: core::fmt::Display> core::fmt::Display for GraphDiffError<E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Definition(error) => write!(f, "diff definition: {error}"),
            Self::Endpoint { endpoint, source } => {
                write!(f, "diff {endpoint:?} endpoint: {source}")
            }
            Self::TemporalSelector => {
                f.write_str("diff requires a query without an explicit historical selector")
            }
            Self::ColumnCount { observed } => write!(f, "diff schema has {observed} columns"),
            Self::RowWidth {
                endpoint,
                expected,
                observed,
            } => write!(
                f,
                "diff {endpoint:?} row width {observed}, expected {expected}"
            ),
            Self::AggregateLayout { endpoint } => {
                write!(f, "diff {endpoint:?} aggregate layout mismatch")
            }
            Self::InvalidValue { endpoint, column } => write!(
                f,
                "diff {endpoint:?} column {column} exceeds native value bounds"
            ),
            Self::Arithmetic => f.write_str("diff exact arithmetic or count overflow"),
        }
    }
}
impl<E: core::error::Error + 'static> core::error::Error for GraphDiffError<E> {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Definition(error) | Self::Endpoint { source: error, .. } => Some(error),
            _ => None,
        }
    }
}

type Row = Box<[GraphAggregateValue]>;
type Error<E, C> = GqlQueryError<GraphDiffError<E>, C>;

/// Completed result changes without source or evaluator diagnostics. A host
/// may release these after its own final authorization/delivery admission.
/// This value grants no authority and does not claim physical noninterference.
/// Equality and Debug cannot expose discarded work or source cardinalities.
#[derive(PartialEq, Eq)]
pub struct GraphResultChanges {
    before: CommitSeq,
    after: CommitSeq,
    columns: Box<[String]>,
    changes: ZSet<Row>,
}
impl GraphResultChanges {
    pub fn before(&self) -> CommitSeq {
        self.before
    }
    pub fn after(&self) -> CommitSeq {
        self.after
    }
    pub fn columns(&self) -> &[String] {
        &self.columns
    }
    pub fn changes(&self) -> &ZSet<Row> {
        &self.changes
    }
}
impl core::fmt::Debug for GraphResultChanges {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("GraphResultChanges")
            .field("before", &self.before)
            .field("after", &self.after)
            .field("data", &"[REDACTED]")
            .finish()
    }
}

/// The existing source engine's result and measured usage, not a new evaluator.
/// Aggregate slots borrow frozen RETURN metadata and preserve repeated aliases.
/// Constructors do not clone rows, payloads, or layout metadata.
pub struct GraphDiffInput<'a> {
    data: InputRows<'a>,
    rows: GqlExecutionStats,
    evaluator: GlaExecutionStats,
}
enum InputRows<'a> {
    Values(Vec<GraphValueRow>),
    Aggregates {
        rows: Vec<GraphAggregateRow>,
        slots: &'a [GraphAggregateTextSlot],
        key_width: usize,
        value_width: usize,
    },
}
impl<'a> GraphDiffInput<'a> {
    pub fn values(result: GqlQueryExecution<GraphValueRow>) -> Self {
        Self {
            data: InputRows::Values(result.value),
            rows: result.rows,
            evaluator: result.evaluator,
        }
    }
    pub fn aggregates(
        result: GqlQueryExecution<GraphAggregateRow>,
        slots: &'a [GraphAggregateTextSlot],
        key_width: usize,
        value_width: usize,
    ) -> Self {
        Self {
            data: InputRows::Aggregates {
                rows: result.value,
                slots,
                key_width,
                value_width,
            },
            rows: result.rows,
            evaluator: result.evaluator,
        }
    }
}

/// Canonically ordered signed NET occurrence changes: after minus before.
/// A changed value retracts its old complete row and inserts its new one.
/// Zero weights are absent. Count/Integer/Average/Value remain distinct domains;
/// equality uses full native values, not hashes. Pure order changes are not bag
/// changes, but each endpoint's ORDER BY/OFFSET/LIMIT is applied before diffing.
///
/// Endpoints name the host's admitted cuts, not a coverage proof or a durable
/// lease. Success means two complete input evaluations; any refused endpoint
/// aborts the whole operation. Intermediate writes that cancel are not events
/// in this result. No branch merge, CDC backlog, partial coverage or spill is
/// implied. Endpoint rows and delta construction are governed in-memory state.
#[derive(PartialEq, Eq)]
pub struct GraphResultDiff {
    before: CommitSeq,
    after: CommitSeq,
    columns: Box<[String]>,
    changes: ZSet<Row>,
    rows: GqlExecutionStats,
    evaluator: GlaExecutionStats,
}
impl GraphResultDiff {
    /// Move only the completed logical result out of the host's execution.
    /// Payloads, columns and exact weights are transferred, never cloned.
    /// Measured usage is discarded, not rewritten as fictitious zero usage.
    pub fn into_changes(self) -> GraphResultChanges {
        GraphResultChanges {
            before: self.before,
            after: self.after,
            columns: self.columns,
            changes: self.changes,
        }
    }

    pub fn before(&self) -> CommitSeq {
        self.before
    }
    pub fn after(&self) -> CommitSeq {
        self.after
    }
    pub fn columns(&self) -> &[String] {
        &self.columns
    }
    pub fn changes(&self) -> &ZSet<Row> {
        &self.changes
    }
    pub fn row_stats(&self) -> GqlExecutionStats {
        self.rows
    }
    pub fn evaluator_stats(&self) -> GlaExecutionStats {
        self.evaluator
    }

    /// Execute each endpoint once under the REMAINING common allowance. The
    /// host must retain one query definition, schema and admitted history for
    /// both calls. Report actual source usage in GraphDiffInput; this seam is
    /// not an authentication or untrusted-plugin boundary.
    ///
    /// Source records, work and scratch accumulate across both evaluations,
    /// row adaptation, exact consolidation and delivery. ResultRows counts
    /// consolidated CHANGED tuples, not endpoint rows or absolute weights.
    /// Private endpoint output has no public row cap; explicit query pages are
    /// still part of its definition. Equal cuts/empty answers do not skip source
    /// validation, errors or cancellation. No partial delta escapes on failure.
    ///
    /// Adapt and consolidate each occurrence before visiting the next one.
    /// Beyond the source's materialized endpoint, retain only the current net
    /// support and one adapted row, not a second bag of every occurrence from
    /// both endpoints. Source evaluation is still materialized and there is no
    /// spill or allocator-byte bound. Work/scratch admission stays cumulative,
    /// including copies of duplicate rows and entries removed by cancellation.
    /// Consolidation of Before is charged before After receives its allowance;
    /// an earlier consolidation refusal does not invoke the later source.
    pub fn execute<'a, E, C>(
        before: CommitSeq,
        after: CommitSeq,
        columns: Vec<String>,
        policy: GqlQueryPolicy,
        mut source: impl FnMut(
            DiffEndpoint,
            GqlQueryPolicy,
        ) -> Result<GraphDiffInput<'a>, GqlQueryError<E, C>>,
        mut checkpoint: impl FnMut() -> Result<(), C>,
    ) -> Result<Self, Error<E, C>> {
        let mut meter = Meter {
            policy,
            rows: GqlExecutionStats {
                snapshot_records: 0,
                result_rows: 0,
            },
            evaluator: GlaExecutionStats::default(),
            checkpoint: &mut checkpoint,
        };
        meter.charge(GlaExecutionEvent::Work)?;
        if columns.len() > MAX_PATTERN_VERTICES {
            return Err(GqlQueryError::Source(GraphDiffError::ColumnCount {
                observed: columns.len(),
            }));
        }
        meter.charge(GlaExecutionEvent::ScratchEntry)?;
        for name in &columns {
            for _ in 0..=name.len().div_ceil(64) {
                meter.charge(GlaExecutionEvent::ScratchEntry)?;
            }
        }
        // One private signed relation for the entire operation. Its insertion
        // order is exactly the former from_updates order: Before, then After.
        // Consolidating here releases duplicate/cancelled payloads immediately
        // and makes their actual work visible to the next source's admission.
        let mut changes = ZSet::new();
        for endpoint in [DiffEndpoint::Before, DiffEndpoint::After] {
            meter.charge(GlaExecutionEvent::Work)?;
            let input = source(endpoint, meter.remaining()).map_err(|error| {
                error.map_source(|source| GraphDiffError::Endpoint { endpoint, source })
            })?;
            meter.absorb(input.rows, input.evaluator)?;
            let sign = match endpoint {
                DiffEndpoint::Before => -1,
                DiffEndpoint::After => 1,
            };
            match input.data {
                InputRows::Values(rows) => {
                    for row in rows {
                        check_width(endpoint, columns.len(), row.len())?;
                        meter.charge(GlaExecutionEvent::ScratchEntry)?;
                        let mut cells = Vec::new();
                        for (column, value) in row.values().iter().enumerate() {
                            cells.push(copy_value(value, endpoint, column, &mut meter)?);
                        }
                        accumulate_row(
                            &mut changes,
                            cells.into_boxed_slice(),
                            sign,
                            &mut meter,
                        )?;
                    }
                }
                InputRows::Aggregates {
                    rows,
                    slots,
                    key_width,
                    value_width,
                } => {
                    check_width(endpoint, columns.len(), slots.len())?;
                    for slot in slots {
                        meter.charge(GlaExecutionEvent::Work)?;
                        let valid = match slot {
                            GraphAggregateTextSlot::GroupKey(at) => *at < key_width,
                            GraphAggregateTextSlot::Aggregate(at) => *at < value_width,
                        };
                        if !valid {
                            return Err(GqlQueryError::Source(GraphDiffError::AggregateLayout {
                                endpoint,
                            }));
                        }
                    }
                    for row in rows {
                        if row.keys().len() != key_width || row.values().len() != value_width {
                            return Err(GqlQueryError::Source(GraphDiffError::AggregateLayout {
                                endpoint,
                            }));
                        }
                        meter.charge(GlaExecutionEvent::ScratchEntry)?;
                        let mut cells = Vec::new();
                        for (column, slot) in slots.iter().enumerate() {
                            let cell = match *slot {
                                GraphAggregateTextSlot::GroupKey(at) => {
                                    copy_value(&row.keys()[at], endpoint, column, &mut meter)?
                                }
                                GraphAggregateTextSlot::Aggregate(at) => match &row.values()[at] {
                                    GraphAggregateValue::Value(value) => {
                                        copy_value(value, endpoint, column, &mut meter)?
                                    }
                                    value => {
                                        meter.charge(GlaExecutionEvent::ScratchEntry)?;
                                        value.clone()
                                    }
                                },
                            };
                            cells.push(cell);
                        }
                        accumulate_row(
                            &mut changes,
                            cells.into_boxed_slice(),
                            sign,
                            &mut meter,
                        )?;
                    }
                }
            }
        }
        // Only final support consumes the public row quota. Intermediate
        // retractions can all cancel even when that quota is zero. The private
        // relation is never released on a source, arithmetic or control error.
        let count = u64::try_from(changes.len())
            .map_err(|_| GqlQueryError::Source(GraphDiffError::Arithmetic))?;
        policy
            .rows
            .check(GqlBudgetDimension::ResultRows, count)
            .map_err(GqlQueryError::Rows)?;
        for _ in changes.iter() {
            meter.charge(GlaExecutionEvent::ResultRow)?;
        }
        meter.charge(GlaExecutionEvent::Work)?;
        meter.rows.result_rows = count;
        Ok(Self {
            before,
            after,
            columns: columns.into_boxed_slice(),
            changes,
            rows: meter.rows,
            evaluator: meter.evaluator,
        })
    }
}
impl core::fmt::Debug for GraphResultDiff {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("GraphResultDiff")
            .field("before", &self.before)
            .field("after", &self.after)
            .field("rows", &self.rows)
            .field("evaluator", &self.evaluator)
            .field("data", &"[REDACTED]")
            .finish()
    }
}

// Reuse the exact signed kernel, including its promoted-weight admission and
// entry accounting. There is no infallible arithmetic or alternate equality.
fn accumulate_row<E, C>(
    changes: &mut ZSet<Row>,
    row: Row,
    sign: i128,
    meter: &mut Meter<'_, C>,
) -> Result<(), Error<E, C>> {
    changes
        .accumulate(row, ZWeight::from_i128(sign), LimbLimit::new(4), &mut |event| {
            meter.charge::<E>(match event {
                ZSetEvent::Work => GlaExecutionEvent::Work,
                ZSetEvent::ScratchEntry => GlaExecutionEvent::ScratchEntry,
            })
        })
        .map_err(|error| match error {
            ZSetError::Control(error) | ZSetError::Callback(error) => error,
            _ => GqlQueryError::Source(GraphDiffError::Arithmetic),
        })
}

fn check_width<E, C>(
    endpoint: DiffEndpoint,
    expected: usize,
    observed: usize,
) -> Result<(), Error<E, C>> {
    if observed == expected {
        Ok(())
    } else {
        Err(GqlQueryError::Source(GraphDiffError::RowWidth {
            endpoint,
            expected,
            observed,
        }))
    }
}
fn copy_value<E, C>(
    value: &GraphValue,
    endpoint: DiffEndpoint,
    column: usize,
    meter: &mut Meter<'_, C>,
) -> Result<GraphAggregateValue, Error<E, C>> {
    meter.charge(GlaExecutionEvent::Work)?;
    if !value.validate_bounds() {
        return Err(GqlQueryError::Source(GraphDiffError::InvalidValue {
            endpoint,
            column,
        }));
    }
    Ok(GraphAggregateValue::Value(
        value.copy_with_control(&mut |event| meter.charge(event))?,
    ))
}

struct Meter<'a, C> {
    policy: GqlQueryPolicy,
    rows: GqlExecutionStats,
    evaluator: GlaExecutionStats,
    checkpoint: &'a mut dyn FnMut() -> Result<(), C>,
}
impl<C> Meter<'_, C> {
    fn charge<E>(&mut self, event: GlaExecutionEvent) -> Result<(), Error<E, C>> {
        (self.checkpoint)().map_err(GqlQueryError::Interrupted)?;
        self.evaluator
            .charge_event(self.policy.evaluator, event)
            .map_err(GqlQueryError::Evaluator)
    }
    fn remaining(&self) -> GqlQueryPolicy {
        GqlQueryPolicy {
            rows: GqlExecutionBudget::snapshot_records(
                self.policy
                    .rows
                    .max_snapshot_records()
                    .unwrap_or(u64::MAX)
                    .saturating_sub(self.rows.snapshot_records),
            ),
            evaluator: GlaExecutionLimits::new(
                self.policy
                    .evaluator
                    .max_work_units
                    .saturating_sub(self.evaluator.work_units),
                self.policy
                    .evaluator
                    .max_scratch_entries
                    .saturating_sub(self.evaluator.scratch_entries),
            ),
        }
    }
    fn absorb<E>(
        &mut self,
        rows: GqlExecutionStats,
        evaluator: GlaExecutionStats,
    ) -> Result<(), Error<E, C>> {
        (self.checkpoint)().map_err(GqlQueryError::Interrupted)?;
        let records = self
            .rows
            .snapshot_records
            .checked_add(rows.snapshot_records)
            .ok_or_else(|| GqlQueryError::Source(GraphDiffError::Arithmetic))?;
        let record_limit = self.policy.rows.max_snapshot_records().unwrap_or(u64::MAX);
        if records > record_limit {
            return Err(GqlQueryError::Rows(crate::GqlBudgetExceeded {
                dimension: GqlBudgetDimension::SnapshotRecords,
                limit: record_limit,
                observed: records,
            }));
        }
        let add = |old: u64, delta: u64, limit: u64, dimension| {
            let observed = u128::from(old) + u128::from(delta);
            if observed > u128::from(limit) {
                Err(GlaLimitExceeded {
                    dimension,
                    limit,
                    observed,
                })
            } else {
                Ok(observed as u64)
            }
        };
        let work_units = add(
            self.evaluator.work_units,
            evaluator.work_units,
            self.policy.evaluator.max_work_units,
            GlaLimitDimension::WorkUnits,
        )
        .map_err(GqlQueryError::Evaluator)?;
        let scratch_entries = add(
            self.evaluator.scratch_entries,
            evaluator.scratch_entries,
            self.policy.evaluator.max_scratch_entries,
            GlaLimitDimension::ScratchEntries,
        )
        .map_err(GqlQueryError::Evaluator)?;
        self.rows.snapshot_records = records;
        self.evaluator = GlaExecutionStats {
            work_units,
            scratch_entries,
        };
        Ok(())
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod online_tests;

#[cfg(test)]
mod release_tests {
    use super::*;

    fn result(work: u64) -> GraphResultDiff {
        GraphResultDiff {
            before: CommitSeq(1),
            after: CommitSeq(2),
            columns: vec!["private-name".to_owned()].into_boxed_slice(),
            changes: ZSet::from_updates(
                [(
                    vec![GraphAggregateValue::Integer(37)].into_boxed_slice(),
                    ZWeight::ONE,
                )],
                LimbLimit::new(4),
                &mut |_| Ok::<_, ()>(()),
            )
            .unwrap(),
            rows: GqlExecutionStats {
                snapshot_records: work,
                result_rows: 1,
            },
            evaluator: GlaExecutionStats {
                work_units: work,
                scratch_entries: work,
            },
        }
    }

    #[test]
    fn logical_release_moves_payloads_and_equality_cannot_observe_source_diagnostics() {
        let result = result(100);
        let column = result.columns.as_ptr();
        let row = result.changes.iter().next().unwrap().0.as_ptr();
        let released = result.into_changes();
        assert_eq!(released.columns().as_ptr(), column);
        assert_eq!(released.changes().iter().next().unwrap().0.as_ptr(), row);
        assert_eq!(released.before(), CommitSeq(1));
        assert_eq!(released.after(), CommitSeq(2));
        assert_eq!(released.columns(), ["private-name"]);
        let other = self::result(9999).into_changes();
        assert_eq!(released, other);
        let debug = format!("{released:?}");
        assert_eq!(debug, format!("{other:?}"));
        for private in [
            "private-name",
            "37",
            "snapshot_records",
            "work_units",
            "scratch_entries",
        ] {
            assert!(!debug.contains(private));
        }
    }
}
