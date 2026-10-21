use super::*;
use crate::{GqlParameterType, GqlParameters, GraphMutationProgramError, PreparedGraphSetText, PreparedGraphWriteScript};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use std::cell::Cell;

type Failure = GraphQueryWriteExecutionError<&'static str, (), usize>;

fn policy() -> GraphWriteProgramPolicy {
    GraphWriteProgramPolicy::new(GqlQueryPolicy::new(100, 100, 1_000_000, 1_000_000), 100, 100, 100)
}

fn definition(text: &str) -> PreparedGraphQueryWrite {
    let source = PreparedGraphSetText::prepare(text, |_, _| None).unwrap()
        .bind_parameters(&GqlParameters::new()).unwrap();
    let mut resolve = |kind, name: &str| match (kind, name) {
        (crate::GraphSymbolKind::Property, "p") => Some(crate::GraphSymbol::Property(PropertyKeyId(1))),
        _ => None,
    };
    let script = PreparedGraphWriteScript::prepare_with_parameter_types_and_resolver(
        "CREATE (n {p:$doc.key})", RelationId(1), &[("doc", GqlParameterType::Map)], &mut resolve,
    ).unwrap();
    PreparedGraphQueryWrite::prepare(source, script).unwrap()
}

fn read(
    query: &PreparedGraphSet,
    policy: GqlQueryPolicy,
) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<GraphSetExecutionError<&'static str>, usize>> {
    query.execute_governed(policy, |_, _| Err(GqlQueryError::Source("unexpected graph source")), || Ok(()))
}

fn stop_stage() -> Result<GraphWriteProgramReceipt, GraphWriteScriptExecutionError<&'static str, (), usize>> {
    Err(GraphWriteScriptExecutionError::Program(
        GraphMutationProgramError::Preflight("stage observed").into(),
    ))
}

#[test]
fn host_binding_admission_sees_exact_recursive_reservations_before_stage() {
    let definition = definition("UNWIND [1,2] AS x RETURN {key:x,nested:[x,{v:x+1}]} AS doc");
    let source = read(definition.source(), policy().mutations.query).unwrap();
    let mut expected = Vec::new();
    let expected_batch = definition.bind_rows_controlled(&source.value, 2, |event| {
        expected.push(event);
        Ok::<(), usize>(())
    }).unwrap().unwrap();
    let mut observed = Vec::new();
    let mut residual = None;
    let mut canonical = None;
    let result: Result<_, Failure> = definition.execute_governed_with_binding_control(
        policy(), 2, read,
        |batch, allowance| {
            residual = Some(allowance);
            canonical = Some(batch.program().canonical_bytes());
            stop_stage()
        },
        || Ok(()),
        |event| { observed.push(event); Ok(()) },
    );
    assert!(matches!(result, Err(GraphQueryWriteExecutionError::Program(_))));
    assert_eq!(observed, expected);
    assert!(observed.len() > 20);
    assert_eq!(canonical.unwrap(), expected_batch.program().canonical_bytes());
    let units = observed.iter().map(|event| event.entries).sum::<u64>();
    let residual = residual.unwrap();
    assert_eq!(residual.mutations.query.evaluator.max_work_units,
        policy().mutations.query.evaluator.max_work_units - source.evaluator.work_units - units);
    assert_eq!(residual.mutations.query.evaluator.max_scratch_entries,
        policy().mutations.query.evaluator.max_scratch_entries - source.evaluator.scratch_entries - units);
    // The old API delegates to the new seam with a no-op host admission.
    let mut legacy = None;
    let _: Result<_, Failure> = definition.execute_governed(policy(), 2, read, |_, allowance| {
        legacy = Some(allowance);
        stop_stage()
    }, || Ok(()));
    assert_eq!(legacy, Some(residual));
}

#[test]
fn every_host_binding_refusal_stops_before_staging_and_keeps_its_row() {
    let definition = definition("UNWIND [1,2] AS x RETURN {key:x,nested:[x,{v:x+1}]} AS doc");
    let source = read(definition.source(), policy().mutations.query).unwrap();
    let original = source.value.clone();
    let mut events = Vec::new();
    definition.bind_rows_controlled(&source.value, 2, |event| {
        events.push(event);
        Ok::<(), usize>(())
    }).unwrap();
    for stop in 0..events.len() {
        let stages = Cell::new(0);
        let mut visited = 0;
        let result: Result<_, Failure> = definition.execute_governed_with_binding_control(
            policy(), 2, read,
            |_, _| { stages.set(stages.get() + 1); stop_stage() },
            || Ok(()),
            |event| {
                assert_eq!(event, events[visited]);
                let current = visited;
                visited += 1;
                if current == stop { Err(stop) } else { Ok(()) }
            },
        );
        assert!(matches!(result, Err(GraphQueryWriteExecutionError::Binding(
            GraphQueryWriteBindError::Interrupted {
                row, source: GqlQueryError::Interrupted(source),
            }
        )) if row == events[stop].row && source == stop));
        assert_eq!(visited, stop + 1);
        assert_eq!(stages.get(), 0);
        assert_eq!(source.value, original);
    }
}

#[test]
fn successful_host_admission_does_not_widen_native_limits() {
    let definition = definition("RETURN {key:1} AS doc");
    let source = read(definition.source(), policy().mutations.query).unwrap();
    let mut tight = policy();
    tight.mutations.query.evaluator.max_work_units = source.evaluator.work_units;
    let mut host_calls = 0;
    let stages = Cell::new(0);
    let result: Result<_, Failure> = definition.execute_governed_with_binding_control(
        tight, 1, read,
        |_, _| { stages.set(stages.get() + 1); stop_stage() },
        || Ok(()),
        |_| { host_calls += 1; Ok(()) },
    );
    assert!(matches!(result, Err(GraphQueryWriteExecutionError::Binding(
        GraphQueryWriteBindError::Interrupted { source: GqlQueryError::Evaluator(_), .. }
    ))));
    assert_eq!(host_calls, 1);
    assert_eq!(stages.get(), 0);
}

#[test]
fn empty_source_still_runs_host_acceptance_and_an_earlier_checkpoint_wins() {
    let definition = definition("UNWIND [] AS x RETURN {key:x} AS doc");
    let mut events = Vec::new();
    let accepted: Result<_, Failure> = definition.execute_governed_with_binding_control(
        policy(), 0, read, |_, _| stop_stage(), || Ok(()),
        |event| { events.push(event); Ok(()) },
    );
    let accepted = accepted.unwrap();
    assert_eq!(accepted.input_rows, 0);
    assert!(accepted.receipt.is_none());
    assert_eq!(events, vec![
        GraphQueryWriteBindEvent { row: None, entries: 0 },
        GraphQueryWriteBindEvent { row: None, entries: 0 },
    ]);
    let mut host_calls = 0;
    let checkpoints = Cell::new(0);
    let result: Result<_, Failure> = definition.execute_governed_with_binding_control(
        policy(), 0, read, |_, _| stop_stage(),
        || {
            let at = checkpoints.get();
            checkpoints.set(at + 1);
            if at == 1 { Err(19) } else { Ok(()) }
        },
        |_| { host_calls += 1; Ok(()) },
    );
    assert!(matches!(result, Err(GraphQueryWriteExecutionError::Binding(
        GraphQueryWriteBindError::Interrupted { source: GqlQueryError::Interrupted(19), .. }
    ))));
    assert_eq!(host_calls, 0);
}
