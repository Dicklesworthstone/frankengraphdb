//! Composition of admission, output, evaluator limits and interruption.
//! This module is policy around its parent's one evaluator and one meter.

use super::{GlaExecutionEvent, GlaExecutionLimits, GlaExecutionStats, GlaLimitExceeded, charge};
use crate::algebra::{GlaPlan, VertexPredicate};
use crate::{GqlBudgetDimension, GqlBudgetExceeded, GqlExecutionBudget, GqlExecutionStats};
use fgdb_delta_types::RelationId;
use fgdb_types::VId;

/// Apply all four deterministic dimensions to one query, not separate runs.
/// Source materialization and allocator bytes are outside these shape limits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GqlQueryPolicy {
    pub rows: GqlExecutionBudget,
    pub evaluator: GlaExecutionLimits,
}

impl GqlQueryPolicy {
    #[must_use]
    pub const fn new(snapshot_records: u64, result_rows: u64, work_units: u64, scratch_entries: u64) -> Self {
        Self {
            rows: GqlExecutionBudget::new(snapshot_records, result_rows),
            evaluator: GlaExecutionLimits::new(work_units, scratch_entries),
        }
    }
}

/// Success counters describe the SAME execution as the returned rows.
#[derive(Clone, PartialEq, Eq)]
pub struct GqlQueryExecution {
    pub value: Vec<VId>,
    pub rows: GqlExecutionStats,
    pub evaluator: GlaExecutionStats,
}

impl core::fmt::Debug for GqlQueryExecution {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("GqlQueryExecution")
            .field("value", &"[REDACTED]")
            .field("rows", &self.rows)
            .field("evaluator", &self.evaluator)
            .finish()
    }
}

/// `C` is the original interruption error; it is never relabeled as exhaustion.
#[derive(Debug)]
pub enum GqlQueryError<E, C> {
    Source(E),
    Rows(GqlBudgetExceeded),
    Evaluator(GlaLimitExceeded),
    Interrupted(C),
}

impl<E, C> GqlQueryError<E, C> {
    pub fn map_source<T>(self, map: impl FnOnce(E) -> T) -> GqlQueryError<T, C> {
        match self {
            Self::Source(error) => GqlQueryError::Source(map(error)),
            Self::Rows(error) => GqlQueryError::Rows(error),
            Self::Evaluator(error) => GqlQueryError::Evaluator(error),
            Self::Interrupted(error) => GqlQueryError::Interrupted(error),
        }
    }
}

impl<E: core::fmt::Display, C: core::fmt::Display> core::fmt::Display for GqlQueryError<E, C> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Source(error) => core::fmt::Display::fmt(error, f),
            Self::Rows(error) => core::fmt::Display::fmt(error, f),
            Self::Evaluator(error) => core::fmt::Display::fmt(error, f),
            Self::Interrupted(error) => write!(f, "query interrupted: {error}"),
        }
    }
}

impl<E: core::error::Error + 'static, C: core::error::Error + 'static> core::error::Error for GqlQueryError<E, C> {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Source(error) => Some(error),
            Self::Rows(error) => Some(error),
            Self::Evaluator(error) => Some(error),
            Self::Interrupted(error) => Some(error),
        }
    }
}

