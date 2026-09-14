//! One cumulative allowance for GLA operands and relational set execution.

use super::*;
use crate::algebra::{GraphValue, GraphValueRow};
use crate::algebra_exec::charge_payload;
use crate::{
    GlaExecutionEvent, GlaExecutionStats, GlaLimitDimension, GlaLimitExceeded, GqlBudgetExceeded,
    GqlExecutionStats,
};
use core::cmp::Ordering;

struct Meter<Checkpoint> {
    policy: GqlQueryPolicy,
    checkpoint: Checkpoint,
    rows: GqlExecutionStats,
    evaluator: GlaExecutionStats,
}
fn evaluator_sum<E, C>(
    used: u64,
    added: u64,
    limit: u64,
    dimension: GlaLimitDimension,
) -> SetResult<u64, E, C> {
    let observed = u128::from(used) + u128::from(added);
    if observed > u128::from(limit) {
        return Err(GqlQueryError::Evaluator(GlaLimitExceeded {
            dimension,
            limit,
            observed,
        }));
    }
    Ok(observed as u64)
}
impl<Checkpoint> Meter<Checkpoint> {
    fn event<E, C>(&mut self, event: GlaExecutionEvent) -> SetResult<(), E, C>
    where
        Checkpoint: FnMut() -> Result<(), C>,
    {
        (self.checkpoint)().map_err(GqlQueryError::Interrupted)?;
        let result_rows = if event == GlaExecutionEvent::ResultRow {
            let next = self.rows.result_rows.checked_add(1).ok_or_else(|| {
                GqlQueryError::Source(GraphSetExecutionError::AccountingOverflow {
                    dimension: GqlBudgetDimension::ResultRows,
                })
            })?;
            self.policy
                .rows
                .check(GqlBudgetDimension::ResultRows, next)
                .map_err(GqlQueryError::Rows)?;
            next
        } else {
            self.rows.result_rows
        };
        let work_units = evaluator_sum(
            self.evaluator.work_units,
            1,
            self.policy.evaluator.max_work_units,
            GlaLimitDimension::WorkUnits,
        )?;
        let scratch_entries = evaluator_sum(
            self.evaluator.scratch_entries,
            u64::from(event == GlaExecutionEvent::ScratchEntry),
            self.policy.evaluator.max_scratch_entries,
            GlaLimitDimension::ScratchEntries,
        )?;
        self.rows.result_rows = result_rows;
        self.evaluator = GlaExecutionStats {
            work_units,
            scratch_entries,
        };
        Ok(())
    }
    fn remaining(&self) -> GqlQueryPolicy {
        GqlQueryPolicy::new(
            self.policy.rows.max_snapshot_records().unwrap_or(u64::MAX)
                - self.rows.snapshot_records,
            u64::MAX,
            self.policy.evaluator.max_work_units - self.evaluator.work_units,
            self.policy.evaluator.max_scratch_entries - self.evaluator.scratch_entries,
        )
    }
    fn absorb<E, C>(
        &mut self,
        execution: &GqlQueryExecution<GraphValueRow>,
        operand: usize,
    ) -> SetResult<(), E, C> {
        if u64::try_from(execution.value.len()).ok() != Some(execution.rows.result_rows) {
            return Err(GqlQueryError::Source(
                GraphSetExecutionError::InvalidSourceStatistics { operand },
            ));
        }
        let snapshot_records = self
            .rows
            .snapshot_records
            .checked_add(execution.rows.snapshot_records)
            .ok_or_else(|| {
                GqlQueryError::Source(GraphSetExecutionError::AccountingOverflow {
                    dimension: GqlBudgetDimension::SnapshotRecords,
                })
            })?;
        self.policy
            .rows
            .check(GqlBudgetDimension::SnapshotRecords, snapshot_records)
            .map_err(GqlQueryError::Rows)?;
        let work_units = evaluator_sum(
            self.evaluator.work_units,
            execution.evaluator.work_units,
            self.policy.evaluator.max_work_units,
            GlaLimitDimension::WorkUnits,
        )?;
        let scratch_entries = evaluator_sum(
            self.evaluator.scratch_entries,
            execution.evaluator.scratch_entries,
            self.policy.evaluator.max_scratch_entries,
            GlaLimitDimension::ScratchEntries,
        )?;
        self.rows.snapshot_records = snapshot_records;
        self.evaluator = GlaExecutionStats {
            work_units,
            scratch_entries,
        };
        Ok(())
    }
    fn source_error<E, C>(
        &self,
        error: GqlQueryError<E, C>,
    ) -> GqlQueryError<GraphSetExecutionError<E>, C> {
        match error {
            GqlQueryError::Source(error) => {
                GqlQueryError::Source(GraphSetExecutionError::Source(error))
            }
            GqlQueryError::Interrupted(error) => GqlQueryError::Interrupted(error),
            GqlQueryError::Evaluator(error) => {
                let (used, limit) = match error.dimension {
                    GlaLimitDimension::WorkUnits => (
                        self.evaluator.work_units,
                        self.policy.evaluator.max_work_units,
                    ),
                    GlaLimitDimension::ScratchEntries => (
                        self.evaluator.scratch_entries,
                        self.policy.evaluator.max_scratch_entries,
                    ),
                };
                GqlQueryError::Evaluator(GlaLimitExceeded {
                    dimension: error.dimension,
                    limit,
                    observed: error.observed.saturating_add(u128::from(used)),
                })
            }
            GqlQueryError::Rows(error)
                if error.dimension == GqlBudgetDimension::SnapshotRecords =>
            {
                match self.rows.snapshot_records.checked_add(error.observed) {
                    Some(observed) => GqlQueryError::Rows(GqlBudgetExceeded {
                        dimension: error.dimension,
                        limit: self.policy.rows.max_snapshot_records().unwrap_or(u64::MAX),
                        observed,
                    }),
                    None => GqlQueryError::Source(GraphSetExecutionError::AccountingOverflow {
                        dimension: error.dimension,
                    }),
                }
            }
            GqlQueryError::Rows(error) => GqlQueryError::Rows(error),
        }
    }
}

