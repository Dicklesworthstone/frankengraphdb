// Mixed write programs reuse the existing private program guard and ordinary
// staging paths. This file owns orchestration, not another writer or matcher.

include!("frozen_update_program.rs");

/// Admitted edge chunks remain private until the program's last stage. Every
/// selection in this profile reads only the unchanged vertex domain, so it
/// sees exactly the same values as the ordinary canonical prefix overlay.
struct IndependentEdgeWrites {
    relation: RelationId,
    batches: Vec<WriteBatch>,
    chunks: Vec<crate::LogicalDeltaTemplate>,
    created: fgdb_gql::id_hash::IdSet<EId>,
    dependencies: crate::prepared_write::PreparedDependencies,
}

impl IndependentEdgeWrites {
    fn admit(txn: &WriteTxn, program: &fgdb_gql::PreparedGraphWriteProgram) -> Option<Self> {
        if !txn.staged.is_empty() || txn.prepared.is_some() || program.statements().len() < 2 {
            return None;
        }
        let mut relation = None;
        for statement in program.statements() {
            let fgdb_gql::GraphWriteStatement::Insert(insertion) = statement else {
                return None;
            };
            let current = insertion.independent_edge_append_relation()?;
            if relation.is_some_and(|previous| previous != current) {
                return None;
            }
            relation = Some(current);
        }
        Some(Self {
            relation: relation?,
            batches: Vec::new(),
            chunks: Vec::new(),
            created: fgdb_gql::id_hash::IdSet::default(),
            dependencies: crate::prepared_write::PreparedDependencies::default(),
        })
    }

    fn stage<V: Vfs + Clone>(
        &mut self,
        database: &mut Database<V>,
        batch: WriteBatch,
    ) -> Result<(), WriteTxnError> {
        // The checked insertion definition proves this shape. Keep the storage
        // seam closed even if a future caller forgets that eligibility proof.
        if batch.relation != self.relation
            || batch
                .rows
                .iter()
                .any(|row| !matches!(row, PendingRow::Edge { ensure: false, .. }))
        {
            return Err(WriteTxnError::UnsupportedAtomicMutation);
        }
        database.ensure_writable()?;
        // Capture BEFORE validation: a later storage error or unwind can expose
        // the same conservative dependencies as an ordinary refused write.
        self.dependencies
            .extend(crate::prepared_write::PreparedDependencies::capture(
                &database.writer,
                &batch,
            ));
        let chunk = database.prepare_independent_edge_chunk(batch.clone(), &self.created)?;
        for row in chunk.rows() {
            let crate::DeltaRow::CreateEdge { eid, .. } = row else {
                return Err(WriteTxnError::UnsupportedAtomicMutation);
            };
            self.created.insert(*eid);
        }
        self.chunks.push(chunk);
        self.batches.push(batch);
        Ok(())
    }
}

impl<'txn> MutationProgramWorkspace<'txn> {
    fn for_write_program(
        txn: &'txn mut WriteTxn,
        program: &fgdb_gql::PreparedGraphWriteProgram,
    ) -> Self {
        let independent_edges = IndependentEdgeWrites::admit(txn, program);
        let mut workspace = Self::new(txn);
        workspace.independent_edges = independent_edges;
        workspace
    }

    fn finish_independent_edges<V: Vfs + Clone>(
        &mut self,
        database: &Database<V>,
    ) -> Result<(), WriteTxnError> {
        let Some(independent) = &mut self.independent_edges else {
            return Ok(());
        };
        if independent.chunks.is_empty() {
            return Ok(());
        }
        let prepared = database.compose_independent_edge_chunks(
            independent.relation,
            &independent.chunks,
            independent.dependencies.clone(),
        )?;
        debug_assert_eq!(prepared.basis(), self.txn.basis);
        debug_assert!(self.txn.prepared.is_none() && self.txn.staged.is_empty());
        self.txn.staged = core::mem::take(&mut independent.batches);
        self.txn.prepared = Some(std::sync::Arc::new(prepared));
        Ok(())
    }
}

