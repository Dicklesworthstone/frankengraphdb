// Registered graph analytics over the same canonical overlay as GQL.
// Included inside query_source so no other overlay implementation is needed.

mod procedures {
    use super::{Database, Vfs, WriteTxn, WriteTxnError};
    use crate::gql_exec::{AdmissionUsage, source::SourceEvent};
    use fgdb_gql::algebra::{GraphValue, GraphValueRow};
    use fgdb_gql::{
        GlaExecutionStats, GqlExecutionStats, GqlQueryError, GqlQueryExecution, GqlQueryPolicy,
        PreparedProcedureCall,
    };
    use fgdb_prism::{
        FnxReadError, FnxReadOptions, ProjectionBuildError, ProjectionEdge, SnapshotBinding,
        SnapshotGraphView,
    };
    use fgdb_types::{QueryCx, VId};

    type Cancel = Box<asupersync::error::Error>;
    type Error = GqlQueryError<WriteTxnError, Cancel>;

    fn prism_failure(error: FnxReadError<crate::ReadError, Cancel>) -> Error {
        crate::query::procedure_failure(error).map_source(WriteTxnError::Gql)
    }

    impl WriteTxn {
        /// The native CALL host for this workspace. Every graph row comes
        /// from the pinned basis plus the already-normalized staged effects.
        /// Both table scan witnesses and observed element dependencies remain
        /// conservative, including empty graphs, failed calls and LIMIT 0.
        ///
        /// Only result rows and resource counters escape. Prism's internal
        /// projection digest covers the actual overlay, but its retained-root
        /// anchor is not issued as a durable snapshot/replay certificate for
        /// uncommitted data. This bounded resident adapter grants no authority.
        pub(in crate::write_txn) fn execute_procedure_governed<V: Vfs + Clone>(
            &self,
            database: &Database<V>,
            cx: &QueryCx,
            call: &PreparedProcedureCall,
            arguments: &[GraphValue],
            policy: GqlQueryPolicy,
        ) -> Result<GqlQueryExecution<GraphValueRow>, Error> {
            let snapshot = self
                .query_snapshot(database)
                .map_err(GqlQueryError::Source)?;
            cx.checkpoint().map_err(GqlQueryError::Interrupted)?;
            let spec = crate::query::bind_procedure(call, arguments)
                .map_err(|error| error.map_source(WriteTxnError::Gql))?;
            // This source admits snapshot records and work independently.
            // The immutable host's work-as-record proxy would incorrectly
            // spend a row allowance on overlay normalization and witnesses.
            let options = FnxReadOptions::whole_graph_for(&spec, Some(self.basis));
            let mut usage = AdmissionUsage::default();
            let mut source_policy = policy;
            source_policy.evaluator.max_work_units = source_policy
                .evaluator
                .max_work_units
                .min(options.source_limits.max_work_units);
            source_policy.evaluator.max_scratch_entries = source_policy
                .evaluator
                .max_scratch_entries
                .min(options.source_limits.max_scratch_entries);
            let source =
                self.query_source_tables::<_, VId>(snapshot, None, None, false, &mut |event| {
                    cx.checkpoint().map_err(GqlQueryError::Interrupted)?;
                    usage.observe(source_policy, event)
                })?;

            // Native CALL currently selects the whole graph and unit weights,
            // exactly as the Database/EmbeddedReadView hosts do. Keep isolated
            // vertices and concrete EIds; the registered projection owns the
            // directedness, self-loop and parallel-edge laws.
            let mut staging_bytes = 0usize;
            let mut vertices = Vec::new();
            for &(vertex, _) in &source.vertices {
                cx.checkpoint().map_err(GqlQueryError::Interrupted)?;
                usage.observe(policy, SourceEvent::ScratchEntry)?;
                crate::query::push_prism_staged(
                    &mut vertices,
                    vertex,
                    "vertices",
                    options.projection_limits.max_vertices,
                    &mut staging_bytes,
                    options.source_limits.max_staging_bytes,
                )
                .map_err(prism_failure)?;
            }
            let mut edges = Vec::new();
            for &((eid, from, _, to), _) in &source.edges {
                cx.checkpoint().map_err(GqlQueryError::Interrupted)?;
                usage.observe(policy, SourceEvent::ScratchEntry)?;
                crate::query::push_prism_staged(
                    &mut edges,
                    ProjectionEdge {
                        eid,
                        source: from,
                        target: to,
                        weight: 1.0,
                    },
                    "input edges",
                    options.projection_limits.max_input_edges,
                    &mut staging_bytes,
                    options.source_limits.max_staging_bytes,
                )
                .map_err(prism_failure)?;
            }
            let graph = SnapshotGraphView::build_owned_with_checkpoint(
                SnapshotBinding {
                    root: snapshot.root.0,
                    as_of: self.basis,
                },
                vertices,
                edges,
                options.projection,
                options.projection_limits,
                || {
                    cx.checkpoint().map_err(GqlQueryError::Interrupted)?;
                    usage.observe(policy, SourceEvent::Work)
                },
            )
            .map_err(|error| match error {
                ProjectionBuildError::Cancelled(error) => error,
                ProjectionBuildError::Projection(error) => {
                    prism_failure(FnxReadError::Projection(error))
                }
            })?;

            let remaining = usage.remaining(policy);
            let mut limits = options.execution_limits;
            limits.max_estimated_work = limits
                .max_estimated_work
                .min(usize::try_from(remaining.evaluator.max_work_units).unwrap_or(usize::MAX));
            // Each kernel row will need one row plus one entry per output
            // cell when converted. Admit that output before the kernel can
            // allocate it, independently of the outer query's final row page.
            limits.max_result_rows = limits.max_result_rows.min(
                usize::try_from(remaining.evaluator.max_scratch_entries).unwrap_or(usize::MAX)
                    / (spec.outputs().len() + 1),
            );
            let analytics = spec
                .execute(&graph, limits, || cx.checkpoint())
                .map_err(|error| prism_failure(FnxReadError::Execution(error)))?;
            usage.charge_work(
                policy,
                u64::try_from(analytics.certificate.estimated_work).unwrap_or(u64::MAX),
            )?;
            for row in &analytics.rows {
                cx.checkpoint().map_err(GqlQueryError::Interrupted)?;
                usage.observe(policy, SourceEvent::ScratchEntry)?;
                for _ in row {
                    cx.checkpoint().map_err(GqlQueryError::Interrupted)?;
                    usage.observe(policy, SourceEvent::ScratchEntry)?;
                }
            }
            let rows = crate::query::procedure_rows(analytics.rows)
                .map_err(|error| error.map_source(WriteTxnError::Gql))?;
            cx.checkpoint().map_err(GqlQueryError::Interrupted)?;
            usage.finish(
                policy,
                Ok(GqlQueryExecution {
                    rows: GqlExecutionStats {
                        snapshot_records: usage.snapshot_records(),
                        result_rows: rows.len() as u64,
                    },
                    evaluator: GlaExecutionStats::default(),
                    value: rows,
                }),
            )
        }
    }
}