pub(super) fn execute<E, C, S, Checkpoint>(
    query: &PreparedGraphSet,
    policy: GqlQueryPolicy,
    source: &mut S,
    checkpoint: Checkpoint,
) -> SetResult<GqlQueryExecution<GraphValueRow>, E, C>
where
    S: FnMut(
        &PreparedGraphPattern<GraphValueRow>,
        GqlQueryPolicy,
    ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<E, C>>,
    Checkpoint: FnMut() -> Result<(), C>,
{
    let mut meter = Meter {
        policy,
        checkpoint,
        rows: GqlExecutionStats {
            snapshot_records: 0,
            result_rows: 0,
        },
        evaluator: GlaExecutionStats::default(),
    };
    let mut operand = 0;
    let value = run(query, source, &mut meter, &mut operand)?;
    // Nothing escapes until all sources and relational operations succeed.
    for _ in &value {
        meter.event(GlaExecutionEvent::ResultRow)?;
    }
    (meter.checkpoint)().map_err(GqlQueryError::Interrupted)?;
    Ok(GqlQueryExecution {
        value,
        rows: meter.rows,
        evaluator: meter.evaluator,
    })
}

fn run<E, C, S, Checkpoint>(
    query: &PreparedGraphSet,
    source: &mut S,
    meter: &mut Meter<Checkpoint>,
    operand: &mut usize,
) -> SetResult<Vec<GraphValueRow>, E, C>
where
    S: FnMut(
        &PreparedGraphPattern<GraphValueRow>,
        GqlQueryPolicy,
    ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<E, C>>,
    Checkpoint: FnMut() -> Result<(), C>,
{
    meter.event(GlaExecutionEvent::Work)?;
    let mut rows = match &query.node {
        SetNode::Pattern(pattern) => {
            let at = *operand;
            *operand += 1;
            let execution =
                source(pattern, meter.remaining()).map_err(|error| meter.source_error(error))?;
            meter.absorb(&execution, at)?;
            for row in &execution.value {
                meter.event(GlaExecutionEvent::ScratchEntry)?;
                if row.len() != query.types.len() {
                    return Err(GqlQueryError::Source(GraphSetExecutionError::InputSchema {
                        operand: at,
                    }));
                }
                for (value, kind) in row.values().iter().zip(&query.types) {
                    meter.event(GlaExecutionEvent::Work)?;
                    let valid = match kind {
                        GraphSetColumnType::Vertex => {
                            value.is_null() || value.as_vertex().is_some()
                        }
                        GraphSetColumnType::Scalar => value.as_scalar().is_some(),
                    };
                    if !valid {
                        return Err(GqlQueryError::Source(GraphSetExecutionError::InputSchema {
                            operand: at,
                        }));
                    }
                }
            }
            let mut rows = execution.value;
            merge::sort(
                &mut rows,
                &mut |event| meter.event(event),
                &mut |a, b, control| compare_rows(a, b, &[], control),
            )?;
            rows
        }
        SetNode::Scope(input) => run(input, source, meter, operand)?,
        SetNode::Binary {
            operation,
            quantifier,
            left,
            right,
        } => {
            let left = run(left, source, meter, operand)?;
            let right = run(right, source, meter, operand)?;
            merge::combine(
                left,
                right,
                *operation,
                *quantifier,
                &mut |event| meter.event(event),
                &mut |a, b, control| compare_rows(a, b, &[], control),
            )?
        }
    };
    if !query.order.is_empty() {
        merge::sort(
            &mut rows,
            &mut |event| meter.event(event),
            &mut |a, b, control| compare_rows(a, b, &query.order, control),
        )?;
    }
    if query.offset == 0 && query.count.is_none() {
        return Ok(rows);
    }
    let mut output = Vec::new();
    let mut skip = query.offset;
    let mut remaining = query.count.unwrap_or(u64::MAX);
    for row in rows {
        meter.event(GlaExecutionEvent::Work)?;
        if skip != 0 {
            skip -= 1;
            continue;
        }
        if remaining == 0 {
            continue;
        }
        meter.event(GlaExecutionEvent::ScratchEntry)?;
        output.push(row);
        remaining -= 1;
    }
    Ok(output)
}

fn compare_value<E>(
    a: &GraphValue,
    b: &GraphValue,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<Ordering, E> {
    control(GlaExecutionEvent::Work)?;
    for value in [a, b] {
        if let Some(scalar) = value.as_scalar() {
            charge_payload(scalar, control)?;
        }
    }
    Ok(a.cmp(b))
}
fn compare_rows<E>(
    a: &GraphValueRow,
    b: &GraphValueRow,
    order: &[GraphValueOrder],
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<Ordering, E> {
    for key in order {
        control(GlaExecutionEvent::Work)?;
        // Definitions and each operand's complete output schema were checked.
        let (a, b) = (&a.values()[key.column], &b.values()[key.column]);
        let result = match (a.is_null(), b.is_null()) {
            (true, true) => Ordering::Equal,
            (true, false) => {
                if key.nulls_first {
                    Ordering::Less
                } else {
                    Ordering::Greater
                }
            }
            (false, true) => {
                if key.nulls_first {
                    Ordering::Greater
                } else {
                    Ordering::Less
                }
            }
            (false, false) => {
                let result = compare_value(a, b, control)?;
                if key.descending {
                    result.reverse()
                } else {
                    result
                }
            }
        };
        if result != Ordering::Equal {
            return Ok(result);
        }
    }
    for (a, b) in a.values().iter().zip(b.values()) {
        let result = compare_value(a, b, control)?;
        if result != Ordering::Equal {
            return Ok(result);
        }
    }
    Ok(a.len().cmp(&b.len()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use fgdb_types::VId;

    #[test]
    fn cumulative_counters_refuse_overflow_without_mutation() {
        let mut meter = Meter {
            policy: GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX),
            checkpoint: || Ok::<_, ()>(()),
            rows: GqlExecutionStats {
                snapshot_records: 0,
                result_rows: 0,
            },
            evaluator: GlaExecutionStats {
                work_units: u64::MAX,
                scratch_entries: 0,
            },
        };
        assert!(matches!(meter.event::<(), ()>(GlaExecutionEvent::Work),
            Err(GqlQueryError::Evaluator(GlaLimitExceeded { observed, .. })) if observed == u128::from(u64::MAX) + 1));
        assert_eq!(meter.evaluator.work_units, u64::MAX);
        meter.evaluator.work_units = 0;
        meter.rows.snapshot_records = u64::MAX;
        let input = GqlQueryExecution {
            value: Vec::new(),
            rows: GqlExecutionStats {
                snapshot_records: 1,
                result_rows: 0,
            },
            evaluator: GlaExecutionStats::default(),
        };
        assert!(matches!(
            meter.absorb::<(), ()>(&input, 0),
            Err(GqlQueryError::Source(
                GraphSetExecutionError::AccountingOverflow { .. }
            ))
        ));
        assert_eq!(meter.rows.snapshot_records, u64::MAX);
    }

    fn scope_leaf() -> PreparedGraphSet {
        let mut builder = crate::algebra::GraphPatternBuilder::new();
        builder.vertex("n").unwrap();
        builder
            .prepare_values(&[crate::algebra::GraphColumn::vertex("n", "n")], 0, None)
            .unwrap()
            .with_duplicates()
            .into()
    }

    #[test]
    fn nested_pages_select_before_outer_reordering_without_repeating_the_source() {
        let inner = scope_leaf()
            .with_order_by(&[GraphValueOrder::descending(0)])
            .unwrap()
            .with_page(1, Some(3));
        let query = inner
            .clone()
            .nested()
            .unwrap()
            .with_order_by(&[GraphValueOrder::ascending(0)])
            .unwrap()
            .with_page(0, Some(1));
        assert_ne!(query.canonical_bytes(), inner.canonical_bytes());
        let mut calls = 0;
        let policy = GqlQueryPolicy::new(5, 1, 100_000, 100_000);
        let result = query
            .execute_governed(
                policy,
                |pattern, remaining| {
                    calls += 1;
                    pattern.plan().execute_governed_with_properties(
                        5,
                        (1..=5).map(VId),
                        [],
                        |_, _| Ok::<_, ()>(true),
                        |_, _| Ok(None),
                        remaining,
                        || Ok::<_, ()>(()),
                    )
                },
                || Ok::<_, ()>(()),
            )
            .unwrap();
        assert_eq!(calls, 1);
        assert_eq!(result.rows.snapshot_records, 5);
        assert_eq!(result.value[0].get(0).unwrap().as_vertex(), Some(VId(2)));
    }

    #[test]
    fn unary_scopes_and_binary_combinations_share_the_definition_depth_limit() {
        let leaf = scope_leaf();
        let mut deepest = leaf.clone();
        for _ in 1..MAX_GRAPH_SET_DEPTH {
            deepest = deepest.nested().unwrap();
        }
        assert_eq!(deepest.operand_count(), 1);
        assert!(matches!(
            deepest.clone().nested(),
            Err(GraphSetBuildError::TooDeep { .. })
        ));
        assert!(matches!(
            leaf.combine(GraphSetOperation::Union, GraphSetQuantifier::All, deepest),
            Err(GraphSetBuildError::TooDeep { .. })
        ));
    }
}
