// Query-selected graph creation stages one ordinary WriteBatch. Selection and
// computed payloads are frozen before allocation; commit remains explicit.

struct InsertGraphSource<'a, V: Vfs> {
    transaction: &'a WriteTxn,
    database: &'a Database<V>,
    cx: &'a fgdb_types::QueryCx,
}

impl<V: Vfs + Clone> fgdb_gql::GraphSetSource<WriteTxnError, Box<asupersync::error::Error>>
    for InsertGraphSource<'_, V>
{
    fn supports_pattern_factors(&self) -> bool {
        true
    }

    fn pattern(
        &mut self,
        pattern: &fgdb_gql::algebra::PreparedGraphPattern<fgdb_gql::algebra::GraphValueRow>,
        policy: fgdb_gql::GqlQueryPolicy,
    ) -> Result<
        fgdb_gql::GqlQueryExecution<fgdb_gql::algebra::GraphValueRow>,
        TxnGqlError<WriteTxnError>,
    > {
        self.transaction
            .execute_graph_pattern_governed(self.database, self.cx, pattern, policy)
    }

    fn pattern_factors(
        &mut self,
        request: &fgdb_gql::PreparedGraphPatternFactors<'_>,
        policy: fgdb_gql::GqlQueryPolicy,
    ) -> Option<Result<fgdb_gql::GraphPatternFactorExecution, TxnGqlError<WriteTxnError>>> {
        Some(self.transaction.execute_graph_pattern_factors_governed(
            self.database,
            self.cx,
            request,
            policy,
        ))
    }
}

impl WriteTxn {
    /// Stage CREATE/INSERT and its native RETURN rows as one atomic operation.
    /// The prepared query evaluates its complete selection, creation payloads,
    /// identity bindings, projection, DISTINCT, ordering and page before any
    /// proposal reaches storage. Result expressions read the frozen creation
    /// bindings; they never rescan the graph or observe another occurrence.
    ///
    /// RETURN row limits govern the final rows independently of vertex/edge
    /// creation limits. In particular LIMIT 0 does not suppress the writes or
    /// hide a failing result expression. The returned execution counters already
    /// include the creation phase; do not add the insertion counters to them.
    ///
    /// Failure, cancellation and unwinding preserve the transaction's earlier
    /// staged effects and retain observed read dependencies. Issued identities
    /// are not reclaimed. Success is transaction-local until finish/commit.
    pub fn execute_graph_insert_query_governed<V: Vfs + Clone, A>(
        &mut self,
        database: &mut Database<V>,
        cx: &fgdb_types::QueryCx,
        query: &fgdb_gql::PreparedGraphInsertQuery,
        policy: fgdb_gql::insertion::GraphInsertPolicy,
        allocate: impl FnMut(fgdb_gql::insertion::GraphInsertRequest) -> Result<ElementId, A>,
    ) -> InsertQueryResult<A> {
        use fgdb_gql::insertion::GraphInsertError;
        use fgdb_gql::{GqlQueryError, GraphInsertQueryError};
        let source = |error| {
            GqlQueryError::Source(GraphInsertQueryError::Insertion(GraphInsertError::Source(
                error,
            )))
        };
        self.ensure_database(database).map_err(source)?;
        let live = database
            .frontier()
            .map_err(WriteTxnError::from)
            .map_err(source)?;
        if live != self.basis {
            return Err(source(WriteTxnError::SnapshotAdvanced {
                pinned: self.basis,
                live,
            }));
        }
        cx.with_restriction(|| {
            let workspace = MutationProgramWorkspace::new(self);
            let proposal = query.execute_governed_with_source(
                policy,
                InsertGraphSource {
                    transaction: workspace.txn,
                    database,
                    cx,
                },
                allocate,
                || cx.checkpoint(),
            )?;
            let (insertion, returning) = proposal.into_parts();
            let (stats, _, _) = workspace
                .txn
                .stage_graph_insert_batch(
                    database,
                    cx,
                    query.insertion().relation(),
                    insertion,
                    false,
                )
                .map_err(|error| error.map_source(GraphInsertQueryError::Insertion))?;
            // Every result admission and cancellation check precedes the sole
            // atomic staging boundary; nothing fallible follows acceptance.
            workspace.accept();
            Ok((stats, returning))
        })
    }

    /// CREATE/INSERT RETURN with identities reserved by the owning database.
    /// The result has the same transaction-local lifetime and row allowances as
    /// `execute_graph_insert_query_governed`; no caller allocator is accepted.
    pub fn execute_graph_insert_query_engine_governed<V: Vfs + Clone>(
        &mut self,
        database: &mut Database<V>,
        cx: &fgdb_types::QueryCx,
        query: &fgdb_gql::PreparedGraphInsertQuery,
        policy: fgdb_gql::insertion::GraphInsertPolicy,
    ) -> InsertQueryResult<WriteTxnError> {
        use fgdb_gql::insertion::GraphInsertError;
        use fgdb_gql::{GqlQueryError, GraphInsertQueryError};
        let source = |error| {
            GqlQueryError::Source(GraphInsertQueryError::Insertion(GraphInsertError::Source(
                error,
            )))
        };
        self.ensure_database(database).map_err(source)?;
        let allocate = database.engine_allocator(cx).map_err(source)?;
        self.execute_graph_insert_query_governed(database, cx, query, policy, allocate)
    }

