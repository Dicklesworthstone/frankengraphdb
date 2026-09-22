//! Compressed one-tick native delivery. This is not a replay log or an ACK.

use super::*;
use fgdb_delta_types::LimbLimit;

/// A signed, compressed native result derivative for exactly `(after, frontier]`.
/// One item is `(native_cells, exact_signed_multiplicity)`, never an expanded
/// deletion/insertion stream. ResultRows counts changed frames. Output frames
/// follow the maintained delta's key order; repeated native projections can
/// collide, so consumers must integrate their weights rather than assume unique
/// projected keys. Pure changes in sequence order do not constitute bag deltas.
///
/// The database borrow pins the accepted tick, while shared metadata permits
/// dropping the caller's handle. The cursor has its own cumulative delivery
/// allowance; all payload and weight copies precede an interruptible release.
/// Opening examines no rows, and pulling never reexecutes expressions or reads
/// graph history. Failure emits one error and fuses without fencing the view.
///
/// Stage the whole transition before publishing it locally. Exhaustion, not
/// the last successful item or early close, proves complete delivery. There is
/// no ACK, buffering of missed ticks, durable cursor, rank edit, or resumption.
/// After a baseline/gap, obtain the current snapshot and start from its frontier.
pub struct StandingNativeDeltaCursor<'a> {
    pull: Pull<'a>,
    cx: &'a QueryCx,
    after: CommitSeq,
}

impl StandingNativeDeltaCursor<'_> {
    #[must_use]
    pub fn columns(&self) -> &[String] {
        self.pull.layout.columns()
    }

    #[must_use]
    pub fn after_seq(&self) -> CommitSeq {
        self.after
    }

    #[must_use]
    pub fn snapshot_seq(&self) -> CommitSeq {
        self.pull.frontier
    }

    #[must_use]
    pub fn state(&self) -> VertexScanState {
        self.pull.state
    }

    /// Result rows are complete changed frames, not the sum of signed weights
    /// and not their absolute expanded occurrence count. No graph visits occur.
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

    /// Discard traversal without pulling, allocating, or invoking checkpoints.
    /// Early close is not successful transition delivery or acknowledgement.
    pub fn close(&mut self) {
        self.pull.close();
    }
}

impl Iterator for StandingNativeDeltaCursor<'_> {
    type Item = Result<(Vec<QueryValue>, ZWeight), StandingQueryError>;

    fn next(&mut self) -> Option<Self::Item> {
        let cx = self.cx;
        cx.with_restriction(|| {
            self.pull
                .advance(
                    &mut || {
                        cx.checkpoint()
                            .map_err(|_| StandingQueryFailure::Interrupted)
                    },
                    pull_change,
                )
                .map(|change| change.map_err(StandingQueryError::Delivery))
        })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        if self.pull.state == VertexScanState::Open {
            (0, None)
        } else {
            (0, Some(0))
        }
    }
}

impl core::iter::FusedIterator for StandingNativeDeltaCursor<'_> {}

impl core::fmt::Debug for StandingNativeDeltaCursor<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("StandingNativeDeltaCursor")
            .field("after", &self.after)
            .field("frontier", &self.pull.frontier)
            .field("columns", &self.columns().len())
            .field("delivered_frames", &self.pull.delivered)
            .field("state", &self.pull.state)
            .field("data", &"[REDACTED]")
            .finish()
    }
}

