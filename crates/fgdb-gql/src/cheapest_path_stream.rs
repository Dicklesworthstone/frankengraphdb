//! Pull delivery over the native ranked PathFind, with one cumulative meter.
//! The graph is admitted once; result pages are never precomputed or retained.

use crate::{
    GlaExecutionEvent, GlaExecutionStats, GlaLimitDimension, GlaLimitExceeded,
    GqlBudgetDimension, GqlExecutionStats, GqlQueryError, GqlQueryPolicy,
    GraphCheapestPathCursor, GraphCheapestPathError, GraphCostPath,
    PreparedGraphCheapestPath,
};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_types::{CanonicalScalar, EId, VId};
use std::convert::Infallible;

type StreamResult<T, E, C> = Result<T, GqlQueryError<GraphCheapestPathError<E>, C>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphCheapestPathStreamState {
    Open,
    Exhausted,
    Closed,
    Failed,
}

/// A single-owner, cumulative-budget stream of at most K ranked paths.
///
/// Source validation, owned topology, suffix construction, all later path
/// refinements and output copies share ONE allowance. The caller may pause
/// between pulls or collect pages of any size; neither operation resets it.
/// This uses the native path cursor, not repeated execution with larger LIMITs.
///
/// Each successful pull delivers one complete path. A later refusal preserves
/// earlier deliveries, returns one error (never EOF), and drops all search
/// state. A checkpoint immediately before delivery can still refuse the row;
/// that work is charged, but ResultRows counts only rows actually delivered.
/// Reaching K closes immediately without expanding the final answer. Explicit
/// close and drop also discard the unexplored suffix without driving it.
///
/// Opening admits the complete selected source and builds the native suffix
/// index, including for K = 0. Source fields are then no longer borrowed: this
/// stream owns its topology and integer costs. It does not provide disk spill,
/// a byte-memory bound, a durable restart token, or authorization/session leases.
/// The host must pin an admitted source and retain its purpose context for pulls.
pub struct GraphCheapestPathStream {
    cursor: GraphCheapestPathCursor,
    policy: GqlQueryPolicy,
    rows: GqlExecutionStats,
    evaluator: GlaExecutionStats,
    count: u64,
    state: GraphCheapestPathStreamState,
}

impl PreparedGraphCheapestPath {
    /// Open a ranked stream over an already admitted immutable source.
    /// SnapshotRecords has the ordinary complete-source meaning, not a page
    /// size. Source property and domain failures are reported before any row.
    #[allow(clippy::too_many_arguments)]
    pub fn stream_governed_with_edge_properties<'a, E, C>(
        &self,
        count: u64,
        snapshot_records: u64,
        vertices: impl IntoIterator<Item = VId>,
        edges: impl IntoIterator<Item = (EId, VId, RelationId, VId)>,
        property: impl FnMut(EId, PropertyKeyId) -> Result<Option<&'a CanonicalScalar>, E>,
        policy: GqlQueryPolicy,
        checkpoint: impl FnMut() -> Result<(), C>,
    ) -> StreamResult<GraphCheapestPathStream, E, C> {
        self.stream_governed_with_admission(
            count, snapshot_records, vertices, edges, property,
            GlaExecutionStats::default(), policy, checkpoint,
        )
    }

    /// Host composition seam: charge source admission and every future pull
    /// against the original policy. `admission` is the host's already incurred
    /// logical work/scratch, not a second allowance or an authorization proof.
    /// The returned counters and refusal limits include this prior work.
    /// Impossible prior usage is rejected before consulting any source field.
    #[allow(clippy::too_many_arguments)]
    pub fn stream_governed_with_admission<'a, E, C>(
        &self,
        count: u64,
        snapshot_records: u64,
        vertices: impl IntoIterator<Item = VId>,
        edges: impl IntoIterator<Item = (EId, VId, RelationId, VId)>,
        mut property: impl FnMut(EId, PropertyKeyId) -> Result<Option<&'a CanonicalScalar>, E>,
        admission: GlaExecutionStats,
        policy: GqlQueryPolicy,
        mut checkpoint: impl FnMut() -> Result<(), C>,
    ) -> StreamResult<GraphCheapestPathStream, E, C> {
        checkpoint().map_err(GqlQueryError::Interrupted)?;
        policy.rows.check(GqlBudgetDimension::SnapshotRecords, snapshot_records)
            .map_err(GqlQueryError::Rows)?;
        for (dimension, observed, limit) in [
            (GlaLimitDimension::WorkUnits, admission.work_units, policy.evaluator.max_work_units),
            (GlaLimitDimension::ScratchEntries, admission.scratch_entries, policy.evaluator.max_scratch_entries),
        ] {
            if observed > limit {
                return Err(GqlQueryError::Evaluator(GlaLimitExceeded {
                    dimension, limit, observed: u128::from(observed),
                }));
            }
        }
        let mut evaluator = admission;
        let mut cursor = flatten(self.cursor_with_control(
            vertices, edges,
            |edge, key| property(edge, key)
                .map_err(|error| GqlQueryError::Source(GraphCheapestPathError::Source(error))),
            |event| {
                checkpoint().map_err(GqlQueryError::Interrupted)?;
                evaluator.charge_event(policy.evaluator, event).map_err(GqlQueryError::Evaluator)
            },
        ))?;
        checkpoint().map_err(GqlQueryError::Interrupted)?;
        let state = if count == 0 || cursor.is_exhausted() {
            cursor.close();
            GraphCheapestPathStreamState::Exhausted
        } else {
            GraphCheapestPathStreamState::Open
        };
        Ok(GraphCheapestPathStream {
            cursor, policy, rows: GqlExecutionStats { snapshot_records, result_rows: 0 },
            evaluator, count, state,
        })
    }
}

