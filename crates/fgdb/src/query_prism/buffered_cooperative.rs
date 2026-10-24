//! Cooperative compressed kernels over the same admitted cold-read generation.

use super::{BufferedReadError, BufferedReadView};
use asupersync::fs::Vfs;
use fgdb_prism::{
    FnxCallSpec, FnxMemoryLimits, FnxParameters, FnxReadError, FnxReadOptions, FnxReadResult,
    FnxSealedExecutionError, FnxSealedReadError,
};
use fgdb_strata::tiered::sealed::SealedLimits;
use fgdb_types::QueryCx;
use std::num::NonZeroUsize;

type Cancel = Box<asupersync::error::Error>;

impl<V: Vfs + Clone> BufferedReadView<V> {
    /// Bind and cooperatively execute a registered compressed procedure over
    /// this buffered generation. Invalid arguments and unsupported cooperative
    /// algorithms refuse before vertex selection or relation payload reads.
    #[allow(clippy::too_many_arguments)]
    pub async fn call_fnx_sealed_cooperative(
        &mut self,
        cx: &QueryCx,
        text: &str,
        parameters: &FnxParameters,
        options: FnxReadOptions,
        memory: FnxMemoryLimits,
        sealing: SealedLimits,
        max_source_incidences: usize,
        quantum: NonZeroUsize,
    ) -> Result<FnxReadResult, FnxSealedReadError<BufferedReadError, Cancel>> {
        let call = FnxCallSpec::bind(text, parameters).map_err(FnxReadError::Bind)?;
        self.execute_fnx_sealed_cooperative(
            cx,
            &call,
            options,
            memory,
            sealing,
            max_source_incidences,
            quantum,
        )
        .await
    }

    /// Execute with the runtime's yield primitive between bounded compressed
    /// traversal and scalar steps. The projection is prepared by the same
    /// pinned-source path as `execute_fnx_sealed`; no writer, resident snapshot,
    /// or decoded-adjacency fallback is opened.
    ///
    /// Source frames remain in the view's pool. Selected vertex staging,
    /// compressed image construction, kernel workspace and output retain their
    /// respective explicit limits. Cancellation or a dropped future cannot
    /// publish partial analytics. The view remains usable afterward.
    ///
    /// Projection and incoming-index preparation also yield. Source admission,
    /// compaction and encoding still contain synchronous CPU stages. The quantum
    /// bounds cooperative construction/kernel steps, not the
    /// complete call's poll duration, and does not make its workspace spillable.
    /// This is an owner-level API, not a capability-authorized procedure host.
    /// The cooperative kernel future is not `Send`; use a registered
    /// current-thread runtime, as with the resident cooperative entry point.
    #[allow(clippy::too_many_arguments)]
    pub async fn execute_fnx_sealed_cooperative(
        &mut self,
        cx: &QueryCx,
        call: &FnxCallSpec,
        options: FnxReadOptions,
        memory: FnxMemoryLimits,
        sealing: SealedLimits,
        max_source_incidences: usize,
        quantum: NonZeroUsize,
    ) -> Result<FnxReadResult, FnxSealedReadError<BufferedReadError, Cancel>> {
        cx.with_restriction_async(async {
            cx.checkpoint().map_err(FnxReadError::Cancelled)?;
            if !call.supports_cooperative_sealed_execution() {
                return Err(FnxSealedReadError::Execution(
                    FnxSealedExecutionError::UnsupportedCooperativeAlgorithm(call.algorithm()),
                ));
            }
            call.validate_sealed_projection(options.projection.directedness)
                .map_err(FnxSealedReadError::Execution)?;
            let graph = self
                .prism_sealed_projection_cooperative_at(
                    cx,
                    options.as_of.unwrap_or(self.frontier()),
                    options.selection,
                    options.projection,
                    options.projection_limits,
                    options.source_limits,
                    sealing,
                    max_source_incidences,
                    quantum,
                )
                .await?;
            let analytics = call
                .execute_sealed_cooperative(
                    cx,
                    &graph,
                    options.execution_limits,
                    memory,
                    quantum,
                    asupersync::runtime::yield_now::yield_now,
                )
                .await
                .map_err(FnxSealedReadError::Execution)?;
            Ok(FnxReadResult::bind_selection(analytics, options.selection))
        })
        .await
    }
}
