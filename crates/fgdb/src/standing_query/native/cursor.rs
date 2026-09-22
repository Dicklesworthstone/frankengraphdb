//! Pull delivery borrows one accepted maintained generation, never a graph.
//! Runs remain compressed; a pull copies at most one complete native row.

use super::*;
use fgdb_gql::stream::VertexScanState;
use fgdb_gql::{GlaExecutionStats, GqlExecutionStats};

mod delta;
pub use delta::StandingNativeDeltaCursor;
pub(super) use delta::open as open_delta;

#[derive(Clone, Copy)]
enum NativeRow<'a> {
    Values(&'a GraphValueRow),
    Group(&'a GraphAggregateRow),
}

#[derive(Clone, Copy)]
struct Run<'a> {
    row: NativeRow<'a>,
    // None is one explicitly ordered occurrence, not missing support.
    weight: Option<&'a ZWeight>,
}

type Runs<'a> = Box<dyn Iterator<Item = Run<'a>> + 'a>;

struct Pending<'a> {
    run: Run<'a>,
    emitted: u64,
}

struct Pull<'a> {
    runs: Option<Runs<'a>>,
    pending: Option<Pending<'a>>,
    layout: Arc<Layout>,
    frontier: CommitSeq,
    policy: GqlQueryPolicy,
    stats: StandingQueryStats,
    delivered: u64,
    state: VertexScanState,
}

/// Backpressured native delivery from one accepted standing-query generation.
/// Every item is one complete row in the same lossless cells and order as
/// `standing_native_query`: counts, wide sums, rational averages and identities
/// are never narrowed. Ranked runs and ALL multiplicities expand only as pulled.
/// Opening reads no rows; neither opening nor pulling executes graph queries.
///
/// The cursor borrows the database and QueryCx, but owns shared presentation
/// metadata, so the caller may drop its handle. This is NOT a detached snapshot:
/// the immutable borrow prevents writes/rebuilds while the cursor remains in use.
/// Drop releases that borrow; close releases traversal state without draining.
/// No result table, sorted copy, resume token, backlog or durable lease is built.
///
/// ResultRows counts delivered occurrences. Work/scratch are cumulative across
/// opening and every pull; SnapshotRecords is unused. A request past an exact
/// result allowance returns one error unless the input is exhausted. Earlier
/// complete rows remain delivered; only exhaustion certifies complete delivery.
/// Error permanently fuses this cursor and never fences the maintained view.
/// These are logical/payload event allowances, not allocator-byte limits.
pub struct StandingNativeCursor<'a> {
    pull: Pull<'a>,
    cx: &'a QueryCx,
}

impl StandingNativeCursor<'_> {
    #[must_use]
    pub fn columns(&self) -> &[String] {
        self.pull.layout.columns()
    }

    #[must_use]
    pub fn snapshot_seq(&self) -> CommitSeq {
        self.pull.frontier
    }

    /// Uses the same four lifecycle states as the native scan cursors; this
    /// describes delivery only, not a vertex-source specialization.
    #[must_use]
    pub fn state(&self) -> VertexScanState {
        self.pull.state
    }

    #[must_use]
    pub fn row_stats(&self) -> GqlExecutionStats {
        GqlExecutionStats {
            snapshot_records: 0,
            result_rows: self.pull.delivered,
        }
    }

    #[must_use]
    pub fn evaluator_stats(&self) -> GlaExecutionStats {
        GlaExecutionStats {
            work_units: self.pull.stats.work_units,
            scratch_entries: self.pull.stats.scratch_entries,
        }
    }

    /// Never pulls, projects, charges, or invokes a checkpoint. An exhausted or
    /// failed cursor keeps its terminal state, including across repeated closes.
    pub fn close(&mut self) {
        self.pull.close();
    }
}

impl Iterator for StandingNativeCursor<'_> {
    type Item = Result<Vec<QueryValue>, StandingQueryError>;

    fn next(&mut self) -> Option<Self::Item> {
        let cx = self.cx;
        cx.with_restriction(|| {
            self.pull
                .next_checked(&mut || {
                    cx.checkpoint().map_err(|_| StandingQueryFailure::Interrupted)
                })
                .map(|row| row.map_err(StandingQueryError::Delivery))
        })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        // Errors occupy an item too. Never expand a run or promise an exact
        // remaining length from the (possibly promoted) multiplicity domain.
        if self.pull.state == VertexScanState::Open {
            (0, None)
        } else {
            (0, Some(0))
        }
    }
}

