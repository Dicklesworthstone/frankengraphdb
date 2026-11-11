//! Host representation admission over the final unpublished authorized overlay.

use super::{Database, Execution, Vfs, WriteTxn, WriteTxnError, selection};
use fgdb_delta_types::ElementId;
use fgdb_gql::algebra::{GraphValue, GraphValueRow};
use fgdb_gql::{
    GlaExecutionEvent, GlaLimitDimension, GlaLimitExceeded, GqlQueryExecution, GqlQueryPolicy,
    GraphElementMetadata, GraphElementMetadataError, ReverseSymbolCatalog,
};
use fgdb_types::QueryCx;
use fgdb_warden::PlannerPredicates;
use std::cell::{Cell, RefCell};

/// Read-only output admission for a capability-authorized write's final overlay.
///
/// The host receives this view only inside a checked output callback, before
/// completion can publish the write. It exposes neither the transaction nor
/// unchecked graph rows. Metadata uses the same live capability, endpoint and
/// field masking, conflict witnesses, cancellation and cumulative native
/// work/scratch allowance as the statement. Returned values must remain private
/// until the enclosing operation completes successfully.
///
/// A hidden or absent identity returns None; an identity retired earlier in
/// this statement returns WriteTxnError::DeletedElementRead. Complete visible
/// fields require the trusted host's reverse catalog, exactly as native
/// properties(), labels() and type() do. No catalog name grants authority.
pub trait AuthorizedWriteOutput {
    fn metadata(
        &mut self,
        element: ElementId,
        kind: GraphElementMetadata,
        catalog: &ReverseSymbolCatalog,
    ) -> Result<Option<GraphValue>, WriteTxnError>;
}

struct MetadataReader<F> {
    read: F,
}

impl<F> AuthorizedWriteOutput for MetadataReader<F>
where
    F: FnMut(
        ElementId,
        GraphElementMetadata,
        &ReverseSymbolCatalog,
    ) -> Result<Option<GraphValue>, WriteTxnError>,
{
    fn metadata(
        &mut self,
        element: ElementId,
        kind: GraphElementMetadata,
        catalog: &ReverseSymbolCatalog,
    ) -> Result<Option<GraphValue>, WriteTxnError> {
        (self.read)(element, kind, catalog)
    }
}

/// All returning owners enter this boundary only after staging their final
/// effects. Ordinary no-op admission creates no additional read or debit.
#[allow(clippy::too_many_arguments)]
pub(super) fn admit_output<V: Vfs + Clone, Clock: FnMut() -> u64>(
    transaction: &WriteTxn,
    database: &Database<V>,
    cx: &QueryCx,
    scope: &PlannerPredicates,
    execution: &mut Execution<'_, '_, Clock>,
    policy: GqlQueryPolicy,
    result: &mut GqlQueryExecution<GraphValueRow>,
    admit: impl FnOnce(
        &[GraphValueRow],
        &mut dyn FnMut(u64) -> Result<(), WriteTxnError>,
        &mut dyn AuthorizedWriteOutput,
    ) -> Result<(), WriteTxnError>,
) -> Result<(), WriteTxnError> {
    cx.with_restriction(|| {
        let controls = RefCell::new(execution);
        let failed = Cell::new(false);
        let rows = &result.value;
        let evaluator = &mut result.evaluator;
        let mut reader = MetadataReader {
            read: |element, kind, catalog: &ReverseSymbolCatalog| {
                if failed.get() {
                    return Err(WriteTxnError::AuthorizedMutationRefused);
                }
                let mut failure = None;
                let metadata = selection::returning_element_metadata(
                    transaction,
                    database,
                    cx,
                    element,
                    kind,
                    catalog,
                    scope,
                    &controls,
                    &mut |event| {
                        let work = u128::from(evaluator.work_units) + 1;
                        let scratch = u128::from(evaluator.scratch_entries)
                            + u128::from(matches!(event, GlaExecutionEvent::ScratchEntry));
                        for (observed, limit, dimension) in [
                            (
                                work,
                                policy.evaluator.max_work_units,
                                GlaLimitDimension::WorkUnits,
                            ),
                            (
                                scratch,
                                policy.evaluator.max_scratch_entries,
                                GlaLimitDimension::ScratchEntries,
                            ),
                        ] {
                            if observed > u128::from(limit) {
                                failure = Some(WriteTxnError::OutputLimit(GlaLimitExceeded {
                                    dimension,
                                    limit,
                                    observed,
                                }));
                                return Err(());
                            }
                        }
                        evaluator.work_units = work as u64;
                        evaluator.scratch_entries = scratch as u64;
                        Ok(())
                    },
                );
                let metadata = match metadata {
                    Ok(value) => Ok(value),
                    Err(GraphElementMetadataError::Source(error)) => Err(error),
                    Err(GraphElementMetadataError::Interrupted) => {
                        Err(failure.expect("metadata control retains its refusal"))
                    }
                };
                if metadata.is_err() {
                    failed.set(true);
                }
                metadata
            },
        };
        admit(
            rows,
            &mut |work| {
                if failed.get() {
                    return Err(WriteTxnError::AuthorizedMutationRefused);
                }
                let charged = (|| {
                    cx.checkpoint().map_err(WriteTxnError::Interrupted)?;
                    controls.borrow_mut().work(work)
                })();
                if charged.is_err() {
                    failed.set(true);
                }
                charged
            },
            &mut reader,
        )?;
        // An adapter must not swallow a failed graph read or work debit and publish its
        // effects with a partial representation. Preserve the original error
        // when the callback propagates it; otherwise still abort the write.
        if failed.get() {
            return Err(WriteTxnError::AuthorizedMutationRefused);
        }
        Ok(())
    })
}
