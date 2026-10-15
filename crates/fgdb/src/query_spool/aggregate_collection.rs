//! Memory admission for the existing native collection cells. This module
//! does not collect, deduplicate, sort, encode or interpret a query itself.
//! Stable external partitions preserve the completed input's per-key order.

use super::*;

type CellError = GqlQueryError<
    fgdb_gql::GraphAggregateError<ScanError<ReadError>>,
    NativeAggregateSpoolError,
>;

fn refusal(error: CellError) -> NativeAggregateSpoolError {
    match error {
        GqlQueryError::Interrupted(error) => error,
        GqlQueryError::Source(error) => execute_error(GqlQueryError::Source(error)),
        GqlQueryError::Rows(error) => execute_error(GqlQueryError::Rows(error)),
        GqlQueryError::Evaluator(error) => execute_error(GqlQueryError::Evaluator(error)),
        GqlQueryError::IdentifiedEdgesRequired => {
            execute_error(GqlQueryError::IdentifiedEdgesRequired)
        }
        GqlQueryError::Data(error) => execute_error(GqlQueryError::Data(error)),
    }
}

fn control<'a>(
    input: &'a mut dyn GroupInput,
    charge: &'a mut MemoryCharge,
    cx: &'a QueryCx,
) -> impl FnMut(VertexScanEvent) -> core::result::Result<(), CellError> + 'a {
    move |event| {
        // Keep the original cumulative native meter. Its refusal is terminal,
        // never disguised as physical capacity pressure and retried.
        input
            .charge(event)
            .map_err(execute_error)
            .map_err(GqlQueryError::Interrupted)?;
        if event == VertexScanEvent::ScratchEntry {
            // Native collection insertion charges an occurrence and each
            // 64-byte payload unit BEFORE cloning/growing; DISTINCT separately
            // charges its membership copy. Eight slots cover geometric vector
            // growth, membership nodes, boxing and the final envelope copy.
            // Keep this conservative reservation until the group's final
            // encoded append completes; do not refund it when a row gets wider.
            let bytes = 8 * size_of::<GraphValue>()
                .max(fgdb_gql::algebra::GRAPH_VALUE_PAYLOAD_UNIT_BYTES);
            charge.grow(cx, bytes).map_err(|error| {
                GqlQueryError::Interrupted(SpillError::Memory(error).into())
            })?;
        }
        Ok(())
    }
}

pub(super) fn capacity_refusal(error: &NativeAggregateSpoolError) -> bool {
    matches!(
        error,
        NativeAggregateSpoolError::Spool(NativeSpoolError::Spill(
            SpillError::Memory(MemoryError::ResourceExhausted { .. })
        ))
    )
}

pub(super) fn new_state(
    definition: &SpillAggregateDefinition,
    input: &mut dyn GroupInput,
    charge: &mut MemoryCharge,
    cx: &QueryCx,
) -> Result<SpillAggregateState> {
    if !definition.has_collections() {
        return definition
            .new_state(&mut |event| input.charge(event))
            .map_err(execute_error);
    }
    definition
        .new_state(&mut control(input, charge, cx))
        .map_err(refusal)
}

pub(super) fn update(
    definition: &SpillAggregateDefinition,
    state: &mut SpillAggregateState,
    row: &GraphValueRow,
    input: &mut dyn GroupInput,
    charge: &mut MemoryCharge,
    cx: &QueryCx,
) -> Result<()> {
    if !definition.has_collections() {
        return definition
            .update(state, row, &mut |event| input.charge(event))
            .map_err(execute_error);
    }
    definition
        .update(state, row, &mut control(input, charge, cx))
        .map_err(refusal)
}

pub(super) fn finish(
    definition: &SpillAggregateDefinition,
    keys: Vec<GraphValue>,
    state: SpillAggregateState,
    input: &mut dyn GroupInput,
    charge: &mut MemoryCharge,
    cx: &QueryCx,
) -> Result<GraphAggregateRow> {
    if !definition.has_collections() {
        return definition
            .finish(keys, state, &mut |event| input.charge(event))
            .map_err(execute_error);
    }
    definition
        .finish(keys, state, &mut control(input, charge, cx))
        .map_err(refusal)
}

#[cfg(test)]
#[path = "aggregate_collection_tests.rs"]
mod tests;
