//! Meter/host-seam laws; native storage semantics are tested by fgdb.

use super::*;
use crate::{
    GqlParameters, GraphSymbol, GraphSymbolKind, GraphWriteStepReceipt, PreparedGraphSetText,
    PreparedGraphWriteScript,
};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_types::VId;

fn definition(text: &str) -> PreparedGraphQueryWrite {
    let source = PreparedGraphSetText::prepare(text, |_, _| None)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
    let script =
        PreparedGraphWriteScript::prepare("CREATE (n {key:$key})", RelationId(1), |kind, name| {
            match (kind, name) {
                (GraphSymbolKind::Property, "key") => Some(GraphSymbol::Property(PropertyKeyId(1))),
                _ => None,
            }
        })
        .unwrap();
    PreparedGraphQueryWrite::prepare(source, script).unwrap()
}

fn read(
    query: &PreparedGraphSet,
    policy: GqlQueryPolicy,
) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<GraphSetExecutionError<()>, usize>> {
    query.execute_governed(policy, |_, _| Err(GqlQueryError::Source(())), || Ok(()))
}

fn policy() -> GraphWriteProgramPolicy {
    GraphWriteProgramPolicy::new(GqlQueryPolicy::new(100, 100, 100_000, 100_000), 0, 3, 0)
}

// A controlled host return lets these laws isolate composition arithmetic
// from the separately governed storage executor. No graph behavior is mocked
// and then advertised as an integration result.
fn receipt(batch: &BoundGraphWriteScriptBatch) -> GraphWriteProgramReceipt {
    let count = batch.program().statements().len();
    GraphWriteProgramReceipt::new(
        GraphWriteProgramStats {
            completed_statements: count,
            selection: GqlExecutionStats {
                snapshot_records: 2,
                result_rows: count as u64,
            },
            evaluator: GlaExecutionStats::default(),
            target_vertex_visits: 0,
            target_edge_visits: 0,
            mutation_effects: 0,
            created_vertices: count as u64,
            created_edges: 0,
        },
        (0..count)
            .map(|row| GraphWriteStepReceipt::Insert {
                vertices: vec![VId(100 + row as u128)],
                edges: Vec::new(),
            })
            .collect(),
    )
}

#[test]
fn one_stage_gets_only_the_residual_quota_and_totals_include_every_phase() {
    let definition = definition("UNWIND [1,2,1] AS key RETURN key");
    let original = policy();
    let source = read(definition.source(), original.mutations.query).unwrap();
    let mut calls = 0;
    let mut allowance = None;
    let result = definition
        .execute_governed::<(), (), usize>(
            original,
            3,
            read,
            |batch, remaining| {
                calls += 1;
                assert_eq!(batch.argument_sets(), 3);
                allowance = Some(remaining);
                Ok(receipt(batch))
            },
            || Ok(()),
        )
        .unwrap();
    let allowance = allowance.unwrap();
    assert_eq!(calls, 1);
    assert_eq!(result.input_rows, 3);
    assert_eq!(result.stats.completed_statements, 3);
    assert_eq!(result.stats.created_vertices, 3);
    assert_eq!(
        result.stats.selection.snapshot_records,
        source.rows.snapshot_records + 2
    );
    assert_eq!(result.stats.selection.result_rows, 6);
    assert_eq!(allowance.mutations.query.rows.max_result_rows(), Some(97));
    assert_eq!(
        allowance.mutations.query.evaluator.max_work_units + result.stats.evaluator.work_units,
        original.mutations.query.evaluator.max_work_units,
    );
    assert_eq!(
        allowance.mutations.query.evaluator.max_scratch_entries
            + result.stats.evaluator.scratch_entries,
        original.mutations.query.evaluator.max_scratch_entries,
    );
    assert!(result.stats.evaluator.work_units > source.evaluator.work_units);
    assert!(result.stats.evaluator.scratch_entries > source.evaluator.scratch_entries);
    assert_eq!(result.receipt.unwrap().stats().selection.result_rows, 3);
}

