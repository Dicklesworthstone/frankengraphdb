//! Composition of admission, output, evaluator limits and interruption.
//! All sealed row shapes use the same evaluator and policy counters.

use super::{GlaExecutionEvent, GlaExecutionLimits, GlaExecutionStats, GlaLimitExceeded, charge};
use crate::algebra::{GlaIdentityOutput, GlaOutput, GlaPlan, VertexPredicate};
use crate::{
    BudgetedGqlError, BudgetedGqlExecution, GqlBudgetDimension, GqlBudgetExceeded,
    GqlExecutionBudget, GqlExecutionStats,
};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_types::{CanonicalScalar, VId};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GqlQueryPolicy {
    pub rows: GqlExecutionBudget,
    pub evaluator: GlaExecutionLimits,
}

impl GqlQueryPolicy {
    #[must_use]
    pub const fn new(
        snapshot_records: u64,
        result_rows: u64,
        work_units: u64,
        scratch_entries: u64,
    ) -> Self {
        Self {
            rows: GqlExecutionBudget::new(snapshot_records, result_rows),
            evaluator: GlaExecutionLimits::new(work_units, scratch_entries),
        }
    }
}

/// Counters describe this one execution. Row limits count complete projected
/// rows, not cells; tuple cells additionally consume evaluator scratch/work.
#[derive(Clone, PartialEq, Eq)]
pub struct GqlQueryExecution<Row = VId> {
    pub value: Vec<Row>,
    pub rows: GqlExecutionStats,
    pub evaluator: GlaExecutionStats,
}

impl<Row> core::fmt::Debug for GqlQueryExecution<Row> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("GqlQueryExecution")
            .field("value", &"[REDACTED]")
            .field("rows", &self.rows)
            .field("evaluator", &self.evaluator)
            .finish()
    }
}

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
impl<E: core::error::Error + 'static, C: core::error::Error + 'static> core::error::Error
    for GqlQueryError<E, C>
{
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Source(error) => Some(error),
            Self::Rows(error) => Some(error),
            Self::Evaluator(error) => Some(error),
            Self::Interrupted(error) => Some(error),
        }
    }
}

struct QueryMeter<Checkpoint> {
    checkpoint: Checkpoint,
    policy: GqlQueryPolicy,
    rows: GqlExecutionStats,
    evaluator: GlaExecutionStats,
}

impl<Checkpoint> QueryMeter<Checkpoint> {
    fn observe<E, C>(&mut self, event: GlaExecutionEvent) -> Result<(), GqlQueryError<E, C>>
    where
        Checkpoint: FnMut() -> Result<(), C>,
    {
        (self.checkpoint)().map_err(GqlQueryError::Interrupted)?;
        charge_result_row(&mut self.rows, self.policy.rows, event).map_err(GqlQueryError::Rows)?;
        charge(&mut self.evaluator, self.policy.evaluator, event).map_err(GqlQueryError::Evaluator)
    }
}

// One admission/completion contract for predicate-only and property-aware
// executions. Neither path gets a second meter or converts a source error to
// null. The accessors and private terminal collector are the only differences.
fn governed<Row, E, C, Checkpoint>(
    snapshot_records: u64,
    policy: GqlQueryPolicy,
    mut checkpoint: Checkpoint,
    execute: impl FnOnce(&mut QueryMeter<Checkpoint>) -> Result<Vec<Row>, GqlQueryError<E, C>>,
) -> Result<GqlQueryExecution<Row>, GqlQueryError<E, C>>
where
    Checkpoint: FnMut() -> Result<(), C>,
{
    checkpoint().map_err(GqlQueryError::Interrupted)?;
    policy
        .rows
        .check(GqlBudgetDimension::SnapshotRecords, snapshot_records)
        .map_err(GqlQueryError::Rows)?;
    let mut meter = QueryMeter {
        checkpoint,
        policy,
        evaluator: GlaExecutionStats::default(),
        rows: GqlExecutionStats {
            snapshot_records,
            result_rows: 0,
        },
    };
    let value = execute(&mut meter)?;
    (meter.checkpoint)().map_err(GqlQueryError::Interrupted)?;
    debug_assert_eq!(
        u64::try_from(value.len()).ok(),
        Some(meter.rows.result_rows)
    );
    Ok(GqlQueryExecution {
        value,
        rows: meter.rows,
        evaluator: meter.evaluator,
    })
}