impl core::iter::FusedIterator for StandingNativeCursor<'_> {}

impl core::fmt::Debug for StandingNativeCursor<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("StandingNativeCursor")
            .field("frontier", &self.pull.frontier)
            .field("columns", &self.columns().len())
            .field("delivered", &self.pull.delivered)
            .field("state", &self.pull.state)
            .field("data", &"[REDACTED]")
            .finish()
    }
}

impl<'a> Pull<'a> {
    fn new(
        runs: Runs<'a>,
        layout: Arc<Layout>,
        frontier: CommitSeq,
        policy: GqlQueryPolicy,
        stats: StandingQueryStats,
    ) -> Self {
        Self {
            runs: Some(runs),
            pending: None,
            layout,
            frontier,
            policy,
            stats,
            delivered: 0,
            state: VertexScanState::Open,
        }
    }

    fn close(&mut self) {
        self.runs = None;
        self.pending = None;
        if self.state == VertexScanState::Open {
            self.state = VertexScanState::Closed;
        }
    }

    fn next_checked(
        &mut self,
        checkpoint: &mut impl FnMut() -> Result<(), StandingQueryFailure>,
    ) -> Option<Result<Vec<QueryValue>, StandingQueryFailure>> {
        self.advance(checkpoint, pull_row)
    }

    // Occurrence and compressed-delta cursors share the same publication and
    // failure lifecycle. The step admits one complete item before returning it.
    fn advance<T>(
        &mut self,
        checkpoint: &mut impl FnMut() -> Result<(), StandingQueryFailure>,
        step: impl FnOnce(
            &mut Runs<'a>,
            &mut Option<Pending<'a>>,
            &Layout,
            u64,
            &mut Meter<'_>,
        ) -> Result<Option<T>, StandingQueryFailure>,
    ) -> Option<Result<T, StandingQueryFailure>> {
        if self.state != VertexScanState::Open {
            return None;
        }
        // Put all traversal state in local ownership before any fallible work.
        // An unwinding checkpoint drops it too and leaves the cursor fused;
        // restoring Open is the last, infallible step after a complete row.
        self.state = VertexScanState::Failed;
        let mut runs = self.runs.take().expect("open delivery owns its iterator");
        let mut pending = self.pending.take();
        let mut meter = Meter {
            policy: self.policy,
            stats: self.stats,
            checkpoint,
        };
        let result = step(&mut runs, &mut pending, &self.layout, self.delivered, &mut meter);
        self.stats = meter.stats;
        match result {
            Err(error) => Some(Err(error)),
            Ok(None) => {
                self.state = VertexScanState::Exhausted;
                None
            }
            Ok(Some(row)) => {
                // The step checked the successor before constructing the item.
                self.delivered += 1;
                self.runs = Some(runs);
                self.pending = pending;
                self.state = VertexScanState::Open;
                Some(Ok(row))
            }
        }
    }
}

fn pull_row<'a>(
    runs: &mut Runs<'a>,
    pending: &mut Option<Pending<'a>>,
    layout: &Layout,
    delivered: u64,
    meter: &mut Meter<'_>,
) -> Result<Option<Vec<QueryValue>>, StandingQueryFailure> {
    meter.charge(ZSetEvent::Work)?;
    if pending.is_none() {
        let Some(run) = runs.next() else {
            // Empty and exact-limit results still have an interruptible EOF.
            (meter.checkpoint)()?;
            return Ok(None);
        };
        meter.charge(ZSetEvent::Work)?;
        if run.weight.is_some_and(|weight| weight <= &ZWeight::ZERO) {
            return Err(StandingQueryFailure::InvalidDelta);
        }
        *pending = Some(Pending { run, emitted: 0 });
    }
    let next = delivered
        .checked_add(1)
        .ok_or(StandingQueryFailure::ResultBudget)?;
    if meter.policy.rows.max_result_rows().is_some_and(|limit| next > limit) {
        return Err(StandingQueryFailure::ResultBudget);
    }
    meter.charge(ZSetEvent::ScratchEntry)?;
    let current = pending.as_mut().expect("a pending occurrence was admitted");
    let cells = match current.run.row {
        NativeRow::Values(row) => copy_values(row, meter)?,
        NativeRow::Group(row) => {
            let slots = match layout {
                Layout::Aggregate { slots, .. } | Layout::GroupCircuit { slots, .. } => slots,
                _ => return Err(StandingQueryFailure::InvalidDelta),
            };
            copy_group(row, slots, meter)?
        }
    };
    if cells.len() != layout.columns().len() {
        return Err(StandingQueryFailure::InvalidDelta);
    }
    meter.charge(ZSetEvent::Work)?;
    let emitted = current
        .emitted
        .checked_add(1)
        .ok_or(StandingQueryFailure::ResultBudget)?;
    // Compare borrowed exact support to a small emitted counter. Do not narrow
    // the run to u64/i128, copy a bigint, or subtract/expand its whole weight.
    let exhausted = current.run.weight.is_none_or(|weight| {
        weight == &ZWeight::from_i128(i128::from(emitted))
    });
    (meter.checkpoint)()?;
    if exhausted {
        *pending = None;
    } else {
        current.emitted = emitted;
    }
    Ok(Some(cells))
}