impl GraphCheapestPathStream {
    #[must_use]
    pub fn state(&self) -> GraphCheapestPathStreamState { self.state }
    #[must_use]
    pub fn row_stats(&self) -> GqlExecutionStats { self.rows }
    #[must_use]
    pub fn evaluator_stats(&self) -> GlaExecutionStats { self.evaluator }

    /// Closing is idempotent. A completed/failed outcome is never relabeled,
    /// and its counters remain available. No source or search work is driven.
    pub fn close(&mut self) {
        if self.state == GraphCheapestPathStreamState::Open {
            self.state = GraphCheapestPathStreamState::Closed;
        }
        self.cursor.close();
    }

    /// Pull one complete row under the same cumulative allowance. The source
    /// is owned after opening, so only cost, resource and interruption errors
    /// remain possible; an Infallible source is not a missing-row sentinel.
    /// A terminal stream never calls the supplied checkpoint again.
    pub fn next_with_checkpoint<C>(
        &mut self,
        mut checkpoint: impl FnMut() -> Result<(), C>,
    ) -> StreamResult<Option<GraphCostPath>, Infallible, C> {
        if self.state != GraphCheapestPathStreamState::Open { return Ok(None); }
        // Open implies delivered < count <= u64::MAX. No unbounded row counter
        // or usize narrowing occurs, even for the largest legal K.
        let next_count = self.rows.result_rows.checked_add(1).expect("open bounded K stream");
        let policy = self.policy;
        let evaluator = &mut self.evaluator;
        let result = (|| {
            checkpoint().map_err(GqlQueryError::Interrupted)?;
            let row = flatten(self.cursor.next_with_control(|event| {
                checkpoint().map_err(GqlQueryError::Interrupted)?;
                if event == GlaExecutionEvent::ResultRow {
                    policy.rows.check(GqlBudgetDimension::ResultRows, next_count)
                        .map_err(GqlQueryError::Rows)?;
                }
                evaluator.charge_event(policy.evaluator, event).map_err(GqlQueryError::Evaluator)
            }))?;
            // This also checks natural EOF; cancellation must not masquerade
            // as exhaustion after a potentially expensive final refinement.
            checkpoint().map_err(GqlQueryError::Interrupted)?;
            Ok(row)
        })();
        match result {
            Ok(Some(row)) => {
                self.rows.result_rows = next_count;
                if next_count == self.count {
                    self.state = GraphCheapestPathStreamState::Exhausted;
                    self.cursor.close();
                }
                Ok(Some(row))
            }
            Ok(None) => {
                self.state = GraphCheapestPathStreamState::Exhausted;
                self.cursor.close();
                Ok(None)
            }
            Err(error) => {
                self.state = GraphCheapestPathStreamState::Failed;
                self.cursor.close();
                Err(error)
            }
        }
    }
}

// The low-level cursor has one generic control error. Preserve its distinction
// from a path-cost domain failure; never turn cancellation into a source error.
fn flatten<T, E, C>(
    result: Result<T, GraphCheapestPathError<GqlQueryError<GraphCheapestPathError<E>, C>>>,
) -> StreamResult<T, E, C> {
    result.map_err(|error| match error {
        GraphCheapestPathError::Source(error) => error,
        GraphCheapestPathError::Cost(error) => GqlQueryError::Source(GraphCheapestPathError::Cost(error)),
    })
}

impl core::fmt::Debug for GraphCheapestPathStream {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("GraphCheapestPathStream")
            .field("state", &self.state)
            .field("rows", &self.rows)
            .field("evaluator", &self.evaluator)
            .field("source_and_definition", &"[REDACTED]")
            .finish()
    }
}

#[cfg(test)]
mod tests;