impl<Row: GlaIdentityOutput> GlaPlan<Row> {
    /// Admission precedes index construction. Each final row is checked before
    /// release; refusal never returns a partially constructed result vector.
    pub fn execute_budgeted<E>(
        &self,
        snapshot_records: u64,
        vertices: impl IntoIterator<Item = VId>,
        edges: impl IntoIterator<Item = (VId, RelationId, VId)>,
        mut test_vertex: impl FnMut(VId, &[VertexPredicate]) -> Result<bool, E>,
        budget: GqlExecutionBudget,
    ) -> Result<BudgetedGqlExecution<Vec<Row>>, BudgetedGqlError<E>> {
        budget
            .check(GqlBudgetDimension::SnapshotRecords, snapshot_records)
            .map_err(BudgetedGqlError::Budget)?;
        let mut stats = GqlExecutionStats {
            snapshot_records,
            result_rows: 0,
        };
        let value = self.execute_with_control(
            vertices,
            edges,
            |vid, predicates| test_vertex(vid, predicates).map_err(BudgetedGqlError::Execution),
            |event| charge_result_row(&mut stats, budget, event).map_err(BudgetedGqlError::Budget),
        )?;
        debug_assert_eq!(u64::try_from(value.len()).ok(), Some(stats.result_rows));
        Ok(BudgetedGqlExecution { value, stats })
    }

    /// Identity outputs do not require a property accessor. Value outputs must
    /// use execute_governed_with_properties instead; an omitted source cannot
    /// silently turn real properties into nulls.
    pub fn execute_governed<E, C>(
        &self,
        snapshot_records: u64,
        vertices: impl IntoIterator<Item = VId>,
        edges: impl IntoIterator<Item = (VId, RelationId, VId)>,
        mut test_vertex: impl FnMut(VId, &[VertexPredicate]) -> Result<bool, E>,
        policy: GqlQueryPolicy,
        checkpoint: impl FnMut() -> Result<(), C>,
    ) -> Result<GqlQueryExecution<Row>, GqlQueryError<E, C>> {
        governed(snapshot_records, policy, checkpoint, |meter| {
            self.execute_with_control(
                vertices,
                edges,
                |vid, predicates| test_vertex(vid, predicates).map_err(GqlQueryError::Source),
                |event| meter.observe(event),
            )
        })
    }
}

impl<Row: GlaOutput> GlaPlan<Row> {
    /// Project exact borrowed canonical values from the SAME admitted snapshot
    /// as matching. A source error remains Source, never a missing/null value.
    /// Property payloads are cloned only for newly retained complete rows and
    /// only after their logical scratch reservations. Result rows are owned.
    #[allow(clippy::too_many_arguments)]
    pub fn execute_governed_with_properties<'a, E, C>(
        &self,
        snapshot_records: u64,
        vertices: impl IntoIterator<Item = VId>,
        edges: impl IntoIterator<Item = (VId, RelationId, VId)>,
        mut test_vertex: impl FnMut(VId, &[VertexPredicate]) -> Result<bool, E>,
        mut property: impl FnMut(VId, PropertyKeyId) -> Result<Option<&'a CanonicalScalar>, E>,
        policy: GqlQueryPolicy,
        checkpoint: impl FnMut() -> Result<(), C>,
    ) -> Result<GqlQueryExecution<Row>, GqlQueryError<E, C>> {
        governed(snapshot_records, policy, checkpoint, |meter| {
            self.execute_with_properties_control(
                vertices,
                edges,
                |vid, predicates| test_vertex(vid, predicates).map_err(GqlQueryError::Source),
                |vid, key| property(vid, key).map_err(GqlQueryError::Source),
                |event| meter.observe(event),
            )
        })
    }
}

