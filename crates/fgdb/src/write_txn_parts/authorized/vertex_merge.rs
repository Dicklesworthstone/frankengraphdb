//! Authorized unique-vertex MERGE and its selected ON MATCH/ON CREATE actions.
//! Reuse the native reducer and literal-action lowering, not a second matcher.

use crate::write_txn::authorized::{
    Database, Execution, Vfs, WriteBatch, WriteTxn, WriteTxnError, selection, stage,
};
use crate::write_txn::{collect_vertex_merge, vertex_upsert_actions};
use fgdb_gql::insertion::GraphInsertIntent;
use fgdb_gql::{
    GqlQueryError, GraphVertexMergeError, GraphVertexMergeOutcome, GraphVertexMergePolicy,
    GraphVertexMergeStats, GraphVertexUpsertError, GraphVertexUpsertPolicy, GraphVertexUpsertStats,
    PreparedGraphVertexMerge, PreparedGraphVertexUpsert,
};
use fgdb_types::QueryCx;
use fgdb_warden::PlannerPredicates;
use std::cell::RefCell;

type MergeFault = GqlQueryError<GraphVertexMergeError<WriteTxnError, WriteTxnError>, WriteTxnError>;
type UpsertFault =
    GqlQueryError<GraphVertexUpsertError<WriteTxnError, WriteTxnError>, WriteTxnError>;

// Both public program entry points verify ReadWrite rights before opening the
// private workspace. No internal proposal, match or permit escapes these steps.
#[allow(clippy::too_many_arguments)]
pub(super) fn merge<V: Vfs + Clone, Clock: FnMut() -> u64>(
    transaction: &mut WriteTxn,
    database: &mut Database<V>,
    cx: &QueryCx,
    merge: &PreparedGraphVertexMerge,
    policy: GraphVertexMergePolicy,
    scope: &PlannerPredicates,
    execution: &mut Execution<'_, '_, Clock>,
    returning: bool,
) -> Result<(GraphVertexMergeStats, GraphVertexMergeOutcome), MergeFault> {
    let proposal = cx.with_restriction(|| {
        // Selection and allocation share the native owner sequentially. The
        // common collector produces an internal proposal, never a receipt or
        // an independently staged/committed creation.
        let database = RefCell::new(&mut *database);
        let controls = RefCell::new(&mut *execution);
        collect_vertex_merge(
            merge,
            policy,
            |pattern, budget| {
                selection::select_overlay(
                    transaction,
                    &database.borrow(),
                    cx,
                    pattern,
                    scope,
                    budget,
                    &controls,
                )
            },
            |request| database.borrow_mut().allocate_identity(cx, request),
            || {
                cx.checkpoint().map_err(WriteTxnError::Interrupted)?;
                controls.borrow_mut().checkpoint()
            },
        )
    })?;
    if let Some(creation) = proposal.creation {
        for intent in creation.into_intents() {
            cx.checkpoint()
                .map_err(WriteTxnError::Interrupted)
                .map_err(|error| GqlQueryError::Source(GraphVertexMergeError::Source(error)))?;
            execution
                .checkpoint()
                .map_err(|error| GqlQueryError::Source(GraphVertexMergeError::Source(error)))?;
            let GraphInsertIntent::Vertex {
                vertex,
                labels,
                properties,
            } = intent
            else {
                unreachable!("validated vertex MERGE contains no edge creation")
            };
            let mut batch = WriteBatch::new(merge.relation());
            batch.create_vertex(vertex, labels, properties);
            for row in batch.rows {
                stage(transaction, database, batch.relation, row, execution)
                    .map_err(|error| GqlQueryError::Source(GraphVertexMergeError::Source(error)))?;
            }
        }
    }
    if returning {
        // The fixed-size private creation identity was NOT a delivered row.
        // Both Created and Matched now pay exactly one final receipt unit.
        deliver(execution)
            .map_err(|error| GqlQueryError::Source(GraphVertexMergeError::Source(error)))?;
    }
    Ok((proposal.stats, proposal.outcome))
}

#[allow(clippy::too_many_arguments)]
pub(super) fn upsert<V: Vfs + Clone, Clock: FnMut() -> u64>(
    transaction: &mut WriteTxn,
    database: &mut Database<V>,
    cx: &QueryCx,
    upsert: &PreparedGraphVertexUpsert,
    policy: GraphVertexUpsertPolicy,
    scope: &PlannerPredicates,
    execution: &mut Execution<'_, '_, Clock>,
    returning: bool,
) -> Result<(GraphVertexUpsertStats, GraphVertexMergeOutcome), UpsertFault> {
    let (merge_stats, outcome) = merge(
        transaction,
        database,
        cx,
        upsert.merge(),
        policy.merge,
        scope,
        execution,
        false,
    )
    .map_err(|error| error.map_source(GraphVertexUpsertError::Merge))?;
    let (stats, batch) = cx.with_restriction(|| {
        vertex_upsert_actions::<WriteTxnError, WriteTxnError, _>(
            upsert,
            policy,
            merge_stats,
            outcome,
            || {
                cx.checkpoint().map_err(WriteTxnError::Interrupted)?;
                execution.checkpoint()
            },
        )
    })?;
    for row in batch.rows {
        cx.checkpoint()
            .map_err(WriteTxnError::Interrupted)
            .map_err(|error| GqlQueryError::Source(GraphVertexUpsertError::Staging(error)))?;
        execution
            .checkpoint()
            .map_err(|error| GqlQueryError::Source(GraphVertexUpsertError::Staging(error)))?;
        // Every selected original field is checked, including forbidden no-ops
        // and label changes that would remove the vertex from the visible scope.
        // Unselected branch actions never execute or consume action allowance.
        stage(transaction, database, batch.relation, row, execution)
            .map_err(|error| GqlQueryError::Source(GraphVertexUpsertError::Staging(error)))?;
    }
    if returning {
        deliver(execution)
            .map_err(|error| GqlQueryError::Source(GraphVertexUpsertError::Staging(error)))?;
    }
    Ok((stats, outcome))
}

fn deliver<Clock: FnMut() -> u64>(
    execution: &mut Execution<'_, '_, Clock>,
) -> Result<(), WriteTxnError> {
    execution.checkpoint()?;
    execution
        .permit
        .charge_rows_at((execution.clock)(), 1)
        .map_err(WriteTxnError::Authorization)
}