impl WriteTxn {
    /// Execute CREATE/INSERT/MERGE with database-owned identity reservations.
    pub fn execute_graph_write_program_engine_governed<V: Vfs + Clone>(
        &mut self,
        database: &mut Database<V>,
        cx: &fgdb_types::QueryCx,
        program: &fgdb_gql::PreparedGraphWriteProgram,
        policy: fgdb_gql::GraphWriteProgramPolicy,
    ) -> Result<
        fgdb_gql::GraphWriteProgramStats,
        fgdb_gql::GraphWriteProgramError<
            WriteTxnError,
            WriteTxnError,
            Box<asupersync::error::Error>,
        >,
    > {
        let preflight = |error| {
            fgdb_gql::GraphWriteProgramError::Program(
                fgdb_gql::GraphMutationProgramError::Preflight(error),
            )
        };
        self.ensure_database(database).map_err(preflight)?;
        let mut allocate = database.engine_allocator(cx).map_err(preflight)?;
        self.execute_graph_write_program_governed(database, cx, program, policy, |request| {
            allocate(request.request)
        })
    }

    /// Return the same ordered receipts with no caller-supplied allocator.
    pub fn execute_graph_write_program_returning_engine_governed<V: Vfs + Clone>(
        &mut self,
        database: &mut Database<V>,
        cx: &fgdb_types::QueryCx,
        program: &fgdb_gql::PreparedGraphWriteProgram,
        policy: fgdb_gql::GraphWriteProgramPolicy,
    ) -> Result<
        fgdb_gql::GraphWriteProgramReceipt,
        fgdb_gql::GraphWriteProgramError<
            WriteTxnError,
            WriteTxnError,
            Box<asupersync::error::Error>,
        >,
    > {
        let preflight = |error| {
            fgdb_gql::GraphWriteProgramError::Program(
                fgdb_gql::GraphMutationProgramError::Preflight(error),
            )
        };
        self.ensure_database(database).map_err(preflight)?;
        let mut allocate = database.engine_allocator(cx).map_err(preflight)?;
        self.execute_graph_write_program_returning_governed(
            database,
            cx,
            program,
            policy,
            |request| allocate(request.request),
        )
    }

