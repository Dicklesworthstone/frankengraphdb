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
        use fgdb_gql::algebra::GraphValue;
        use fgdb_gql::insertion::{GraphInsertError, GraphInsertPolicy, GraphInsertStats};
        use fgdb_gql::{
            GlaExecutionStats, GqlExecutionStats, GqlQueryError, GqlQueryExecution, GqlQueryPolicy,
            GraphEdgeMergePolicy, GraphEdgeMergeQueryError, GraphEdgeMergeStats,
            GraphInsertQueryError, GraphVertexMergePolicy, GraphVertexMergeRowsBinding,
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
        let remaining = |used: &GraphInsertStats, last: bool| {
            let records = policy
                .query
                .rows
                .max_snapshot_records()
                .map_or(u64::MAX, |limit| {
                    limit.saturating_sub(used.selection.snapshot_records)
                });
            let rows = if last {
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
                let allowance = remaining(&used, at == last);
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
                        let (stats, receipt, input, pairs) = workspace
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
                        let output = pairs
                            .into_iter()
                            .map(|(at, vertex)| merge.output_row(&input[at], vertex))
                            .collect();
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
                    GraphWriteChainStep::EdgeMergeRows { merge, keep } => {
                        let (stats, resolved, selected) = workspace
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
                            cx.checkpoint().map_err(GqlQueryError::Interrupted)?;
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
                                let mut cells = values[..(*keep).min(values.len())].to_vec();
                                cells.push(GraphValue::Edge(edge));
                                output.push(fgdb_gql::algebra::GraphValueRow::from_owned_values(
                                    cells,
                                ));
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
                        let (stats, receipt, input, pairs) = workspace
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
                        let read = |element: Option<ElementId>,
                                    key|
                         -> Result<
                            GraphValue,
                            TxnGqlError<GraphWriteChainError<WriteTxnError, A>>,
                        > {
                            let value = match element {
                                None => None,
                                Some(element) => workspace
                                    .txn
                                    .merge_return_property(database, element, key)
                                    .map_err(chain)?,
                            };
                            match value {
                                None => Ok(GraphValue::Scalar(CanonicalScalar::Null)),
                                Some(value) => GraphValue::from_property_value(&value).ok_or(
                                    GqlQueryError::Data(fgdb_gql::GraphIntegerError {
                                        instruction: 0,
                                        kind: fgdb_gql::GraphIntegerErrorKind::StoredValueBounds,
                                    }),
                                ),
                            }
                        };
                        let mut returned = Vec::with_capacity(pairs.len());
                        for (at, vertex) in pairs {
                            cx.checkpoint().map_err(GqlQueryError::Interrupted)?;
                            let mut row = Vec::with_capacity(query.bindings().len());
                            for binding in query.bindings() {
                                row.push(match *binding {
                                    GraphVertexMergeRowsBinding::Input(column) => input[at]
                                        .values()
                                        .get(column)
                                        .cloned()
                                        .unwrap_or(GraphValue::Scalar(CanonicalScalar::Null)),
                                    GraphVertexMergeRowsBinding::Vertex => {
                                        GraphValue::Vertex(vertex)
                                    }
                                    GraphVertexMergeRowsBinding::Property(key) => {
                                        read(Some(ElementId::Vertex(vertex)), key)?
                                    }
                                    GraphVertexMergeRowsBinding::InputProperty { column, key } => {
                                        let element = match input[at].values().get(column) {
                                            Some(GraphValue::Vertex(carried)) => {
                                                Some(ElementId::Vertex(*carried))
                                            }
                                            Some(GraphValue::Edge(carried)) => {
                                                Some(ElementId::Edge(*carried))
                                            }
                                            _ => None,
                                        };
                                        read(element, key)?
                                    }
                                });
                            }
                            returned.push(row);
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
                add(&mut used, &stats);
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