impl GlaPlan {
    /// Execute an already admitted snapshot with one combined policy. The
    /// caller supplies its actual admitted count and original interruption
    /// error. All arithmetic for work/scratch delegates to the shared meter.
    /// Snapshot records are checked before index construction. Final rows are
    /// checked after distinct/order/pagination but BEFORE each output copy.
    /// No partial result, success statistics or certificate escapes a refusal.
    pub fn execute_governed<E, C>(
        &self,
        snapshot_records: u64,
        vertices: impl IntoIterator<Item = VId>,
        edges: impl IntoIterator<Item = (VId, RelationId, VId)>,
        mut test_vertex: impl FnMut(VId, &[VertexPredicate]) -> Result<bool, E>,
        policy: GqlQueryPolicy,
        mut checkpoint: impl FnMut() -> Result<(), C>,
    ) -> Result<GqlQueryExecution, GqlQueryError<E, C>> {
        checkpoint().map_err(GqlQueryError::Interrupted)?;
        policy.rows.check(GqlBudgetDimension::SnapshotRecords, snapshot_records)
            .map_err(GqlQueryError::Rows)?;
        let mut evaluator = GlaExecutionStats::default();
        let mut rows = GqlExecutionStats { snapshot_records, result_rows: 0 };
        let value = self.execute_with_control(
            vertices,
            edges,
            |vid, predicates| test_vertex(vid, predicates).map_err(GqlQueryError::Source),
            |event| {
                checkpoint().map_err(GqlQueryError::Interrupted)?;
                if event == GlaExecutionEvent::ResultRow {
                    // ResultRow is issued once for each member of an in-memory
                    // BTreeSet, so its count fits usize and therefore u64 on
                    // the supported targets. This is not an untrusted counter.
                    let next = rows.result_rows.checked_add(1)
                        .expect("an in-memory result cannot contain 2^64 VIds");
                    policy.rows.check(GqlBudgetDimension::ResultRows, next)
                        .map_err(GqlQueryError::Rows)?;
                    rows.result_rows = next;
                }
                charge(&mut evaluator, policy.evaluator, event).map_err(GqlQueryError::Evaluator)
            },
        )?;
        // Empty and zero-LIMIT results also observe a terminal checkpoint.
        checkpoint().map_err(GqlQueryError::Interrupted)?;
        debug_assert_eq!(u64::try_from(value.len()).ok(), Some(rows.result_rows));
        Ok(GqlQueryExecution { value, rows, evaluator })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RelationBind;
    use std::cell::Cell;

    fn plan(tail: &str) -> GlaPlan {
        GlaPlan::lower(&RelationBind::new().with_relation("R", RelationId(1))
            .bind(&format!("MATCH (a)-[:R]->(b) RETURN b{tail}")).unwrap())
    }

    fn edges() -> [(VId, RelationId, VId); 3] {
        [(VId(1), RelationId(1), VId(3)), (VId(1), RelationId(1), VId(2)),
            (VId(1), RelationId(1), VId(2))]
    }

    #[test]
    fn all_four_dimensions_are_enforced_in_one_execution() {
        let plan = plan("");
        let run = |policy| plan.execute_governed(3, [], edges(), |_, _| Ok::<_, ()>(true),
            policy, || Ok::<_, ()>(()));
        let wide = run(GqlQueryPolicy::new(3, 2, u64::MAX, u64::MAX)).unwrap();
        assert_eq!(wide.value, vec![VId(2), VId(3)]);
        assert_eq!(wide.rows, GqlExecutionStats { snapshot_records: 3, result_rows: 2 });
        let exact = GqlQueryPolicy::new(3, 2, wide.evaluator.work_units, wide.evaluator.scratch_entries);
        assert_eq!(run(exact).unwrap(), wide);
        for policy in [GqlQueryPolicy::new(2, 2, u64::MAX, u64::MAX),
            GqlQueryPolicy::new(3, 1, u64::MAX, u64::MAX)]
        {
            assert!(matches!(run(policy), Err(GqlQueryError::Rows(_))));
        }
        for policy in [GqlQueryPolicy::new(3, 2, exact.evaluator.max_work_units - 1, u64::MAX),
            GqlQueryPolicy::new(3, 2, u64::MAX, exact.evaluator.max_scratch_entries - 1)]
        {
            assert!(matches!(run(policy), Err(GqlQueryError::Evaluator(_))));
        }
    }

    #[test]
    fn rejected_admission_never_consumes_the_edge_iterator() {
        let consumed = Cell::new(0);
        let input = edges().into_iter().inspect(|_| consumed.set(consumed.get() + 1));
        let error = plan("").execute_governed(3, [], input, |_, _| Ok::<_, ()>(true),
            GqlQueryPolicy::new(2, 2, 100, 100), || Ok::<_, ()>(())).unwrap_err();
        assert!(matches!(error, GqlQueryError::Rows(GqlBudgetExceeded {
            dimension: GqlBudgetDimension::SnapshotRecords, observed: 3, limit: 2,
        })));
        assert_eq!(consumed.get(), 0);
    }

    #[test]
    fn result_events_count_final_rows_and_can_refuse_the_copy_tail() {
        let returned = Cell::new(0);
        let error = plan("").execute_with_control([], edges(), |_, _| Ok(true), |event| {
            if event == GlaExecutionEvent::ResultRow {
                returned.set(returned.get() + 1);
                if returned.get() == 2 { return Err("cancel at final row copy"); }
            }
            Ok(())
        }).unwrap_err();
        assert_eq!(error, "cancel at final row copy");
        assert_eq!(returned.get(), 2);
        let paged = plan(" SKIP 1 LIMIT 1").execute_governed(3, [], edges(),
            |_, _| Ok::<_, ()>(true), GqlQueryPolicy::new(3, 1, 100, 100),
            || Ok::<_, ()>(())).unwrap();
        assert_eq!(paged.value, vec![VId(3)]);
        assert_eq!(paged.rows.result_rows, 1);
    }

    #[test]
    fn interruption_at_every_checkpoint_is_terminal_even_for_empty_results() {
        for tail in ["", " SKIP 99"] {
            let logical = plan(tail);
            let calls = Cell::new(0);
            logical.execute_governed(3, [], edges(), |_, _| Ok::<_, ()>(true),
                GqlQueryPolicy::new(3, 2, 100, 100), || {
                    calls.set(calls.get() + 1); Ok::<_, usize>(())
                }).unwrap();
            for stop in 1..=calls.get() {
                let at = Cell::new(0);
                let result = logical.execute_governed(3, [], edges(), |_, _| Ok::<_, ()>(true),
                    GqlQueryPolicy::new(3, 2, 100, 100), || {
                        at.set(at.get() + 1);
                        if at.get() == stop { Err(stop) } else { Ok(()) }
                    });
                assert!(matches!(result, Err(GqlQueryError::Interrupted(value)) if value == stop));
                assert_eq!(at.get(), stop);
            }
        }
    }
}