    /// Stage a chained write (`CREATE (a) WITH a CREATE (b) [RETURN ...]`,
    /// fgdb-cpny9) as one atomic operation. Each step stages its creation
    /// before the next step reads the rows it returned, so a later write sees
    /// the earlier ones; every step runs in this one workspace. The steps share
    /// `policy` cumulatively: source records, work, scratch and created
    /// vertices/edges count across the whole statement, and only the last
    /// step's RETURN rows meet the result-row allowance (earlier rows are the
    /// statement's private input to the next write).
    ///
    /// Failure, cancellation and unwinding restore the staged prefix the
    /// statement found, as for every write program. Issued identities are not
    /// reclaimed. A chain without RETURN returns zero rows.
    ///
    /// A relationship MERGE step resolves every distinct endpoint pair of the
    /// previous write's rows as a native MERGE program does; its creations
    /// count against the statement's edge allowance. A MERGE with RETURN adds
    /// no step receipt: such a statement answers with its rows.
    pub fn execute_graph_write_chain_governed<V: Vfs + Clone, A>(
        &mut self,
        database: &mut Database<V>,
        cx: &fgdb_types::QueryCx,
        chain: &fgdb_gql::PreparedGraphWriteChain,
        policy: fgdb_gql::insertion::GraphInsertPolicy,
        mut allocate: impl FnMut(fgdb_gql::insertion::GraphInsertRequest) -> Result<ElementId, A>,
    ) -> WriteChainResult<A> {
        use fgdb_gql::insertion::{GraphInsertError, GraphInsertPolicy, GraphInsertStats};
        use fgdb_gql::{
            GlaExecutionEvent, GlaExecutionStats, GqlExecutionStats, GqlQueryError,
            GqlQueryExecution, GqlQueryPolicy, GraphEdgeMergePolicy, GraphEdgeMergeQueryError,
            GraphEdgeMergeStats, GraphInsertQueryError, GraphVertexMergePolicy,
            GraphVertexMergeRowsQueryError, GraphWriteChainError, GraphWriteChainStep,
            GraphWriteProgramReceipt, GraphWriteProgramStats, GraphWriteStepReceipt,
        };
        let source = |error| {
            GqlQueryError::Source(GraphWriteChainError::Insert(
                GraphInsertQueryError::Insertion(GraphInsertError::Source(error)),
            ))
        };
        let insertion = |error: GraphInsertError<WriteTxnError, A>| {
            GraphWriteChainError::Insert(GraphInsertQueryError::Insertion(error))
        };
        // A MERGE step's counters in the statement's cumulative form.
        let merged = |stats: &GraphEdgeMergeStats| GraphInsertStats {
            selection: stats.match_selection,
            evaluator: stats.evaluator,
            created_vertices: 0,
            created_edges: stats.created_edges,
        };
        self.ensure_database(database).map_err(source)?;
        let live = database
            .frontier()
            .map_err(WriteTxnError::from)
            .map_err(source)?;
        if live != self.basis {
            return Err(source(WriteTxnError::SnapshotAdvanced {
                pinned: self.basis,
                live,
            }));
        }
        // What the statement has left after the steps so far.
        let remaining = |used: &GraphInsertStats, delivers: bool| {
            let records = policy
                .query
                .rows
                .max_snapshot_records()
                .map_or(u64::MAX, |limit| {
                    limit.saturating_sub(used.selection.snapshot_records)
                });
            let rows = if delivers {
                policy.query.rows.max_result_rows().unwrap_or(u64::MAX)
            } else {
                u64::MAX
            };
            GraphInsertPolicy::new(
                GqlQueryPolicy::new(
                    records,
                    rows,
                    policy
                        .query
                        .evaluator
                        .max_work_units
                        .saturating_sub(used.evaluator.work_units),
                    policy
                        .query
                        .evaluator
                        .max_scratch_entries
                        .saturating_sub(used.evaluator.scratch_entries),
                ),
                policy.max_vertices.saturating_sub(used.created_vertices),
                policy.max_edges.saturating_sub(used.created_edges),
            )
        };
        let add = |used: &mut GraphInsertStats, step: &GraphInsertStats| {
            used.selection.snapshot_records = used
                .selection
                .snapshot_records
                .saturating_add(step.selection.snapshot_records);
            used.evaluator.work_units = used
                .evaluator
                .work_units
                .saturating_add(step.evaluator.work_units);
            used.evaluator.scratch_entries = used
                .evaluator
                .scratch_entries
                .saturating_add(step.evaluator.scratch_entries);
            used.created_vertices = used.created_vertices.saturating_add(step.created_vertices);
            used.created_edges = used.created_edges.saturating_add(step.created_edges);
        };
        cx.with_restriction(|| {
            let workspace = MutationProgramWorkspace::new(self);
            // A chain is an ordered program whose steps may write different
            // relation types (`MERGE (a)-[:X]->(b) CREATE (b)-[:Y]->(c)`), each
            // seeing its predecessor, as a mixed write program's do.
            workspace.txn.program_multi_relation = true;
            let mut used = GraphInsertStats {
                selection: GqlExecutionStats {
                    snapshot_records: 0,
                    result_rows: 0,
                },
                evaluator: GlaExecutionStats::default(),
                created_vertices: 0,
                created_edges: 0,
            };
            let mut rows = None;
            let mut result = None;
            let mut receipts = Vec::with_capacity(chain.steps().len());
            let last = chain.steps().len().saturating_sub(1);
            for (at, step) in chain.steps().iter().enumerate() {
                // Only a last step with RETURN delivers rows. A last write
                // without one (`CREATE (a) WITH a CREATE (b)`) selects the
                // statement's private input, as every earlier step does.
                let delivers = at == last
                    && matches!(
                        step,
                        GraphWriteChainStep::Query(_)
                            | GraphWriteChainStep::EdgeMergeQuery(_)
                            | GraphWriteChainStep::VertexMergeQuery(_)
                    );
                let allowance = remaining(&used, delivers);
                // A step without RETURN rows: its own counters, no rows.
                let unreturned = |stats: &GraphInsertStats| GqlQueryExecution {
                    value: Vec::new(),
                    rows: GqlExecutionStats {
                        snapshot_records: stats.selection.snapshot_records,
                        result_rows: 0,
                    },
                    evaluator: stats.evaluator,
                };
                let edge_merge = GraphEdgeMergePolicy::new(allowance.query)
                    .with_creation_limit(allowance.max_edges);
                let (stats, returning) = match step {
                    GraphWriteChainStep::Query(query) => {
                        let proposal = query
                            .execute_governed_over(
                                rows.take(),
                                allowance,
                                // Typed: a closure behind `impl GraphSetSource`
                                // gets no signature from the bound.
                                |pattern: &fgdb_gql::algebra::PreparedGraphPattern<
                                    fgdb_gql::algebra::GraphValueRow,
                                >,
                                 allowance: fgdb_gql::GqlQueryPolicy| {
                                    workspace.txn.execute_graph_pattern_governed(
                                        database, cx, pattern, allowance,
                                    )
                                },
                                &mut allocate,
                                || cx.checkpoint(),
                            )
                            .map_err(|error| error.map_source(GraphWriteChainError::Insert))?;
                        let (insertion_batch, returning) = proposal.into_parts();
                        let (stats, vertices, edges) = workspace
                            .txn
                            .stage_graph_insert_batch(
                                database,
                                cx,
                                query.insertion().relation(),
                                insertion_batch,
                                true,
                            )
                            .map_err(|error| error.map_source(insertion))?;
                        receipts.push(GraphWriteStepReceipt::Insert { vertices, edges });
                        (stats, returning)
                    }
                    GraphWriteChainStep::Insert(prepared) => {
                        let proposal = prepared
                            .execute_governed_over(
                                rows.take(),
                                allowance,
                                // Typed: a closure behind `impl GraphSetSource`
                                // gets no signature from the bound.
                                |pattern: &fgdb_gql::algebra::PreparedGraphPattern<
                                    fgdb_gql::algebra::GraphValueRow,
                                >,
                                 allowance: fgdb_gql::GqlQueryPolicy| {
                                    workspace.txn.execute_graph_pattern_governed(
                                        database, cx, pattern, allowance,
                                    )
                                },
                                &mut allocate,
                                || cx.checkpoint(),
                            )
                            .map_err(|error| error.map_source(insertion))?;
                        let (stats, vertices, edges) = workspace
                            .txn
                            .stage_graph_insert_batch(
                                database,
                                cx,
                                prepared.relation(),
                                proposal,
                                true,
                            )
                            .map_err(|error| error.map_source(insertion))?;
                        receipts.push(GraphWriteStepReceipt::Insert { vertices, edges });
                        (stats, unreturned(&stats))
                    }
                    GraphWriteChainStep::EdgeMerge(merge) => {
                        let (stats, receipt) = workspace
                            .txn
                            .execute_graph_edge_merges_over(
                                database,
                                cx,
                                merge,
                                edge_merge,
                                rows.take(),
                                &mut allocate,
                            )
                            .map_err(|error| {
                                error.map_source(|error| {
                                    GraphWriteChainError::EdgeMerge(
                                        GraphEdgeMergeQueryError::Merge(error),
                                    )
                                })
                            })?;
                        receipts.push(receipt);
                        let stats = merged(&stats);
                        (stats, unreturned(&stats))
                    }
                    GraphWriteChainStep::VertexMerge(merge) => {
                        let (mut stats, receipt, input, pairs) = workspace
                            .txn
                            .execute_graph_vertex_merges_over(
                                database,
                                cx,
                                merge,
                                GraphVertexMergePolicy::new(allowance.query)
                                    .with_creation_limit(allowance.max_vertices),
                                rows.take(),
                                &mut allocate,
                            )
                            .map_err(|error| {
                                error.map_source(|error| {
                                    GraphWriteChainError::VertexMerge(
                                        GraphVertexMergeRowsQueryError::Merge(error),
                                    )
                                })
                            })?;
                        receipts.push(receipt);
                        // Multiplicity can repeat a large carried value for
                        // every matching vertex. Admit each complete copy
                        // against this step's remaining allowance, before it
                        // grows the private continuation buffer.
                        let mut output = Vec::new();
                        for (at, vertex) in pairs {
                            output.push(step.continuation_row_controlled(
                                &input[at],
                                ElementId::Vertex(vertex),
                                &mut |event| {
                                    vertex_targets_event::<GraphWriteChainError<WriteTxnError, A>>(
                                        cx,
                                        allowance.query,
                                        &mut stats.evaluator,
                                        event,
                                    )
                                },
                            )?);
                        }
                        let stats = GraphInsertStats {
                            selection: stats.match_selection,
                            evaluator: stats.evaluator,
                            created_vertices: stats.created_vertices,
                            created_edges: 0,
                        };
                        // Its rows are the next write's input, never results.
                        let returning = GqlQueryExecution {
                            value: output,
                            rows: GqlExecutionStats {
                                snapshot_records: stats.selection.snapshot_records,
                                result_rows: 0,
                            },
                            evaluator: stats.evaluator,
                        };
                        (stats, returning)
                    }
                    GraphWriteChainStep::EdgeMergeRows { merge, .. } => {
                        let (mut stats, resolved, selected) = workspace
                            .txn
                            .execute_graph_edge_merge_output_governed::<EdgeMergeRows, V, A>(
                                database,
                                cx,
                                merge,
                                edge_merge,
                                rows.take(),
                                |row| {
                                    allocate(fgdb_gql::insertion::GraphInsertRequest::Edge {
                                        row,
                                        edge: 0,
                                    })
                                },
                            )
                            .map_err(|error| {
                                error.map_source(|error| {
                                    GraphWriteChainError::EdgeMerge(
                                        GraphEdgeMergeQueryError::Merge(error),
                                    )
                                })
                            })?;
                        let (source_column, destination_column) =
                            (merge.source_column(), merge.destination_column());
                        // Each row continues once per relationship its pair
                        // resolved to, in row order and then identity order.
                        let mut output = Vec::new();
                        for row in &selected {
                            vertex_targets_event::<GraphWriteChainError<WriteTxnError, A>>(
                                cx,
                                allowance.query,
                                &mut stats.evaluator,
                                GlaExecutionEvent::Work,
                            )?;
                            let values = row.values();
                            let (Some(source), Some(destination)) = (
                                values[source_column].as_vertex(),
                                values[destination_column].as_vertex(),
                            ) else {
                                continue;
                            };
                            let Some(edges) = resolved.edges.get(&(source, destination)) else {
                                continue;
                            };
                            for &(edge, _) in edges {
                                output.push(
                                    step.continuation_row_controlled(
                                        row,
                                        ElementId::Edge(edge),
                                        &mut |event| {
                                            vertex_targets_event::<
                                                GraphWriteChainError<WriteTxnError, A>,
                                            >(
                                                cx, allowance.query, &mut stats.evaluator, event
                                            )
                                        },
                                    )?,
                                );
                            }
                        }
                        receipts.push(GraphWriteStepReceipt::EdgeMergeBatch {
                            outcomes: resolved.outcomes,
                            created: resolved.created,
                        });
                        let stats = merged(&stats);
                        let returning = GqlQueryExecution {
                            value: output,
                            rows: GqlExecutionStats {
                                snapshot_records: stats.selection.snapshot_records,
                                result_rows: 0,
                            },
                            evaluator: stats.evaluator,
                        };
                        (stats, returning)
                    }
                    GraphWriteChainStep::VertexMergeQuery(query) => {
                        let merge = query.merge();
                        let chain = |error| {
                            GqlQueryError::Source(GraphWriteChainError::VertexMerge(
                                GraphVertexMergeRowsQueryError::Merge(
                                    fgdb_gql::GraphVertexMergeError::Source(error),
                                ),
                            ))
                        };
                        let (mut stats, receipt, input, pairs) = workspace
                            .txn
                            .execute_graph_vertex_merges_over(
                                database,
                                cx,
                                merge,
                                GraphVertexMergePolicy::new(allowance.query)
                                    .with_creation_limit(allowance.max_vertices),
                                rows.take(),
                                &mut allocate,
                            )
                            .map_err(|error| {
                                error.map_source(|error| {
                                    GraphWriteChainError::VertexMerge(
                                        GraphVertexMergeRowsQueryError::Merge(error),
                                    )
                                })
                            })?;
                        receipts.push(receipt);
                        // RETURN's input: each bound column of each pair,
                        // properties read after the statement: the merged
                        // vertex's, and a carried element's (ON/SET effects
                        // included). A NULL column reads NULL.
                        let mut read = |element: ElementId,
                                        key|
                         -> Result<
                            Option<fgdb_types::CanonicalPropertyValue>,
                            TxnGqlError<GraphWriteChainError<WriteTxnError, A>>,
                        > {
                            workspace
                                .txn
                                .merge_return_property(database, element, key)
                                .map_err(chain)
                        };
                        let mut returned = Vec::new();
                        for (at, vertex) in pairs {
                            returned.push(step.vertex_return_row_controlled(
                                &input[at],
                                vertex,
                                &mut read,
                                &mut |event| {
                                    vertex_targets_event::<GraphWriteChainError<WriteTxnError, A>>(
                                        cx,
                                        allowance.query,
                                        &mut stats.evaluator,
                                        event,
                                    )
                                },
                            )?);
                        }
                        let stats = GraphInsertStats {
                            selection: stats.match_selection,
                            evaluator: stats.evaluator,
                            created_vertices: stats.created_vertices,
                            created_edges: 0,
                        };
                        let returning = query
                            .finish_governed(
                                returned,
                                stats.selection,
                                stats.evaluator,
                                allowance.query,
                                || cx.checkpoint(),
                            )
                            .map_err(|error| {
                                error.map_source(|error| {
                                    GraphWriteChainError::VertexMerge(
                                        GraphVertexMergeRowsQueryError::Returning(error),
                                    )
                                })
                            })?;
                        (stats, returning)
                    }
                    GraphWriteChainStep::EdgeMergeQuery(query) => {
                        let (stats, returning) = workspace
                            .txn
                            .execute_graph_edge_merge_query_over(
                                database,
                                cx,
                                query,
                                edge_merge,
                                rows.take(),
                                &mut allocate,
                            )
                            .map_err(|error| error.map_source(GraphWriteChainError::EdgeMerge))?;
                        (merged(&stats), returning)
                    }
                };
                // A query step's execution includes its write prefix PLUS
                // projection, grouping, ordering and WITH-tail evaluation.
                // The write stats alone stop before that work. Carry the
                // complete counters into the next allowance and the final
                // receipt; do not add the prefix a second time. Non-query
                // steps supply the same counters through `unreturned`.
                let complete = GraphInsertStats {
                    selection: returning.rows,
                    evaluator: returning.evaluator,
                    ..stats
                };
                add(&mut used, &complete);
                if at == last {
                    result = Some(returning);
                } else {
                    rows = Some(returning.value);
                }
            }
            // Only the gql compiler builds a chain, always with two or more
            // steps; an empty one would have nothing to stage.
            let Some(mut returning) = result else {
                return Err(source(WriteTxnError::NoPreparedWrite));
            };
            // Whole-statement counters, as every write RETURN reports them.
            returning.rows.snapshot_records = used.selection.snapshot_records;
            returning.evaluator = used.evaluator;
            used.selection.result_rows = returning.rows.result_rows;
            let receipt = GraphWriteProgramReceipt::new(
                GraphWriteProgramStats {
                    completed_statements: 1,
                    selection: used.selection,
                    evaluator: used.evaluator,
                    target_vertex_visits: 0,
                    target_edge_visits: 0,
                    mutation_effects: 0,
                    created_vertices: used.created_vertices,
                    created_edges: used.created_edges,
                },
                receipts,
            );
            // Every step's work and cancellation check precedes acceptance.
            workspace.accept();
            Ok((used, returning, receipt))
        })
    }