fn charge_result_row(
    rows: &mut GqlExecutionStats,
    budget: GqlExecutionBudget,
    event: GlaExecutionEvent,
) -> Result<(), GqlBudgetExceeded> {
    if event == GlaExecutionEvent::ResultRow {
        let next = rows
            .result_rows
            .checked_add(1)
            .expect("an in-memory result cannot contain 2^64 rows");
        budget.check(GqlBudgetDimension::ResultRows, next)?;
        rows.result_rows = next;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RelationBind;
    use std::cell::Cell;

    fn plan(tail: &str) -> GlaPlan {
        GlaPlan::lower(
            &RelationBind::new()
                .with_relation("R", RelationId(1))
                .bind(&format!("MATCH (a)-[:R]->(b) RETURN b{tail}"))
                .unwrap(),
        )
    }
    fn edges() -> [(VId, RelationId, VId); 3] {
        [
            (VId(1), RelationId(1), VId(3)),
            (VId(1), RelationId(1), VId(2)),
            (VId(1), RelationId(1), VId(2)),
        ]
    }

    #[test]
    fn row_budget_refuses_at_first_excess_output_and_preserves_source_errors() {
        let edges = [2, 3, 4, 5].map(|id| (VId(1), RelationId(1), VId(id)));
        let result = plan("").execute_budgeted(
            4,
            [],
            edges,
            |_, _| Ok::<_, &str>(true),
            GqlExecutionBudget::result_rows(1),
        );
        assert!(matches!(
            result,
            Err(BudgetedGqlError::Budget(GqlBudgetExceeded {
                dimension: GqlBudgetDimension::ResultRows,
                limit: 1,
                observed: 2
            }))
        ));
        let predicates = GlaPlan::lower(
            &RelationBind::new()
                .with_relation("R", RelationId(1))
                .with_label("L", fgdb_delta_types::LabelId(1))
                .bind("MATCH (a)-[:R]->(b:L) RETURN b")
                .unwrap(),
        );
        let result = predicates.execute_budgeted(
            4,
            [],
            edges,
            |vid, _| {
                if vid == VId(5) {
                    Err("last predicate failed")
                } else {
                    Ok(true)
                }
            },
            GqlExecutionBudget::result_rows(0),
        );
        assert!(matches!(
            result,
            Err(BudgetedGqlError::Execution("last predicate failed"))
        ));
    }

    #[test]
    fn row_budget_admission_and_pagination_share_the_canonical_evaluator() {
        let consumed = Cell::new(0);
        let result = plan("").execute_budgeted(
            3,
            [],
            edges()
                .into_iter()
                .inspect(|_| consumed.set(consumed.get() + 1)),
            |_, _| Ok::<_, ()>(true),
            GqlExecutionBudget::snapshot_records(2),
        );
        assert!(matches!(
            result,
            Err(BudgetedGqlError::Budget(GqlBudgetExceeded {
                dimension: GqlBudgetDimension::SnapshotRecords,
                limit: 2,
                observed: 3
            }))
        ));
        assert_eq!(consumed.get(), 0, "admission precedes evaluator input");
        for (tail, expected) in [(" SKIP 1 LIMIT 1", vec![VId(3)]), (" SKIP 2", vec![])] {
            let result = plan(tail)
                .execute_budgeted(
                    3,
                    [],
                    edges(),
                    |_, _| Ok::<_, ()>(true),
                    GqlExecutionBudget::new(3, expected.len() as u64),
                )
                .unwrap();
            assert_eq!(result.value, expected);
            assert_eq!(
                result.stats,
                GqlExecutionStats {
                    snapshot_records: 3,
                    result_rows: expected.len() as u64
                }
            );
        }
        let mut zero = crate::RelationBind::new()
            .with_relation("R", RelationId(1))
            .bind("MATCH (a)-[:R]->(b) RETURN b")
            .unwrap();
        zero.limit = Some(0);
        let result = GlaPlan::lower(&zero)
            .execute_budgeted(
                3,
                [],
                edges(),
                |_, _| Ok::<_, ()>(true),
                GqlExecutionBudget::new(3, 0),
            )
            .unwrap();
        assert!(result.value.is_empty());
        assert_eq!(
            result.stats,
            GqlExecutionStats {
                snapshot_records: 3,
                result_rows: 0
            }
        );
    }

    #[test]
    fn all_four_dimensions_are_enforced_in_one_execution() {
        let plan = plan("");
        let run = |policy| {
            plan.execute_governed(
                3,
                [],
                edges(),
                |_, _| Ok::<_, ()>(true),
                policy,
                || Ok::<_, ()>(()),
            )
        };
        let wide = run(GqlQueryPolicy::new(3, 2, u64::MAX, u64::MAX)).unwrap();
        assert_eq!(wide.value, vec![VId(2), VId(3)]);
        assert_eq!(
            wide.rows,
            GqlExecutionStats {
                snapshot_records: 3,
                result_rows: 2
            }
        );
        let exact = GqlQueryPolicy::new(
            3,
            2,
            wide.evaluator.work_units,
            wide.evaluator.scratch_entries,
        );
        assert_eq!(run(exact).unwrap(), wide);
        for policy in [
            GqlQueryPolicy::new(2, 2, u64::MAX, u64::MAX),
            GqlQueryPolicy::new(3, 1, u64::MAX, u64::MAX),
        ] {
            assert!(matches!(run(policy), Err(GqlQueryError::Rows(_))));
        }
        for policy in [
            GqlQueryPolicy::new(3, 2, exact.evaluator.max_work_units - 1, u64::MAX),
            GqlQueryPolicy::new(3, 2, u64::MAX, exact.evaluator.max_scratch_entries - 1),
        ] {
            assert!(matches!(run(policy), Err(GqlQueryError::Evaluator(_))));
        }
    }

    #[test]
    fn rejected_admission_never_consumes_the_edge_iterator() {
        let consumed = Cell::new(0);
        let input = edges()
            .into_iter()
            .inspect(|_| consumed.set(consumed.get() + 1));
        let error = plan("")
            .execute_governed(
                3,
                [],
                input,
                |_, _| Ok::<_, ()>(true),
                GqlQueryPolicy::new(2, 2, 100, 100),
                || Ok::<_, ()>(()),
            )
            .unwrap_err();
        assert!(matches!(
            error,
            GqlQueryError::Rows(GqlBudgetExceeded {
                dimension: GqlBudgetDimension::SnapshotRecords,
                observed: 3,
                limit: 2
            })
        ));
        assert_eq!(consumed.get(), 0);
    }

    #[test]
    fn result_events_count_final_rows_and_can_refuse_the_copy_tail() {
        let returned = Cell::new(0);
        let error = plan("")
            .execute_with_control(
                [],
                edges(),
                |_, _| Ok(true),
                |event| {
                    if event == GlaExecutionEvent::ResultRow {
                        returned.set(returned.get() + 1);
                        if returned.get() == 2 {
                            return Err("cancel at final row copy");
                        }
                    }
                    Ok(())
                },
            )
            .unwrap_err();
        assert_eq!(error, "cancel at final row copy");
        assert_eq!(returned.get(), 2);
        let paged = plan(" SKIP 1 LIMIT 1")
            .execute_governed(
                3,
                [],
                edges(),
                |_, _| Ok::<_, ()>(true),
                GqlQueryPolicy::new(3, 1, 100, 100),
                || Ok::<_, ()>(()),
            )
            .unwrap();
        assert_eq!(paged.value, vec![VId(3)]);
        assert_eq!(paged.rows.result_rows, 1);
    }

    #[test]
    fn interruption_at_every_checkpoint_is_terminal_even_for_empty_results() {
        for tail in ["", " SKIP 99"] {
            let logical = plan(tail);
            let calls = Cell::new(0);
            logical
                .execute_governed(
                    3,
                    [],
                    edges(),
                    |_, _| Ok::<_, ()>(true),
                    GqlQueryPolicy::new(3, 2, 100, 100),
                    || {
                        calls.set(calls.get() + 1);
                        Ok::<_, usize>(())
                    },
                )
                .unwrap();
            for stop in 1..=calls.get() {
                let at = Cell::new(0);
                let result = logical.execute_governed(
                    3,
                    [],
                    edges(),
                    |_, _| Ok::<_, ()>(true),
                    GqlQueryPolicy::new(3, 2, 100, 100),
                    || {
                        at.set(at.get() + 1);
                        if at.get() == stop { Err(stop) } else { Ok(()) }
                    },
                );
                assert!(matches!(result, Err(GqlQueryError::Interrupted(value)) if value == stop));
                assert_eq!(at.get(), stop);
            }
        }
    }
}