    /// Stage CREATE, SET/REMOVE, DELETE, DETACH DELETE, vertex MERGE and directed
    /// relationship MERGE, including ON MATCH/ON CREATE actions on either
    /// element kind, as one atomic operation inside this transaction. Each step
    /// sees its predecessor's canonical overlay; assignments within a step
    /// remain frozen and simultaneous. A native UNWIND vertex-update program
    /// with frozen input provenance evaluates every step against its entry
    /// overlay, then stages the updates in input order. No step commits.
    /// Plain DELETE refuses incident relationships; it never becomes a cascade.
    /// Statements may name different relations; their effects must satisfy
    /// `write_atomic` composition, including the shared-initializer prefix rules.
    ///
    /// Any error, cancellation or Rust unwind restores the exact prior staged
    /// workspace, including its already-prepared write. Read observations are
    /// retained even from failed or rolled-back steps. The snapshot pin stays
    /// live so the caller may continue or abort the outer transaction.
    ///
    /// Identity requests carry the program statement index and row-local request.
    /// The caller's allocation policy must also distinguish separate executions.
    /// Issued identities are NOT reclaimed on rollback. No allocator request
    /// precedes owner, health and basis preflight. MERGE's create
    /// branch observes the same remaining creation cap as ordinary insertion;
    /// existing matches and empty endpoint selections do not allocate identities.
    /// Plain relationship MERGE handles every distinct directed endpoint pair
    /// in its frozen selection. Relationship ON/trailing actions preserve every
    /// MATCH occurrence and parallel match. Their receipts retain occurrence
    /// order; target visits count distinct modified identities, not occurrences.
    /// Vertex MERGE and ON/SET handle all distinct frozen targets, or create
    /// one vertex for an empty match. Adding RETURN is not required for this
    /// cardinality. Explicit unique and authorized primitives stay separate.
    ///
    /// Quotas sum source visits (including relationship existence scans),
    /// selected occurrences, work, scratch, mutation intents, created vertices
    /// and created edges, including later-canceled effects. They do not cover
    /// allocator service work, cloning the initial prepared workspace, repeated
    /// storage preparation, cascades or commit I/O.
    pub fn execute_graph_write_program_governed<V: Vfs + Clone, A>(
        &mut self,
        database: &mut Database<V>,
        cx: &fgdb_types::QueryCx,
        program: &fgdb_gql::PreparedGraphWriteProgram,
        policy: fgdb_gql::GraphWriteProgramPolicy,
        mut allocate: impl FnMut(fgdb_gql::GraphWriteIdentityRequest) -> Result<ElementId, A>,
    ) -> Result<
        fgdb_gql::GraphWriteProgramStats,
        fgdb_gql::GraphWriteProgramError<WriteTxnError, A, Box<asupersync::error::Error>>,
    > {
        use fgdb_gql::{
            GraphMutationProgramError, GraphWriteIdentityRequest, GraphWriteProgramError,
            GraphWriteStatement, GraphWriteStepError, GraphWriteStepStats,
        };
        let preflight =
            |error| GraphWriteProgramError::Program(GraphMutationProgramError::Preflight(error));
        self.ensure_database(database).map_err(preflight)?;
        let live = database
            .frontier()
            .map_err(WriteTxnError::from)
            .map_err(preflight)?;
        if live != self.basis {
            return Err(preflight(WriteTxnError::SnapshotAdvanced {
                pinned: self.basis,
                live,
            }));
        }
        if program.freezes_vertex_update_input() {
            return self
                .execute_frozen_vertex_updates_governed(database, cx, program, policy, false)
                .map(|(stats, _)| stats);
        }
        cx.with_restriction(|| {
            let mut workspace = MutationProgramWorkspace::for_write_program(self, program);
            workspace.txn.program_multi_relation = true;
            let stats = program.execute_governed(
                policy,
                |statement, input, remaining| {
                    let stats = match input {
                        GraphWriteStatement::Mutation(input) => workspace
                            .txn
                            .execute_graph_mutation_governed(
                                database,
                                cx,
                                input,
                                remaining.mutations,
                            )
                            .map(GraphWriteStepStats::Mutation)
                            .map_err(GraphWriteStepError::Mutation),
                        GraphWriteStatement::Insert(input) => workspace
                            .txn
                            .execute_graph_insert_governed_inner(
                                database,
                                cx,
                                input,
                                remaining.insertion_policy(),
                                |request| {
                                    allocate(GraphWriteIdentityRequest { statement, request })
                                },
                                false,
                                workspace.independent_edges.as_mut(),
                            )
                            .map(|(stats, _, _)| GraphWriteStepStats::Insert(stats))
                            .map_err(GraphWriteStepError::Insert),
                        GraphWriteStatement::VertexMerge(input) => workspace
                            .txn
                            .execute_graph_vertex_merges_governed(
                                database,
                                cx,
                                input,
                                remaining.vertex_merge_policy(),
                                |request| {
                                    allocate(GraphWriteIdentityRequest { statement, request })
                                },
                            )
                            .map(|(stats, _)| GraphWriteStepStats::VertexMerge(stats))
                            .map_err(GraphWriteStepError::VertexMerge),
                        GraphWriteStatement::VertexUpsert(input) => workspace
                            .txn
                            .execute_graph_vertex_upserts_governed(
                                database,
                                cx,
                                input,
                                remaining.vertex_upsert_policy(),
                                |request| {
                                    allocate(GraphWriteIdentityRequest { statement, request })
                                },
                            )
                            .and_then(vertex_upsert_program_result)
                            .map(|(stats, _)| stats)
                            .map_err(GraphWriteStepError::VertexUpsert),
                        GraphWriteStatement::EdgeMerge(input) => workspace
                            .txn
                            .execute_graph_edge_merges_governed(
                                database,
                                cx,
                                input,
                                remaining.edge_merge_policy(),
                                |request| {
                                    allocate(GraphWriteIdentityRequest { statement, request })
                                },
                            )
                            .map(|(stats, _)| GraphWriteStepStats::EdgeMerge(stats))
                            .map_err(GraphWriteStepError::EdgeMerge),
                        GraphWriteStatement::EdgeUpsert(input) => workspace
                            .txn
                            .execute_graph_edge_upserts_governed(
                                database,
                                cx,
                                input,
                                remaining.edge_upsert_policy(),
                                |request| {
                                    allocate(GraphWriteIdentityRequest { statement, request })
                                },
                            )
                            .and_then(|execution| {
                                edge_upsert_program_result(
                                    input,
                                    execution,
                                    remaining.edge_upsert_policy(),
                                    cx,
                                )
                            })
                            .map(|(stats, _)| stats)
                            .map_err(GraphWriteStepError::EdgeUpsert),
                        GraphWriteStatement::Delete(input) => workspace
                            .txn
                            .execute_graph_delete_governed(
                                database,
                                cx,
                                input,
                                remaining.deletion_policy(),
                            )
                            .map(GraphWriteStepStats::Delete)
                            .map_err(GraphWriteStepError::Delete),
                    }?;
                    if statement + 1 == program.statements().len() {
                        workspace
                            .finish_independent_edges(database)
                            .map_err(|error| {
                                GraphWriteStepError::Insert(fgdb_gql::GqlQueryError::Source(
                                    fgdb_gql::insertion::GraphInsertError::Source(error),
                                ))
                            })?;
                    }
                    Ok(stats)
                },
                || cx.checkpoint(),
            )?;
            // All quota checks and the final checkpoint ran before acceptance.
            // There is no fallible operation after installing the workspace.
            workspace.accept();
            Ok(stats)
        })
    }