fn view_runs<'a, Row: Ord + 'a>(
    rows: &'a ZSet<Row>,
    ordered: Option<&'a [Arc<Row>]>,
    wrap: fn(&'a Row) -> NativeRow<'a>,
) -> Runs<'a> {
    match ordered {
        Some(sequence) => Box::new(sequence.iter().map(move |row| Run {
            row: wrap(row.as_ref()),
            weight: None,
        })),
        None => Box::new(rows.iter().map(move |(row, weight)| Run {
            row: wrap(row),
            weight: Some(weight),
        })),
    }
}

pub(super) fn open<'a, V: Vfs + Clone>(
    database: &'a Database<V>,
    cx: &'a QueryCx,
    handle: &StandingQueryHandle,
    policy: GqlQueryPolicy,
) -> Result<StandingNativeCursor<'a>, StandingQueryError> {
    let root = database.admitted_standing_query(cx, handle)?;
    let layout = handle.native.as_ref().ok_or(StandingQueryError::Unsupported)?;
    cx.with_restriction(|| {
        let mut checkpoint = || cx.checkpoint().map_err(|_| StandingQueryFailure::Interrupted);
        let mut meter = Meter {
            policy,
            stats: StandingQueryStats::default(),
            checkpoint: &mut checkpoint,
        };
        meter.charge(ZSetEvent::Work).map_err(StandingQueryError::Delivery)?;
        // One fixed-size iterator box; no column, row or support traversal.
        meter.charge(ZSetEvent::ScratchEntry).map_err(StandingQueryError::Delivery)?;
        let runs: Runs<'a> = if let StandingQuery::Window(query) = root {
            Box::new(query.ordered().map(|(row, weight)| Run {
                row: NativeRow::Values(row),
                weight: Some(weight),
            }))
        } else {
            match layout.as_ref() {
                Layout::Rows { .. } | Layout::Circuit { .. } => {
                    let mut view = match root {
                        StandingQuery::Rows { .. } | StandingQuery::Constant(_) => {
                            database.standing_rows(cx, handle)?
                        }
                        StandingQuery::Set(_) => database.standing_set(cx, handle)?,
                        StandingQuery::Join(_) => database.standing_join(cx, handle)?,
                        StandingQuery::Projection(_) => database.standing_projection(cx, handle)?,
                        _ => return Err(StandingQueryError::Unsupported),
                    };
                    if matches!(layout.as_ref(), Layout::Circuit { .. })
                        && !matches!(root, StandingQuery::Constant(_))
                    {
                        view.ordered = None;
                    }
                    view_runs(view.rows, view.ordered, NativeRow::Values)
                }
                Layout::Aggregate { .. } | Layout::GroupCircuit { .. } => {
                    let view = database.standing_query(cx, handle)?;
                    view_runs(view.rows, view.ordered, NativeRow::Group)
                }
            }
        };
        (meter.checkpoint)().map_err(StandingQueryError::Delivery)?;
        Ok(StandingNativeCursor {
            pull: Pull::new(runs, Arc::clone(layout), root.status().1, policy, meter.stats),
            cx,
        })
    })
}

#[cfg(test)]
mod tests;