fn pull_change<'a>(
    runs: &mut Runs<'a>,
    pending: &mut Option<Pending<'a>>,
    layout: &Layout,
    delivered: u64,
    meter: &mut Meter<'_>,
) -> Result<Option<(Vec<QueryValue>, ZWeight)>, StandingQueryFailure> {
    meter.charge(ZSetEvent::Work)?;
    if pending.is_some() {
        return Err(StandingQueryFailure::InvalidDelta);
    }
    let Some(run) = runs.next() else {
        (meter.checkpoint)()?;
        return Ok(None);
    };
    meter.charge(ZSetEvent::Work)?;
    let weight = run.weight.ok_or(StandingQueryFailure::InvalidDelta)?;
    if weight.is_zero() {
        return Err(StandingQueryFailure::InvalidDelta);
    }
    let next = delivered
        .checked_add(1)
        .ok_or(StandingQueryFailure::ResultBudget)?;
    if meter
        .policy
        .rows
        .max_result_rows()
        .is_some_and(|limit| next > limit)
    {
        return Err(StandingQueryFailure::ResultBudget);
    }
    // Retained operators use this same bounded exact-weight domain. Reserve a
    // whole frame and weight copy, then let checked_clone enforce limb admission.
    // A large magnitude does not spend a row allowance per occurrence.
    meter.charge(ZSetEvent::ScratchEntry)?;
    meter.charge(ZSetEvent::Work)?;
    meter.charge(ZSetEvent::ScratchEntry)?;
    let weight = weight
        .checked_clone(LimbLimit::new(4))
        .map_err(|_| StandingQueryFailure::Arithmetic)?;
    let cells = match run.row {
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
    (meter.checkpoint)()?;
    Ok(Some((cells, weight)))
}

fn require_predecessor(after: CommitSeq, frontier: CommitSeq) -> Result<(), StandingQueryError> {
    if after.checked_successor().ok() != Some(frontier) {
        return Err(StandingQueryError::DeltaGap { after, frontier });
    }
    Ok(())
}

pub(in crate::standing_query::native) fn open<'a, V: Vfs + Clone>(
    database: &'a Database<V>,
    cx: &'a QueryCx,
    handle: &StandingQueryHandle,
    after: CommitSeq,
    policy: GqlQueryPolicy,
) -> Result<Option<StandingNativeDeltaCursor<'a>>, StandingQueryError> {
    let root = database.admitted_standing_query(cx, handle)?;
    let layout = handle
        .native
        .as_ref()
        .ok_or(StandingQueryError::Unsupported)?;
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
            .map_err(StandingQueryError::Delivery)?;
        let frontier = root.status().1;
        // Do not turn an unsupported producer into a baseline. Only these
        // complete-result owners retain the final derivative for their output.
        enum Changes<'a> {
            Rows(&'a ZSet<GraphValueRow>),
            Groups(&'a ZSet<GraphAggregateRow>),
        }
        let changes = match (layout.as_ref(), root) {
            (
                Layout::Rows { .. } | Layout::Circuit { .. },
                StandingQuery::Rows { .. }
                | StandingQuery::Constant(_)
                | StandingQuery::Set(_)
                | StandingQuery::Join(_)
                | StandingQuery::Projection(_)
                | StandingQuery::Filter(_)
                | StandingQuery::Window(_),
            ) => sets::delta(root).map(Changes::Rows),
            (Layout::Aggregate { .. } | Layout::GroupCircuit { .. }, StandingQuery::Group(_)) => {
                database
                    .standing_group_delta(cx, handle)?
                    .map(|view| Changes::Groups(view.rows))
            }
            _ => return Err(StandingQueryError::Unsupported),
        };
        let Some(changes) = changes else {
            (meter.checkpoint)().map_err(StandingQueryError::Delivery)?;
            return Ok(None);
        };
        // Even an empty derivative represents a specific accepted successor.
        // Never let emptiness erase a gap, reversal, or exhausted sequence.
        require_predecessor(after, frontier)?;
        meter
            .charge(ZSetEvent::ScratchEntry)
            .map_err(StandingQueryError::Delivery)?;
        let runs = match changes {
            Changes::Rows(rows) => view_runs(rows, None, NativeRow::Values),
            Changes::Groups(rows) => view_runs(rows, None, NativeRow::Group),
        };
        (meter.checkpoint)().map_err(StandingQueryError::Delivery)?;
        Ok(Some(StandingNativeDeltaCursor {
            pull: Pull::new(runs, Arc::clone(layout), frontier, policy, meter.stats),
            cx,
            after,
        }))
    })
}

#[cfg(test)]
mod tests;