    /// Execute the identical atomic mixed program but retain one ordered receipt
    /// per successfully accepted step. Creation receipts contain exact staged
    /// IDs; mutation/deletion receipts contain distinct proposal targets; MERGE
    /// receipts distinguish matched from created identities and missing input.
    /// No successful-prefix receipt escapes if ANY statement, quota check or
    /// final checkpoint fails.
    ///
    /// Program rollback cannot reclaim external identities. A returned receipt
    /// is still transaction-local: only a later successful finish/commit makes
    /// the staged effects durable.
    pub fn execute_graph_write_program_returning_governed<V: Vfs + Clone, A>(
        &mut self,
        database: &mut Database<V>,
        cx: &fgdb_types::QueryCx,
        program: &fgdb_gql::PreparedGraphWriteProgram,
        policy: fgdb_gql::GraphWriteProgramPolicy,
        mut allocate: impl FnMut(fgdb_gql::GraphWriteIdentityRequest) -> Result<ElementId, A>,
    ) -> Result<
        fgdb_gql::GraphWriteProgramReceipt,
        fgdb_gql::GraphWriteProgramError<WriteTxnError, A, Box<asupersync::error::Error>>,
    > {
        use fgdb_gql::{
            GraphMutationProgramError, GraphWriteIdentityRequest, GraphWriteProgramError,
            GraphWriteStatement, GraphWriteStepError, GraphWriteStepReceipt, GraphWriteStepStats,
        };
        let preflight =
            |error| GraphWriteProgramError::Program(GraphMutationProgramError::Preflight(error));
        self.ensure_database(database).map_err(preflight)?;
        let live = database
            .frontier()
            .map_err(WriteTxnError::from)
            .map_err(preflight)?;
        if live != self.basis {
            return Err(preflight(WriteTxnError::SnapshotAdvanced {
                pinned: self.basis,
                live,
            }));
        }
        if program.freezes_vertex_update_input() {
            return self
                .execute_frozen_vertex_updates_governed(database, cx, program, policy, true)
                .map(|(stats, receipts)| fgdb_gql::GraphWriteProgramReceipt::new(stats, receipts));
        }
        cx.with_restriction(|| {
            let mut workspace = MutationProgramWorkspace::for_write_program(self, program);
            workspace.txn.program_multi_relation = true;
            let mut receipts = Vec::with_capacity(program.statements().len());
            let stats = program.execute_governed(
                policy,
                |statement, input, remaining| {
                    let stats = match input {
                        GraphWriteStatement::Mutation(input) => workspace
                            .txn
                            .execute_graph_mutation_returning_governed(
                                database,
                                cx,
                                input,
                                remaining.mutations,
                            )
                            .map(|(stats, targets, edges)| {
                                receipts.push(GraphWriteStepReceipt::Mutation { targets, edges });
                                GraphWriteStepStats::Mutation(stats)
                            })
                            .map_err(GraphWriteStepError::Mutation),
                        GraphWriteStatement::Insert(input) => workspace
                            .txn
                            .execute_graph_insert_governed_inner(
                                database,
                                cx,
                                input,
                                remaining.insertion_policy(),
                                |request| {
                                    allocate(GraphWriteIdentityRequest { statement, request })
                                },
                                true,
                                workspace.independent_edges.as_mut(),
                            )
                            .map(|(stats, vertices, edges)| {
                                receipts.push(GraphWriteStepReceipt::Insert { vertices, edges });
                                GraphWriteStepStats::Insert(stats)
                            })
                            .map_err(GraphWriteStepError::Insert),
                        GraphWriteStatement::VertexMerge(input) => workspace
                            .txn
                            .execute_graph_vertex_merges_governed(
                                database,
                                cx,
                                input,
                                remaining.vertex_merge_policy(),
                                |request| {
                                    allocate(GraphWriteIdentityRequest { statement, request })
                                },
                            )
                            .and_then(vertex_merge_program_result)
                            .map(|(stats, receipt)| {
                                receipts.push(receipt);
                                GraphWriteStepStats::VertexMerge(stats)
                            })
                            .map_err(GraphWriteStepError::VertexMerge),
                        GraphWriteStatement::VertexUpsert(input) => workspace
                            .txn
                            .execute_graph_vertex_upserts_governed(
                                database,
                                cx,
                                input,
                                remaining.vertex_upsert_policy(),
                                |request| {
                                    allocate(GraphWriteIdentityRequest { statement, request })
                                },
                            )
                            .and_then(vertex_upsert_program_result)
                            .map(|(stats, receipt)| {
                                receipts.push(receipt);
                                stats
                            })
                            .map_err(GraphWriteStepError::VertexUpsert),
                        GraphWriteStatement::EdgeMerge(input) => workspace
                            .txn
                            .execute_graph_edge_merges_governed(
                                database,
                                cx,
                                input,
                                remaining.edge_merge_policy(),
                                |request| {
                                    allocate(GraphWriteIdentityRequest { statement, request })
                                },
                            )
                            .map(|(stats, receipt)| {
                                receipts.push(receipt);
                                GraphWriteStepStats::EdgeMerge(stats)
                            })
                            .map_err(GraphWriteStepError::EdgeMerge),
                        GraphWriteStatement::EdgeUpsert(input) => workspace
                            .txn
                            .execute_graph_edge_upserts_governed(
                                database,
                                cx,
                                input,
                                remaining.edge_upsert_policy(),
                                |request| {
                                    allocate(GraphWriteIdentityRequest { statement, request })
                                },
                            )
                            .and_then(|execution| {
                                edge_upsert_program_result(
                                    input,
                                    execution,
                                    remaining.edge_upsert_policy(),
                                    cx,
                                )
                            })
                            .map(|(stats, receipt)| {
                                receipts.push(receipt);
                                stats
                            })
                            .map_err(GraphWriteStepError::EdgeUpsert),
                        GraphWriteStatement::Delete(input) => workspace
                            .txn
                            .execute_graph_delete_elements_returning_governed(
                                database,
                                cx,
                                input,
                                remaining.deletion_policy(),
                            )
                            .map(|(stats, targets, edges)| {
                                receipts.push(GraphWriteStepReceipt::Delete { targets, edges });
                                GraphWriteStepStats::Delete(stats)
                            })
                            .map_err(GraphWriteStepError::Delete),
                    }?;
                    if statement + 1 == program.statements().len() {
                        workspace
                            .finish_independent_edges(database)
                            .map_err(|error| {
                                GraphWriteStepError::Insert(fgdb_gql::GqlQueryError::Source(
                                    fgdb_gql::insertion::GraphInsertError::Source(error),
                                ))
                            })?;
                    }
                    Ok(stats)
                },
                || cx.checkpoint(),
            )?;
            debug_assert_eq!(receipts.len(), stats.completed_statements);
            let receipt = fgdb_gql::GraphWriteProgramReceipt::new(stats, receipts);
            // Receipt construction is complete before acceptance. No fallible
            // operation or cancellation point follows this workspace boundary.
            workspace.accept();
            Ok(receipt)
        })
    }
}