    /// A chained write with identities reserved by the owning database.
    pub fn execute_graph_write_chain_engine_governed<V: Vfs + Clone>(
        &mut self,
        database: &mut Database<V>,
        cx: &fgdb_types::QueryCx,
        chain: &fgdb_gql::PreparedGraphWriteChain,
        policy: fgdb_gql::insertion::GraphInsertPolicy,
    ) -> WriteChainResult<WriteTxnError> {
        use fgdb_gql::insertion::GraphInsertError;
        use fgdb_gql::{GqlQueryError, GraphInsertQueryError, GraphWriteChainError};
        let source = |error| {
            GqlQueryError::Source(GraphWriteChainError::Insert(
                GraphInsertQueryError::Insertion(GraphInsertError::Source(error)),
            ))
        };
        self.ensure_database(database).map_err(source)?;
        let allocate = database.engine_allocator(cx).map_err(source)?;
        self.execute_graph_write_chain_governed(database, cx, chain, policy, allocate)
    }

    /// INSERT/CREATE with identities reserved by the owning database.
    pub fn execute_graph_insert_engine_governed<V: Vfs + Clone>(
        &mut self,
        database: &mut Database<V>,
        cx: &fgdb_types::QueryCx,
        insertion: &fgdb_gql::insertion::PreparedGraphInsert,
        policy: fgdb_gql::insertion::GraphInsertPolicy,
    ) -> Result<
        fgdb_gql::insertion::GraphInsertStats,
        TxnGqlError<fgdb_gql::insertion::GraphInsertError<WriteTxnError, WriteTxnError>>,
    > {
        let source = |error| {
            fgdb_gql::GqlQueryError::Source(fgdb_gql::insertion::GraphInsertError::Source(error))
        };
        self.ensure_database(database).map_err(source)?;
        let allocate = database.engine_allocator(cx).map_err(source)?;
        self.execute_graph_insert_governed(database, cx, insertion, policy, allocate)
    }

