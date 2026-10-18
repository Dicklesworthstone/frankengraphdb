// Registered graph analytics over the same canonical overlay as GQL.
// Included inside query_source so no other overlay implementation is needed.

mod procedures {
    use super::{Database, Vfs, WriteTxn, WriteTxnError};
    use crate::gql_exec::{AdmissionUsage, source::SourceEvent};
    use fgdb_gql::algebra::{GraphValue, GraphValueRow, VertexPredicate};
    use fgdb_gql::{
        GlaExecutionStats, GqlExecutionStats, GqlQueryError, GqlQueryExecution, GqlQueryPolicy,
        PreparedProcedureCall,
    };
    use fgdb_prism::{
        FnxReadError, ParallelEdgePolicy, ProjectionBuildError, ProjectionEdge, ProjectionError,
        SelfLoopPolicy, SnapshotBinding, SnapshotGraphView,
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
            if crate::query::is_hybrid(call) {
                return self
                    .execute_hybrid_procedure_governed(database, cx, call, arguments, policy);
            }
            let spec = crate::query::bind_procedure(call, arguments)
                .map_err(|error| error.map_source(WriteTxnError::Gql))?;
            // This source admits snapshot records and work independently.
            // The immutable host's work-as-record proxy would incorrectly
            // spend a row allowance on overlay normalization and witnesses.
            // Keep the bound recipe; actual source/kernel work below still
            // consumes the original query's cumulative allowance.
            let options = spec.options_at(self.basis);
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
                self.query_source_tables::<_, VId, _>(snapshot, None, None, false, &mut |event| {
                    cx.checkpoint().map_err(GqlQueryError::Interrupted)?;
                    usage.observe(source_policy, event)
                })?;

            // Project the canonical overlay, including staged label/weight
            // edits and isolated vertices. Keep the source's complete table
            // witnesses: selecting fewer rows must not lose conflict evidence.
            let mut staging_bytes = 0usize;
            let mut vertices = Vec::new();
            for &(vertex, ref view) in &source.vertices {
                cx.checkpoint().map_err(GqlQueryError::Interrupted)?;
                if let Some(label) = options.selection.vertex_label {
                    usage.observe(policy, SourceEvent::Work)?;
                    if !view.matches(&VertexPredicate::HasLabel(label)) {
                        continue;
                    }
                }
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
            for &((eid, from, relation, to), ref view) in &source.edges {
                cx.checkpoint().map_err(GqlQueryError::Interrupted)?;
                if options.selection.relation.is_some() || options.selection.vertex_label.is_some()
                {
                    usage.observe(policy, SourceEvent::Work)?;
                    if options
                        .selection
                        .relation
                        .is_some_and(|selected| selected != relation)
                        || vertices.binary_search(&from).is_err()
                        || vertices.binary_search(&to).is_err()
                    {
                        continue;
                    }
                }
                // Discarded loops and explicit unit collapse do not observe
                // their properties, matching the immutable and scoped hosts.
                if from == to {
                    match options.projection.self_loops {
                        SelfLoopPolicy::Drop => continue,
                        SelfLoopPolicy::Reject => {
                            return Err(prism_failure(FnxReadError::Projection(
                                ProjectionError::SelfLoop(eid),
                            )));
                        }
                        SelfLoopPolicy::Keep => {}
                    }
                }
                let weight =
                    if options.projection.parallel_edges == ParallelEdgePolicy::CollapseUnit {
                        1.0
                    } else {
                        let value = options
                            .selection
                            .weight
                            .property_key()
                            .and_then(|key| view.property(key));
                        options.selection.weight.resolve(value).map_err(|reason| {
                            prism_failure(FnxReadError::Weight { edge: eid, reason })
                        })?
                    };
                usage.observe(policy, SourceEvent::ScratchEntry)?;
                crate::query::push_prism_staged(
                    &mut edges,
                    ProjectionEdge {
                        eid,
                        source: from,
                        target: to,
                        weight,
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