// Receipt construction remains inside the caller's program workspace. A late
// allocation/quota/cancellation failure therefore rolls back every step, not
// just the already-accepted inner upsert. The occurrence bag is transferred,
// never deduplicated; only the separate modified-target count is distinct.
type EdgeUpsertProgramResult<A> = Result<
    (
        fgdb_gql::GraphWriteStepStats,
        fgdb_gql::GraphWriteStepReceipt,
    ),
    TxnGqlError<fgdb_gql::GraphEdgeUpsertError<WriteTxnError, A>>,
>;

fn edge_upsert_program_result<A>(
    input: &fgdb_gql::PreparedGraphEdgeUpsert,
    (mut stats, outcomes): (
        fgdb_gql::GraphEdgeUpsertBatchStats,
        Vec<fgdb_gql::GraphEdgeMergeOutcome>,
    ),
    policy: fgdb_gql::GraphEdgeUpsertPolicy,
    cx: &fgdb_types::QueryCx,
) -> EdgeUpsertProgramResult<A> {
    use fgdb_gql::{
        GlaExecutionEvent, GraphEdgeMergeOutcome, GraphEdgeUpsertBranch, GraphWriteStepReceipt,
        GraphWriteStepStats,
    };

    let bulk = outcomes.len() > 1;
    let mut created = Vec::new();
    let mut targets = std::collections::BTreeSet::new();
    let mut control =
        |event| edge_upsert_batch_event::<A>(cx, policy.merge.query, &mut stats.evaluator, event);
    control(GlaExecutionEvent::Work)?;
    for outcome in &outcomes {
        control(GlaExecutionEvent::Work)?;
        let (edge, branch) = match *outcome {
            GraphEdgeMergeOutcome::Created(edge) => {
                if bulk {
                    control(GlaExecutionEvent::ScratchEntry)?;
                    created.push(edge);
                }
                (edge, GraphEdgeUpsertBranch::Create)
            }
            GraphEdgeMergeOutcome::Matched(edge) => (edge, GraphEdgeUpsertBranch::Match),
            // The native batch represents NoInput by an empty bag, never a
            // fabricated occurrence mixed with resolved relationships.
            GraphEdgeMergeOutcome::NoInput => return Err(edge_upsert_batch_invalid()),
        };
        if input.action_count(branch) != 0 && !targets.contains(&edge) {
            control(GlaExecutionEvent::ScratchEntry)?;
            targets.insert(edge);
        }
    }
    control(GlaExecutionEvent::Work)?;
    let target_edges =
        u64::try_from(targets.len()).map_err(|_| edge_upsert_batch_invalid::<A>())?;
    let receipt = match outcomes.len() {
        0 => GraphWriteStepReceipt::EdgeUpsert {
            outcome: GraphEdgeMergeOutcome::NoInput,
        },
        1 => GraphWriteStepReceipt::EdgeUpsert {
            outcome: outcomes[0],
        },
        _ => GraphWriteStepReceipt::EdgeUpsertBatch { outcomes, created },
    };
    Ok((
        GraphWriteStepStats::EdgeUpsertBatch {
            stats,
            target_edges,
        },
        receipt,
    ))
}