#[test]
fn count_and_binding_limits_refuse_instead_of_truncating_or_refreshing() {
    let definition = definition("UNWIND [1,2,1] AS key RETURN key");
    let baseline = definition
        .execute_governed::<(), (), usize>(
            policy(),
            3,
            read,
            |batch, _| Ok(receipt(batch)),
            || Ok(()),
        )
        .unwrap();
    for dimension in 0..3 {
        let mut limited = policy();
        let max_statements = if dimension == 0 { 2 } else { 3 };
        if dimension == 1 {
            limited.mutations.query.evaluator.max_work_units =
                baseline.stats.evaluator.work_units - 1;
        } else if dimension == 2 {
            limited.mutations.query.evaluator.max_scratch_entries =
                baseline.stats.evaluator.scratch_entries - 1;
        }
        let mut stages = 0;
        let error = definition
            .execute_governed::<(), (), usize>(
                limited,
                max_statements,
                read,
                |batch, _| {
                    stages += 1;
                    Ok(receipt(batch))
                },
                || Ok(()),
            )
            .unwrap_err();
        assert_eq!(stages, 0);
        match dimension {
            0 => assert!(matches!(
                error,
                GraphQueryWriteExecutionError::Source(GqlQueryError::Rows(_))
            )),
            _ => assert!(matches!(
                error,
                GraphQueryWriteExecutionError::Binding(GraphQueryWriteBindError::Interrupted {
                    source: GqlQueryError::Evaluator(_),
                    ..
                })
            )),
        }
    }
}

#[test]
fn empty_source_is_a_counted_noop_and_bad_host_statistics_never_escape() {
    let empty = definition("UNWIND [] AS key RETURN key");
    let mut stages = 0;
    let result = empty
        .execute_governed::<(), (), usize>(
            policy(),
            0,
            read,
            |batch, _| {
                stages += 1;
                Ok(receipt(batch))
            },
            || Ok(()),
        )
        .unwrap();
    assert_eq!(stages, 0);
    assert_eq!(result.input_rows, 0);
    assert_eq!(result.stats.completed_statements, 0);
    assert!(result.receipt.is_none());
    let definition = definition("RETURN 1 AS key");
    let error = definition
        .execute_governed::<(), (), usize>(
            policy(),
            1,
            |query, policy| {
                let mut rows = read(query, policy)?;
                rows.rows.result_rows = 0;
                Ok(rows)
            },
            |batch, _| {
                stages += 1;
                Ok(receipt(batch))
            },
            || Ok(()),
        )
        .unwrap_err();
    assert!(matches!(
        error,
        GraphQueryWriteExecutionError::InvalidSourceStatistics
    ));
    assert_eq!(stages, 0);
    let error = definition
        .execute_governed::<(), (), usize>(
            policy(),
            1,
            read,
            |batch, _| {
                let original = receipt(batch);
                let mut stats = original.stats();
                stats.evaluator.work_units = u64::MAX;
                Ok(GraphWriteProgramReceipt::new(
                    stats,
                    original.steps().to_vec(),
                ))
            },
            || Ok(()),
        )
        .unwrap_err();
    assert!(matches!(
        error,
        GraphQueryWriteExecutionError::InvalidProgramStatistics
    ));
}

#[test]
fn every_composition_checkpoint_including_after_staging_can_refuse() {
    let definition = definition("RETURN 1 AS key");
    let mut checkpoints = 0;
    definition
        .execute_governed::<(), (), usize>(
            policy(),
            1,
            read,
            |batch, _| Ok(receipt(batch)),
            || {
                checkpoints += 1;
                Ok(())
            },
        )
        .unwrap();
    for stop in 0..checkpoints {
        let mut at = 0;
        let mut stages = 0;
        let result = definition.execute_governed::<(), (), usize>(
            policy(),
            1,
            read,
            |batch, _| {
                stages += 1;
                Ok(receipt(batch))
            },
            || {
                let current = at;
                at += 1;
                if current == stop { Err(stop) } else { Ok(()) }
            },
        );
        assert!(result.is_err());
        assert_eq!(at, stop + 1);
        if stop + 1 == checkpoints {
            assert_eq!(stages, 1);
            assert!(matches!(result, Err(GraphQueryWriteExecutionError::Control(
                GqlQueryError::Interrupted(value)
            )) if value == stop));
        } else {
            assert_eq!(stages, 0);
        }
    }
}

#[test]
fn accounting_overflow_and_scratch_refusal_install_neither_counter() {
    let policy = GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX);
    for original in [
        GlaExecutionStats {
            work_units: u64::MAX,
            scratch_entries: 0,
        },
        GlaExecutionStats {
            work_units: 0,
            scratch_entries: u64::MAX,
        },
    ] {
        let mut used = original;
        assert!(matches!(charge::<()>(policy, &mut used, 1),
            Err(GqlQueryError::Evaluator(GlaLimitExceeded { observed, .. }))
                if observed == u128::from(u64::MAX) + 1));
        assert_eq!(used, original);
    }
}