    /// INSERT/CREATE with an engine-issued identity receipt; publication remains explicit.
    pub fn execute_graph_insert_returning_engine_governed<V: Vfs + Clone>(
        &mut self,
        database: &mut Database<V>,
        cx: &fgdb_types::QueryCx,
        insertion: &fgdb_gql::insertion::PreparedGraphInsert,
        policy: fgdb_gql::insertion::GraphInsertPolicy,
    ) -> Result<
        WithAffectedIds<fgdb_gql::insertion::GraphInsertStats>,
        TxnGqlError<fgdb_gql::insertion::GraphInsertError<WriteTxnError, WriteTxnError>>,
    > {
        let source = |error| {
            fgdb_gql::GqlQueryError::Source(fgdb_gql::insertion::GraphInsertError::Source(error))
        };
        self.ensure_database(database).map_err(source)?;
        let allocate = database.engine_allocator(cx).map_err(source)?;
        self.execute_graph_insert_returning_governed(database, cx, insertion, policy, allocate)
    }

    /// Create graph structures once per selected occurrence in this workspace.
    /// Matched endpoints and property reads use the canonical staged overlay.
    /// Newly created vertices are available as endpoints within their own row.
    ///
    /// The allocator supplies fresh typed IDs under the caller's identity
    /// policy. This API never rolls back an allocator or recycles issued IDs;
    /// retain its request-to-ID mapping for deterministic retries/replay. All
    /// database writes still pass ordinary creation/non-revival validation.
    ///
    /// Matching, property, identity, resource and preparation failures leave
    /// earlier staged work intact. Observed read/scan dependencies remain even
    /// on failure. Success only stages; finish/commit publishes later. Counts
    /// describe proposed creations, not a durable acknowledgment. No cancellation
    /// checkpoint follows the synchronous atomic staging boundary.
    pub fn execute_graph_insert_governed<V: Vfs + Clone, A>(
        &mut self,
        database: &mut Database<V>,
        cx: &fgdb_types::QueryCx,
        insertion: &fgdb_gql::insertion::PreparedGraphInsert,
        policy: fgdb_gql::insertion::GraphInsertPolicy,
        allocate: impl FnMut(fgdb_gql::insertion::GraphInsertRequest) -> Result<ElementId, A>,
    ) -> Result<
        fgdb_gql::insertion::GraphInsertStats,
        TxnGqlError<fgdb_gql::insertion::GraphInsertError<WriteTxnError, A>>,
    > {
        self.execute_graph_insert_governed_inner(
            database, cx, insertion, policy, allocate, false, None,
        )
        .map(|(stats, _, _)| stats)
    }