// Native vertex collectors already own a distinct ordered bag and charge its
// retention before returning. Transfer it without another collection or graph
// access. Empty bags are impossible for a vertex get-or-create and refuse.
type VertexMergeProgramResult<A> = Result<
    (
        fgdb_gql::GraphVertexMergeStats,
        fgdb_gql::GraphWriteStepReceipt,
    ),
    TxnGqlError<fgdb_gql::GraphVertexMergeError<WriteTxnError, A>>,
>;

fn vertex_merge_program_result<A>(
    (stats, outcomes): (
        fgdb_gql::GraphVertexMergeStats,
        Vec<fgdb_gql::GraphVertexMergeOutcome>,
    ),
) -> VertexMergeProgramResult<A> {
    use fgdb_gql::{GqlQueryError, GraphVertexMergeError, GraphWriteStepReceipt};
    let receipt = match outcomes.len() {
        0 => {
            return Err(GqlQueryError::Source(
                GraphVertexMergeError::InvalidSourceStatistics,
            ));
        }
        1 => GraphWriteStepReceipt::VertexMerge {
            outcome: outcomes[0],
        },
        _ => GraphWriteStepReceipt::VertexMergeBatch { outcomes },
    };
    Ok((stats, receipt))
}

type VertexUpsertProgramResult<A> = Result<
    (
        fgdb_gql::GraphWriteStepStats,
        fgdb_gql::GraphWriteStepReceipt,
    ),
    TxnGqlError<fgdb_gql::GraphVertexUpsertError<WriteTxnError, A>>,
>;

fn vertex_upsert_program_result<A>(
    (stats, outcomes): (
        fgdb_gql::GraphVertexUpsertStats,
        Vec<fgdb_gql::GraphVertexMergeOutcome>,
    ),
) -> VertexUpsertProgramResult<A> {
    use fgdb_gql::{
        GqlQueryError, GraphVertexMergeError, GraphVertexUpsertError, GraphWriteStepReceipt,
        GraphWriteStepStats,
    };
    let invalid = || {
        GqlQueryError::Source(GraphVertexUpsertError::Merge(
            GraphVertexMergeError::InvalidSourceStatistics,
        ))
    };
    let target_vertices = u64::try_from(outcomes.len()).map_err(|_| invalid())?;
    let receipt = match outcomes.len() {
        0 => return Err(invalid()),
        1 => GraphWriteStepReceipt::VertexUpsert {
            outcome: outcomes[0],
        },
        _ => GraphWriteStepReceipt::VertexUpsertBatch { outcomes },
    };
    Ok((
        GraphWriteStepStats::VertexUpsertBatch {
            stats,
            target_vertices,
        },
        receipt,
    ))
}