    /// Stage the same insertion while retaining the exact identities accepted
    /// into the workspace. Vertex IDs are ordered by selected occurrence then
    /// vertex declaration; edge IDs use occurrence then edge declaration. The
    /// fixed per-occurrence widths are `insertion.vertices_per_row()` and
    /// `insertion.edges_per_row()`, so callers can recover the row-local mapping
    /// without graph reads or allocator-side bookkeeping.
    ///
    /// These vectors are a staged-operation receipt, NOT proof of durability:
    /// only a later successful transaction finish/commit publishes them. The
    /// identity vectors are bounded by `max_vertices`/`max_edges`; they do not
    /// add an unbounded result surface outside the insertion policy. Failure
    /// returns no receipt even when the external allocator already issued IDs.
    pub fn execute_graph_insert_returning_governed<V: Vfs + Clone, A>(
        &mut self,
        database: &mut Database<V>,
        cx: &fgdb_types::QueryCx,
        insertion: &fgdb_gql::insertion::PreparedGraphInsert,
        policy: fgdb_gql::insertion::GraphInsertPolicy,
        allocate: impl FnMut(fgdb_gql::insertion::GraphInsertRequest) -> Result<ElementId, A>,
    ) -> Result<
        WithAffectedIds<fgdb_gql::insertion::GraphInsertStats>,
        TxnGqlError<fgdb_gql::insertion::GraphInsertError<WriteTxnError, A>>,
    > {
        self.execute_graph_insert_governed_inner(
            database, cx, insertion, policy, allocate, true, None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn execute_graph_insert_governed_inner<V: Vfs + Clone, A>(
        &mut self,
        database: &mut Database<V>,
        cx: &fgdb_types::QueryCx,
        insertion: &fgdb_gql::insertion::PreparedGraphInsert,
        policy: fgdb_gql::insertion::GraphInsertPolicy,
        allocate: impl FnMut(fgdb_gql::insertion::GraphInsertRequest) -> Result<ElementId, A>,
        retain_identities: bool,
        independent: Option<&mut IndependentEdgeWrites>,
    ) -> Result<
        WithAffectedIds<fgdb_gql::insertion::GraphInsertStats>,
        TxnGqlError<fgdb_gql::insertion::GraphInsertError<WriteTxnError, A>>,
    > {
        use fgdb_gql::GqlQueryError;
        use fgdb_gql::insertion::GraphInsertError;
        let source = |error| GqlQueryError::Source(GraphInsertError::Source(error));
        // Never allocate even one ID before ownership, health, basis and
        // relation-coordinate checks, including for empty or zero-budget input.
        self.ensure_database(database).map_err(source)?;
        let live = database
            .frontier()
            .map_err(WriteTxnError::from)
            .map_err(source)?;
        if live != self.basis {
            return Err(source(WriteTxnError::SnapshotAdvanced {
                pinned: self.basis,
                live,
            }));
        }
        cx.with_restriction(|| {
            let proposal = insertion.execute_governed_with_source(
                policy,
                InsertGraphSource {
                    transaction: self,
                    database,
                    cx,
                },
                allocate,
                || cx.checkpoint(),
            )?;
            self.stage_graph_insert_batch_with_independent(
                database,
                cx,
                insertion.relation(),
                proposal,
                retain_identities,
                independent,
            )
        })
    }

    // Both ordinary insertion and CREATE RETURN submit the same already-frozen
    // proposal to the existing atomic storage path. RETURN never reconstructs
    // that proposal from graph reads or from identity receipt vectors.
    fn stage_graph_insert_batch<V: Vfs + Clone, A>(
        &mut self,
        database: &mut Database<V>,
        cx: &fgdb_types::QueryCx,
        relation: RelationId,
        proposal: fgdb_gql::insertion::GraphInsertBatch,
        retain_identities: bool,
    ) -> Result<
        WithAffectedIds<fgdb_gql::insertion::GraphInsertStats>,
        TxnGqlError<fgdb_gql::insertion::GraphInsertError<WriteTxnError, A>>,
    > {
        self.stage_graph_insert_batch_with_independent(
            database,
            cx,
            relation,
            proposal,
            retain_identities,
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn stage_graph_insert_batch_with_independent<V: Vfs + Clone, A>(
        &mut self,
        database: &mut Database<V>,
        cx: &fgdb_types::QueryCx,
        relation: RelationId,
        proposal: fgdb_gql::insertion::GraphInsertBatch,
        retain_identities: bool,
        independent: Option<&mut IndependentEdgeWrites>,
    ) -> Result<
        WithAffectedIds<fgdb_gql::insertion::GraphInsertStats>,
        TxnGqlError<fgdb_gql::insertion::GraphInsertError<WriteTxnError, A>>,
    > {
        use fgdb_gql::GqlQueryError;
        use fgdb_gql::insertion::{GraphInsertError, GraphInsertIntent};
        let source = |error| GqlQueryError::Source(GraphInsertError::Source(error));
        let stats = proposal.stats();
        // Preserve the preexisting stats-only physical path: identity result
        // vectors exist only for the explicit returning API.
        let mut vertices = retain_identities.then(Vec::new);
        let mut edges = retain_identities.then(Vec::new);
        // Vertex declarations must lead the shared initializer prefix.
        // Edge groups retain declaration order within each relation.
        let mut batch = WriteBatch::new(relation);
        let mut groups = std::collections::BTreeMap::new();
        for intent in proposal.into_intents() {
            cx.checkpoint().map_err(GqlQueryError::Interrupted)?;
            match intent {
                GraphInsertIntent::Vertex {
                    vertex,
                    labels,
                    properties,
                } => {
                    if let Some(vertices) = &mut vertices {
                        vertices.push(vertex);
                    }
                    batch.create_vertex_values(vertex, labels, properties);
                }
                GraphInsertIntent::Edge {
                    edge,
                    relation,
                    source,
                    destination,
                    properties,
                } => {
                    if let Some(edges) = &mut edges {
                        edges.push(edge);
                    }
                    groups
                        .entry(relation)
                        .or_insert_with(|| WriteBatch::new(relation))
                        .add_edge_values(edge, source, destination, properties);
                }
            }
        }
        cx.checkpoint().map_err(GqlQueryError::Interrupted)?;
        let mut batches = Vec::with_capacity(groups.len() + usize::from(!batch.is_empty()));
        if !batch.is_empty() {
            batches.push(batch);
        }
        batches.extend(groups.into_values());
        if let Some(independent) = independent {
            if !batches.is_empty() {
                self.ensure_database(database).map_err(source)?;
                let live = database
                    .frontier()
                    .map_err(WriteTxnError::from)
                    .map_err(source)?;
                if live != self.basis {
                    return Err(source(WriteTxnError::SnapshotAdvanced {
                        pinned: self.basis,
                        live,
                    }));
                }
                if batches.len() != 1 {
                    return Err(source(WriteTxnError::UnsupportedAtomicMutation));
                }
                independent
                    .stage(database, batches.remove(0))
                    .map_err(source)?;
            }
        } else if let Some(first) = batches.first() {
            let relation = first.relation;
            if batches.iter().all(|batch| batch.relation == relation)
                && self.staged.iter().all(|batch| batch.relation == relation)
            {
                let mut combined = batches.remove(0);
                for batch in batches {
                    combined.rows.extend(batch.rows);
                }
                self.write(database, combined).map_err(source)?;
            } else {
                self.write_atomic(database, batches).map_err(source)?;
            }
        }
        Ok((
            stats,
            vertices.unwrap_or_default(),
            edges.unwrap_or_default(),
        ))
    }
}

#[cfg(test)]
mod chain_materialization_tests {
    use super::*;
    use asupersync::lab::run_async_under_lab;
    use fgdb_delta_types::PropertyKeyId;
    use fgdb_gql::algebra::{GraphValue, GraphValueRow, PreparedGraphPattern};
    use fgdb_gql::insertion::{GraphInsertPolicy, GraphInsertRequest};
    use fgdb_gql::{
        GlaExecutionEvent, GlaExecutionStats, GlaLimitDimension, GqlParameterType, GqlParameters,
        GqlQueryError, GqlQueryExecution, GqlQueryPolicy, GraphEdgeMergePolicy, GraphSymbol,
        GraphSymbolKind, GraphVertexMergePolicy, GraphWriteChainStep, PreparedGraphWriteChain,
        PreparedGraphWriteChainText,
    };
    use fgdb_types::{DatabaseSecurityNamespaceId, PurposeContexts};
    use std::sync::atomic::{AtomicU64, Ordering};

    const R: RelationId = RelationId(1);
    const P: PropertyKeyId = PropertyKeyId(1);
    const Q: PropertyKeyId = PropertyKeyId(2);
    const HIGH: u64 = 10_000_000;
    const WIDE: VId = VId((1_u128 << 100) | 2);
    const WIDE_EDGE: EId = EId((1_u128 << 112) | 1);
    const VERTEX: &str = "UNWIND [1,1,2] AS k WITH k,$payload AS payload \
        MERGE (n:Target {p:k}) CREATE (c:Copy {p:size(payload)})";
    const EDGE: &str = "MATCH (a:Source),(b:Target) WHERE b.p=1 \
        WITH a,b,$payload AS payload MERGE (a)-[r:R]->(b) \
        CREATE (c:Copy {p:size(payload)})";
    const RETURNING: &str = "UNWIND [1,1,2] AS k WITH k,$payload AS payload \
        MERGE (n:Target {p:k}) ON MATCH SET n.seen=7 \
        RETURN n,n.q AS q,payload AS payload ORDER BY n";

    fn keys() -> crate::DatabaseKeys {
        crate::DatabaseKeys::new(
            [0x31; 32],
            DatabaseSecurityNamespaceId([0x32; 32]),
            [0x33; 32],
        )
    }

    fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        match (kind, name) {
            (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
            (GraphSymbolKind::Label, "Source") => Some(GraphSymbol::Label(LabelId(1))),
            (GraphSymbolKind::Label, "Target") => Some(GraphSymbol::Label(LabelId(2))),
            (GraphSymbolKind::Label, "Copy") => Some(GraphSymbol::Label(LabelId(3))),
            (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
            (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
            (GraphSymbolKind::Property, "seen") => Some(GraphSymbol::Property(PropertyKeyId(3))),
            _ => None,
        }
    }

    fn payload() -> Vec<GraphValue> {
        vec![
            GraphValue::Scalar(CanonicalScalar::ucs_basic_text(&"x".repeat(2048)).unwrap()),
            GraphValue::List(
                vec![
                    GraphValue::Scalar(CanonicalScalar::Int(7)),
                    GraphValue::Scalar(CanonicalScalar::Null),
                ]
                .into(),
            ),
        ]
    }

    fn prepare(text: &str) -> PreparedGraphWriteChain {
        assert!(
            PreparedGraphWriteChainText::is_chain(text).unwrap(),
            "{text}"
        );
        PreparedGraphWriteChainText::prepare_with_parameter_types(
            text,
            R,
            &[("payload", GqlParameterType::List)],
            symbols,
        )
        .expect(text)
        .bind_parameters(
            &GqlParameters::new()
                .with_list("payload", payload())
                .unwrap(),
        )
        .expect(text)
    }

    fn policy(work: u64, scratch: u64) -> GraphInsertPolicy {
        GraphInsertPolicy::new(GqlQueryPolicy::new(HIGH, HIGH, work, scratch), 100, 100)
    }

    fn seed() -> WriteBatch {
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(1), vec![LabelId(1)], vec![]);
        let stored = GraphValue::List(payload().into())
            .to_property_value()
            .unwrap();
        for (vertex, key) in [(VId(2), 1), (WIDE, 1), (VId(3), 2)] {
            seed.create_vertex_values(
                vertex,
                vec![LabelId(2)],
                vec![(P, CanonicalScalar::Int(key).into()), (Q, stored.clone())],
            );
        }
        seed.add_edge(EId(1), VId(1), VId(2), vec![]);
        seed.add_edge(WIDE_EDGE, VId(1), VId(2), vec![]);
        seed.add_edge(EId(3), VId(1), WIDE, vec![]);
        seed
    }

    fn stage_prefix<V: Vfs + Clone>(transaction: &mut WriteTxn, db: &mut Database<V>) {
        let mut prefix = WriteBatch::new(R);
        prefix.create_vertex(VId(77), vec![], vec![(P, CanonicalScalar::Int(99))]);
        transaction.write(db, prefix).unwrap();
    }

    fn allocate(next: &AtomicU64, request: GraphInsertRequest) -> Result<ElementId, ()> {
        let id = u128::from(next.fetch_add(1, Ordering::Relaxed));
        Ok(match request {
            GraphInsertRequest::Vertex { .. } => ElementId::Vertex(VId(id)),
            GraphInsertRequest::Edge { .. } => ElementId::Edge(EId(id)),
        })
    }

    fn event(stats: &mut GlaExecutionStats, event: GlaExecutionEvent) {
        assert_ne!(event, GlaExecutionEvent::ResultRow);
        stats.work_units += 1;
        stats.scratch_entries += u64::from(event == GlaExecutionEvent::ScratchEntry);
    }

    fn no_graph(
        _: &PreparedGraphPattern<GraphValueRow>,
        _: GqlQueryPolicy,
    ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<(), ()>> {
        panic!("the final CREATE consumes owned rows, never a fresh graph scan")
    }

    // Independently execute the native MERGE and CREATE, not the chain adapter.
    // Use the old row constructors for expected values; derive copy charges
    // from the canonical value copier and explicit occurrence/cell slots.
    fn continuation_reference<V: Vfs + Clone>(
        transaction: &mut WriteTxn,
        db: &mut Database<V>,
        cx: &fgdb_types::QueryCx,
        chain: &PreparedGraphWriteChain,
    ) -> (GlaExecutionStats, usize) {
        let mut copies = GlaExecutionStats::default();
        let (mut expected, rows) = match &chain.steps()[0] {
            GraphWriteChainStep::VertexMerge(merge) => {
                let (stats, _, input, pairs) = transaction
                    .execute_graph_vertex_merges_over(
                        db,
                        cx,
                        merge,
                        GraphVertexMergePolicy::new(policy(HIGH, HIGH).query),
                        None,
                        |_| -> Result<ElementId, ()> { panic!("all targets exist") },
                    )
                    .unwrap();
                let rows = pairs
                    .into_iter()
                    .map(|(at, vertex)| merge.output_row(&input[at], vertex))
                    .collect::<Vec<_>>();
                assert_eq!(rows.len(), 5, "two matches for each repeated key, then one");
                (stats.evaluator, rows)
            }
            GraphWriteChainStep::EdgeMergeRows { merge, keep } => {
                let (stats, resolved, input) = transaction
                    .execute_graph_edge_merge_output_governed::<EdgeMergeRows, V, ()>(
                        db,
                        cx,
                        merge,
                        GraphEdgeMergePolicy::new(policy(HIGH, HIGH).query),
                        None,
                        |_| -> Result<ElementId, ()> { panic!("all edges exist") },
                    )
                    .unwrap();
                let mut rows = Vec::new();
                for row in &input {
                    event(&mut copies, GlaExecutionEvent::Work);
                    let source = row.values()[merge.source_column()].as_vertex().unwrap();
                    let destination = row.values()[merge.destination_column()]
                        .as_vertex()
                        .unwrap();
                    for &(edge, _) in &resolved.edges[&(source, destination)] {
                        let mut cells = row.values()[..*keep].to_vec();
                        cells.push(GraphValue::Edge(edge));
                        rows.push(GraphValueRow::from_owned_values(cells));
                    }
                }
                assert_eq!(
                    rows.len(),
                    3,
                    "parallel full-width edge IDs remain separate"
                );
                assert!(
                    rows.iter()
                        .any(|row| row.values().last() == Some(&GraphValue::Edge(WIDE_EDGE)))
                );
                (stats.evaluator, rows)
            }
            _ => panic!("a non-returning MERGE must own the first step"),
        };
        let count = rows.len();
        for row in &rows {
            event(&mut copies, GlaExecutionEvent::ScratchEntry);
            for value in &row.values()[..row.len() - 1] {
                event(&mut copies, GlaExecutionEvent::ScratchEntry);
                let _copy = value
                    .copy_with_control(&mut |observed| {
                        event(&mut copies, observed);
                        Ok::<(), ()>(())
                    })
                    .unwrap();
            }
            event(&mut copies, GlaExecutionEvent::ScratchEntry);
        }
        assert!(
            copies.scratch_entries > 100,
            "amplified payload costs must be nonzero"
        );
        expected.work_units += copies.work_units;
        expected.scratch_entries += copies.scratch_entries;
        let GraphWriteChainStep::Insert(insertion) = &chain.steps()[1] else {
            panic!("the final CREATE must not synthesize a RETURN")
        };
        let next = AtomicU64::new(1_000);
        let proposal = insertion
            .execute_governed_over(
                Some(rows),
                policy(HIGH, HIGH),
                no_graph,
                |request| allocate(&next, request),
                || Ok(()),
            )
            .unwrap();
        assert_eq!(proposal.stats().created_vertices as usize, count);
        expected.work_units += proposal.stats().evaluator.work_units;
        expected.scratch_entries += proposal.stats().evaluator.scratch_entries;
        (expected, count)
    }

    #[test]
    fn vertex_and_parallel_edge_continuations_spend_each_payload_copy_before_next_write() {
        let ((), report) = run_async_under_lab(0xc4a2_1001, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            for text in [VERTEX, EDGE] {
                let chain = prepare(text);
                for dimension in [
                    GlaLimitDimension::WorkUnits,
                    GlaLimitDimension::ScratchEntries,
                ] {
                    let mut db = Database::open_memory(&contexts.commit(), keys())
                        .await
                        .unwrap();
                    db.write(&contexts.commit(), seed()).await.unwrap();
                    let basis = db.frontier().unwrap();
                    let mut reference = db.begin(&contexts.txn()).unwrap();
                    stage_prefix(&mut reference, &mut db);
                    let (expected, count) =
                        continuation_reference(&mut reference, &mut db, &contexts.query(), &chain);
                    reference.abort();
                    let mut transaction = db.begin(&contexts.txn()).unwrap();
                    stage_prefix(&mut transaction, &mut db);
                    let digest = transaction.staged_effect_digest().unwrap();
                    let next = AtomicU64::new(1_000);
                    let mut tight = policy(expected.work_units, expected.scratch_entries);
                    if dimension == GlaLimitDimension::WorkUnits {
                        tight.query.evaluator.max_work_units -= 1;
                    } else {
                        tight.query.evaluator.max_scratch_entries -= 1;
                    }
                    let refused = transaction
                        .execute_graph_write_chain_governed(
                            &mut db,
                            &contexts.query(),
                            &chain,
                            tight,
                            |request| allocate(&next, request),
                        )
                        .unwrap_err();
                    assert!(
                        matches!(refused, GqlQueryError::Evaluator(limit) if limit.dimension == dimension)
                    );
                    assert_eq!(transaction.staged_effect_digest().unwrap(), digest);
                    assert_eq!(db.frontier().unwrap(), basis);
                    assert!(db.vertex(VId(77)).unwrap().is_none());
                    let after_failure = next.load(Ordering::Relaxed);
                    let (stats, result, receipt) = transaction
                        .execute_graph_write_chain_governed(
                            &mut db,
                            &contexts.query(),
                            &chain,
                            policy(expected.work_units, expected.scratch_entries),
                            |request| allocate(&next, request),
                        )
                        .unwrap();
                    assert_eq!(stats.evaluator, expected, "{text}");
                    assert_eq!(result.evaluator, expected);
                    assert_eq!(receipt.stats().evaluator, expected);
                    assert!(result.value.is_empty());
                    assert_eq!(result.rows.result_rows, 0);
                    assert_eq!(stats.created_vertices as usize, count);
                    transaction
                        .finish(&mut db, &contexts.commit())
                        .await
                        .unwrap();
                    assert_eq!(db.frontier().unwrap().0, basis.0 + 1);
                    assert_eq!(db.vertices().unwrap().len(), 5 + count);
                    assert_eq!(db.edges().unwrap().len(), 3);
                    for id in 1_000..after_failure {
                        assert!(db.vertex(VId(u128::from(id))).unwrap().is_none());
                    }
                    for id in after_failure..next.load(Ordering::Relaxed) {
                        assert_eq!(
                            db.vertex(VId(u128::from(id))).unwrap().unwrap().props,
                            vec![(P, CanonicalScalar::Int(2).into())]
                        );
                    }
                    assert_eq!(contexts.txn().outstanding_obligations(), 0);
                }
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn return_binding_costs_survive_limit_zero_and_late_failure_restores_merge_actions() {
        let ((), report) = run_async_under_lab(0xc4a2_1002, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            for text in [RETURNING.to_owned(), format!("{RETURNING} LIMIT 0")] {
                let chain = prepare(&text);
                let [step] = chain.steps() else {
                    panic!("one returning MERGE step is expected")
                };
                let GraphWriteChainStep::VertexMergeQuery(query) = step else {
                    panic!("the native per-row MERGE must own this statement")
                };
                for dimension in [
                    GlaLimitDimension::WorkUnits,
                    GlaLimitDimension::ScratchEntries,
                ] {
                    let mut db = Database::open_memory(&contexts.commit(), keys())
                        .await
                        .unwrap();
                    db.write(&contexts.commit(), seed()).await.unwrap();
                    let basis = db.frontier().unwrap();
                    let original = db.vertex(VId(2)).unwrap().unwrap().props;
                    let mut reference = db.begin(&contexts.txn()).unwrap();
                    stage_prefix(&mut reference, &mut db);
                    let cx = contexts.query();
                    let (stats, _, input, pairs) = reference
                        .execute_graph_vertex_merges_over(
                            &mut db,
                            &cx,
                            query.merge(),
                            GraphVertexMergePolicy::new(policy(HIGH, HIGH).query),
                            None,
                            |_| -> Result<ElementId, ()> { panic!("all targets exist") },
                        )
                        .unwrap();
                    assert_eq!(pairs.len(), 5);
                    let mut copying = GlaExecutionStats::default();
                    let mut rows = Vec::new();
                    for (at, vertex) in pairs {
                        rows.push(
                            step.vertex_return_row_controlled(
                                &input[at],
                                vertex,
                                |element, key| {
                                    reference
                                        .merge_return_property(&db, element, key)
                                        .map_err(GqlQueryError::<WriteTxnError, ()>::Source)
                                },
                                &mut |observed| {
                                    event(&mut copying, observed);
                                    Ok::<(), GqlQueryError<WriteTxnError, ()>>(())
                                },
                            )
                            .unwrap(),
                        );
                    }
                    assert!(copying.scratch_entries > 100);
                    let mut complete = stats.evaluator;
                    complete.work_units += copying.work_units;
                    complete.scratch_entries += copying.scratch_entries;
                    let expected = query
                        .finish_governed::<WriteTxnError, ()>(
                            rows,
                            stats.match_selection,
                            complete,
                            policy(HIGH, HIGH).query,
                            || Ok(()),
                        )
                        .unwrap();
                    reference.abort();
                    if text == RETURNING {
                        assert_eq!(expected.value.len(), 5);
                        for row in &expected.value {
                            assert_eq!(row.values()[1], GraphValue::List(payload().into()));
                            assert_eq!(row.values()[2], GraphValue::List(payload().into()));
                        }
                        assert_eq!(
                            expected
                                .value
                                .iter()
                                .filter(|row| { row.values()[0] == GraphValue::Vertex(WIDE) })
                                .count(),
                            2
                        );
                    } else {
                        assert!(expected.value.is_empty());
                    }
                    let mut transaction = db.begin(&contexts.txn()).unwrap();
                    stage_prefix(&mut transaction, &mut db);
                    let digest = transaction.staged_effect_digest().unwrap();
                    let mut tight = policy(
                        expected.evaluator.work_units,
                        expected.evaluator.scratch_entries,
                    );
                    if dimension == GlaLimitDimension::WorkUnits {
                        tight.query.evaluator.max_work_units -= 1;
                    } else {
                        tight.query.evaluator.max_scratch_entries -= 1;
                    }
                    let refused = transaction
                        .execute_graph_write_chain_governed(
                            &mut db,
                            &cx,
                            &chain,
                            tight,
                            |_| -> Result<ElementId, ()> { panic!("a match cannot allocate") },
                        )
                        .unwrap_err();
                    assert!(
                        matches!(refused, GqlQueryError::Evaluator(limit) if limit.dimension == dimension)
                    );
                    assert_eq!(transaction.staged_effect_digest().unwrap(), digest);
                    assert_eq!(
                        transaction.vertex(&db, VId(2)).unwrap().unwrap().props,
                        original
                    );
                    assert_eq!(db.frontier().unwrap(), basis);
                    let (stats, actual, receipt) = transaction
                        .execute_graph_write_chain_governed(
                            &mut db,
                            &cx,
                            &chain,
                            policy(
                                expected.evaluator.work_units,
                                expected.evaluator.scratch_entries,
                            ),
                            |_| -> Result<ElementId, ()> { panic!("a match cannot allocate") },
                        )
                        .unwrap();
                    assert_eq!(actual, expected);
                    assert_eq!(stats.evaluator, expected.evaluator);
                    assert_eq!(receipt.stats().evaluator, expected.evaluator);
                    transaction
                        .finish(&mut db, &contexts.commit())
                        .await
                        .unwrap();
                    assert_eq!(db.frontier().unwrap().0, basis.0 + 1);
                    assert_eq!(db.vertices().unwrap().len(), 5);
                    for vertex in [VId(2), WIDE, VId(3)] {
                        assert!(
                            db.vertex(vertex)
                                .unwrap()
                                .unwrap()
                                .props
                                .contains(&(PropertyKeyId(3), CanonicalScalar::Int(7).into()),)
                        );
                    }
                    assert_eq!(contexts.txn().outstanding_obligations(), 0);
                }
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }
}
