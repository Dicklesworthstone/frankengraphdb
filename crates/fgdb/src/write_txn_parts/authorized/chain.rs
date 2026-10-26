//! Write chains compose the native collectors and authorized staging owners.
//! Private WITH rows never mint authority, a new allowance or a commit.

use super::{
    Authority, CapabilityToken, Database, Error, Execution, Vfs, Workspace, WriteTxn,
    WriteTxnError, insert, mutation, selection,
};
use crate::write_txn::{
    FrozenCreations, MutationProgramWorkspace, MutationProjectionTail, frozen_chain,
};
use crate::{PendingRow, WriteBatch};
use fgdb_gql::algebra::{GraphValueRow, PreparedGraphPattern, ValueProjection};
use fgdb_gql::insertion::{
    GraphInsertBatch, GraphInsertError, GraphInsertPolicy, GraphInsertStats, PreparedGraphInsert,
};
use fgdb_gql::{
    GlaExecutionStats, GqlExecutionStats, GqlQueryError, GqlQueryExecution, GqlQueryPolicy,
    GraphInsertBinding, GraphInsertQueryError, GraphMutationBatch, GraphMutationError,
    GraphMutationPolicy, GraphMutationQueryError, GraphPatternFactorExecution, GraphSetSource,
    GraphWriteChainError, GraphWriteChainStep, PreparedGraphMutation, PreparedGraphPatternFactors,
    PreparedGraphWriteChain,
};
use fgdb_types::{CommitCx, EmbeddedTxnCompletion, QueryCx, TxnCx};
use fgdb_warden::PlannerPredicates;
use std::cell::RefCell;

#[path = "chain_merges.rs"]
mod merges;
#[path = "chain_vertex_merges.rs"]
mod vertices;

type Fault = GqlQueryError<GraphWriteChainError<WriteTxnError, WriteTxnError>, WriteTxnError>;
type Receipt = (
    GraphInsertStats,
    GqlQueryExecution<GraphValueRow>,
    EmbeddedTxnCompletion,
);

fn source(error: WriteTxnError) -> Fault {
    GqlQueryError::Source(GraphWriteChainError::Insert(
        GraphInsertQueryError::Insertion(GraphInsertError::Source(error)),
    ))
}

/// Authorize an original intent against native before/after images. A frozen
/// creation chain materializes only the held target or endpoints needed by
/// this intent, so admission never re-prepares the growing chain prefix.
/// These initializer rows are previously admitted creations, not new effects;
/// only the original intent enters the final ordered preparation.
pub(super) fn stage_batch<V: Vfs + Clone, Clock: FnMut() -> u64>(
    transaction: &mut WriteTxn,
    database: &mut Database<V>,
    cx: &QueryCx,
    batch: WriteBatch,
    mut frozen: Option<&mut FrozenCreations>,
    execution: &mut Execution<'_, '_, Clock>,
) -> Result<(), WriteTxnError> {
    let relation = batch.relation;
    for intent in batch.rows {
        cx.checkpoint().map_err(WriteTxnError::Interrupted)?;
        let Some(held) = frozen.as_deref_mut() else {
            super::stage(transaction, database, relation, intent, execution)?;
            continue;
        };
        // The native frozen-chain classifier admits creations and ON CREATE
        // assignments only. Refuse a future unsupported family rather than
        // silently authorizing it against an incomplete temporary overlay.
        let endpoints = match &intent {
            PendingRow::Vertex { ensure: false, .. } => [None, None],
            PendingRow::Edge {
                src,
                dst,
                ensure: false,
                ..
            } => [Some(*src), (*dst != *src).then_some(*dst)],
            PendingRow::SetLabel { vid, .. } | PendingRow::SetProperty { vid, .. }
                if held.index.contains_key(vid) =>
            {
                [Some(*vid), None]
            }
            _ => return Err(WriteTxnError::UnsupportedAtomicMutation),
        };
        let row = MutationProgramWorkspace::new(transaction);
        let mut seeds = Vec::new();
        for vertex in endpoints.into_iter().flatten() {
            let Some(&at) = held.index.get(&vertex) else {
                // Existing endpoints are already in the chain-entry overlay.
                continue;
            };
            let (vertex, labels, properties) = &held.vertices[at];
            cx.checkpoint().map_err(WriteTxnError::Interrupted)?;
            execution.checkpoint()?;
            if !execution.permit.predicates().allows_vertex(labels) {
                return Err(super::denied());
            }
            // Copies concern only the chain's own authorized creations.
            // Admit their complete payload before cloning the initializer.
            for _ in labels {
                execution.checkpoint()?;
            }
            for (_, value) in properties {
                execution.checkpoint()?;
                crate::write_txn::point_payload_admission(value, &mut |_| {
                    cx.checkpoint().map_err(WriteTxnError::Interrupted)?;
                    execution.checkpoint()
                })?;
            }
            let mut seed = WriteBatch::new(relation);
            seed.create_vertex_values(*vertex, labels.clone(), properties.clone());
            // stage_native bills the staged batch count as its input prefix.
            // Preserve its one-intent-per-batch contract for seed rows too.
            seeds.push(seed);
        }
        if !seeds.is_empty() {
            execution.work(
                u64::try_from(seeds.len())
                    .map_err(|_| WriteTxnError::Authorization(Error::TooLarge))?,
            )?;
            row.txn
                .write_ordered(database, seeds)
                .map_err(super::redacted)?;
        }
        super::stage(row.txn, database, relation, intent, execution)?;
        // Transfer the one admitted original batch without copying its payload.
        // Keep its staged slot until the guard drops: its appended-prefix test
        // must still retain the discarded preparation's read observations.
        let original = row
            .txn
            .staged
            .last_mut()
            .expect("successful authorized staging retains the original intent");
        let original = core::mem::replace(original, WriteBatch::new(relation));
        drop(row);
        held.defer(original);
    }
    Ok(())
}

fn finish_frozen<V: Vfs + Clone, Clock: FnMut() -> u64>(
    transaction: &mut WriteTxn,
    database: &mut Database<V>,
    cx: &QueryCx,
    frozen: FrozenCreations,
    execution: &mut Execution<'_, '_, Clock>,
) -> Result<(), WriteTxnError> {
    let batches = frozen.into_batches();
    let work = batches.iter().try_fold(0_u64, |used, batch| {
        u64::try_from(batch.rows.len())
            .ok()
            .and_then(|rows| used.checked_add(rows))
            .ok_or(WriteTxnError::Authorization(Error::TooLarge))
    })?;
    cx.checkpoint().map_err(WriteTxnError::Interrupted)?;
    execution.work(work)?;
    if !batches.is_empty() {
        transaction
            .write_ordered(database, batches)
            .map_err(super::redacted)?;
    }
    cx.checkpoint().map_err(WriteTxnError::Interrupted)?;
    execution.checkpoint()
}

// A compiler-created hidden head column is still a property read. Reject an
// explicitly named unavailable field before it can be copied into WITH rows.
// Complete property maps retain the ordinary source's before-read masking.
fn admit_pattern(
    pattern: &PreparedGraphPattern<GraphValueRow>,
    scope: &PlannerPredicates,
) -> Result<(), WriteTxnError> {
    for column in pattern.value_columns() {
        if let ValueProjection::Property { key, .. } | ValueProjection::EdgeProperty { key, .. } =
            column
            && !scope.allows_property(*key)
        {
            return Err(WriteTxnError::Authorization(Error::ScopeDenied));
        }
    }
    Ok(())
}

struct ChainSource<'s, 'db, 'run, 'cx, 'permit, V: Vfs, Clock> {
    transaction: &'s WriteTxn,
    database: &'s RefCell<&'db mut Database<V>>,
    cx: &'s QueryCx,
    scope: &'s PlannerPredicates,
    execution: &'s RefCell<&'run mut Execution<'cx, 'permit, Clock>>,
}

impl<V: Vfs + Clone, Clock: FnMut() -> u64> GraphSetSource<WriteTxnError, WriteTxnError>
    for ChainSource<'_, '_, '_, '_, '_, V, Clock>
{
    fn pattern(
        &mut self,
        pattern: &PreparedGraphPattern<GraphValueRow>,
        policy: GqlQueryPolicy,
    ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<WriteTxnError, WriteTxnError>> {
        admit_pattern(pattern, self.scope).map_err(GqlQueryError::Source)?;
        selection::select_overlay(
            self.transaction,
            &self.database.borrow(),
            self.cx,
            pattern,
            self.scope,
            policy,
            self.execution,
        )
    }

    fn supports_pattern_factors(&self) -> bool {
        true
    }

    fn pattern_factors(
        &mut self,
        request: &PreparedGraphPatternFactors<'_>,
        policy: GqlQueryPolicy,
    ) -> Option<Result<GraphPatternFactorExecution, GqlQueryError<WriteTxnError, WriteTxnError>>>
    {
        if let Err(error) = admit_pattern(request.original(), self.scope) {
            return Some(Err(GqlQueryError::Source(error)));
        }
        Some(selection::select_overlay_factors(
            self.transaction,
            &self.database.borrow(),
            self.cx,
            request,
            self.scope,
            policy,
            self.execution,
        ))
    }
}

fn remaining(
    policy: GraphInsertPolicy,
    used: &GraphInsertStats,
    mutation_effects: u64,
    delivers: bool,
) -> GraphInsertPolicy {
    GraphInsertPolicy::new(
        GqlQueryPolicy::new(
            policy
                .query
                .rows
                .max_snapshot_records()
                .unwrap_or(u64::MAX)
                .saturating_sub(used.selection.snapshot_records),
            if delivers {
                policy.query.rows.max_result_rows().unwrap_or(u64::MAX)
            } else {
                u64::MAX
            },
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
    .with_mutation_limit(policy.max_mutation_effects.saturating_sub(mutation_effects))
}

enum Proposal<'a> {
    Insert(&'a PreparedGraphInsert, GraphInsertBatch),
    Mutation(
        &'a PreparedGraphMutation,
        GraphMutationBatch,
        GraphMutationPolicy,
    ),
}

fn mutation_error(
    error: GraphMutationError<WriteTxnError>,
) -> GraphWriteChainError<WriteTxnError, WriteTxnError> {
    GraphWriteChainError::Mutation(GraphMutationQueryError::Mutation(error))
}

impl<V: Vfs + Clone> Database<V> {
    /// Execute creation, mutation and MERGE chains under one capability.
    ///
    /// Every graph leaf uses the same masked transaction overlay as an
    /// authorized single insertion. Every original intent passes its ordinary
    /// before/after scope checks. Identities are allocated only by the engine.
    /// The native collectors own WITH/RETURN, expressions, bag multiplicity and
    /// pagination; no text rewrite or alternate mutation evaluator is involved.
    ///
    /// Private continuation rows spend evaluator work/scratch but no signed
    /// result rows. A source-free chain without RETURN needs Write rights;
    /// graph-selected creation, a mutation or a public RETURN requires ReadWrite. Explicit
    /// hidden-property continuation reads refuse before publication. All native
    /// limits accumulate across the statement, and one live permit covers all
    /// steps and the sole commit. A failure discards every unpublished step.
    ///
    /// SET, REMOVE and DELETE steps use the same original-intent admission and
    /// native incidence proof as standalone authorized mutations. Their effect
    /// counts share one statement limit; projection and deletion proof share the
    /// same native work/scratch allowance. Restricted deletion retains the
    /// capability-only refusal that prevents probing hidden fields or incidence.
    /// Relationship MERGE keeps the native whole-input pair reducer, including
    /// parallel matches and private continuation rows. Vertex MERGE binds all
    /// visible matches and preserves repeated input occurrences and ON/SET
    /// clause order. The native creation-only chain profile holds its admitted
    /// effects and prepares them once, while later MERGEs see held vertices.
    /// Native unknown/recovery outcomes and the existing authorized insertion's
    /// durable-identity and security boundaries apply unchanged.
    #[allow(clippy::too_many_arguments)]
    pub async fn execute_graph_write_chain_authorized(
        &mut self,
        txn_cx: &TxnCx,
        query_cx: &QueryCx,
        commit_cx: &CommitCx,
        authority: &Authority,
        token: &CapabilityToken,
        branch: &str,
        chain: &PreparedGraphWriteChain,
        policy: GraphInsertPolicy,
        clock: impl FnMut() -> u64,
    ) -> Result<Receipt, Fault> {
        self.execute_graph_write_chain_authorized_checked(
            txn_cx,
            query_cx,
            commit_cx,
            authority,
            token,
            branch,
            chain,
            policy,
            clock,
            |_, _| Ok(()),
        )
        .await
    }

    /// Admit a host's final representation before the chain's sole commit.
    /// The callback receives immutable final rows once, including an empty
    /// result, and the same live signed work meter as every native step. It
    /// must buffer privately; any refusal aborts all staged effects. No
    /// fallible output conversion or authorization follows publication.
    #[allow(clippy::too_many_arguments)]
    pub async fn execute_graph_write_chain_authorized_checked(
        &mut self,
        txn_cx: &TxnCx,
        query_cx: &QueryCx,
        commit_cx: &CommitCx,
        authority: &Authority,
        token: &CapabilityToken,
        branch: &str,
        chain: &PreparedGraphWriteChain,
        policy: GraphInsertPolicy,
        clock: impl FnMut() -> u64,
        admit: impl FnOnce(
            &[GraphValueRow],
            &mut dyn FnMut(u64) -> Result<(), WriteTxnError>,
        ) -> Result<(), WriteTxnError>,
    ) -> Result<Receipt, Fault> {
        self.execute_graph_write_chain_authorized_with_read(
            txn_cx, query_cx, commit_cx, authority, token, branch, chain, None, policy, clock,
            admit,
        )
        .await
    }

    /// Execute a returning write chain and a correlated read under one live
    /// capability and one commit. Build the continuation from
    /// `chain.read_input()` with the native relational operators. It reads the
    /// complete post-write overlay through the same masked source as the write.
    /// An outer join preserves each unmatched input occurrence and extends only
    /// its new columns with NULL.
    ///
    /// ReadWrite authority is required before allocation or graph access. The
    /// write's output is private: only the continuation's final rows spend the
    /// signed and native result-row allowances. Snapshot, work and scratch
    /// limits cover both phases cumulatively. A late read, permission, budget
    /// or cancellation failure discards the entire unpublished statement.
    /// This is typed composition, with the same scope as the owner-level
    /// `execute_graph_write_chain_then_read_autocommit_engine_governed` API.
    #[allow(clippy::too_many_arguments)]
    pub async fn execute_graph_write_chain_then_read_authorized(
        &mut self,
        txn_cx: &TxnCx,
        query_cx: &QueryCx,
        commit_cx: &CommitCx,
        authority: &Authority,
        token: &CapabilityToken,
        branch: &str,
        chain: &PreparedGraphWriteChain,
        continuation: &fgdb_gql::PreparedGraphSet,
        policy: GraphInsertPolicy,
        clock: impl FnMut() -> u64,
    ) -> Result<Receipt, Fault> {
        self.execute_graph_write_chain_then_read_authorized_checked(
            txn_cx,
            query_cx,
            commit_cx,
            authority,
            token,
            branch,
            chain,
            continuation,
            policy,
            clock,
            |_, _| Ok(()),
        )
        .await
    }

    /// Admit the final read's host representation before the sole commit.
    /// The callback has the same private-buffer and live-meter contract as
    /// `execute_graph_write_chain_authorized_checked`; it never receives the
    /// intermediate write rows. Returned statistics include both phases.
    #[allow(clippy::too_many_arguments)]
    pub async fn execute_graph_write_chain_then_read_authorized_checked(
        &mut self,
        txn_cx: &TxnCx,
        query_cx: &QueryCx,
        commit_cx: &CommitCx,
        authority: &Authority,
        token: &CapabilityToken,
        branch: &str,
        chain: &PreparedGraphWriteChain,
        continuation: &fgdb_gql::PreparedGraphSet,
        policy: GraphInsertPolicy,
        clock: impl FnMut() -> u64,
        admit: impl FnOnce(
            &[GraphValueRow],
            &mut dyn FnMut(u64) -> Result<(), WriteTxnError>,
        ) -> Result<(), WriteTxnError>,
    ) -> Result<Receipt, Fault> {
        self.execute_graph_write_chain_authorized_with_read(
            txn_cx,
            query_cx,
            commit_cx,
            authority,
            token,
            branch,
            chain,
            Some(continuation),
            policy,
            clock,
            admit,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn execute_graph_write_chain_authorized_with_read(
        &mut self,
        txn_cx: &TxnCx,
        query_cx: &QueryCx,
        commit_cx: &CommitCx,
        authority: &Authority,
        token: &CapabilityToken,
        branch: &str,
        chain: &PreparedGraphWriteChain,
        continuation: Option<&fgdb_gql::PreparedGraphSet>,
        policy: GraphInsertPolicy,
        mut clock: impl FnMut() -> u64,
        admit: impl FnOnce(
            &[GraphValueRow],
            &mut dyn FnMut(u64) -> Result<(), WriteTxnError>,
        ) -> Result<(), WriteTxnError>,
    ) -> Result<Receipt, Fault> {
        if authority.namespace() != self.keys.namespace {
            return Err(source(WriteTxnError::Authorization(Error::WrongAuthority)));
        }
        let now = clock();
        let verified = authority
            .verify_at(token, branch, now)
            .map_err(|error| source(WriteTxnError::Authorization(error)))?;
        let permit = verified
            .begin_write_at(branch, now)
            .map_err(|error| source(WriteTxnError::Authorization(error)))?;
        let mut execution = Execution {
            cx: commit_cx,
            permit,
            clock,
        };
        let scope = verified.predicates();
        let continuation = continuation.or_else(|| chain.read_continuation());
        if continuation.is_some_and(|read| !chain.accepts_read_continuation(read)) {
            return Err(GqlQueryError::Source(GraphWriteChainError::Read(
                fgdb_gql::GraphSetExecutionError::InputSchema { operand: 0 },
            )));
        }
        // A terminal read's explicitly named field is a textual read, refused
        // out of scope like a field named in a write's RETURN.
        if chain
            .read_element_keys()
            .iter()
            .any(|key| !scope.allows_property(*key))
        {
            return Err(source(WriteTxnError::Authorization(Error::ScopeDenied)));
        }
        // Whole-program admission precedes even the first engine ID. The
        // chain is compiler-owned, but its unsupported family is not authority.
        let mut reads = chain.returns_rows() || continuation.is_some();
        for (step, statement) in chain.steps().iter().enumerate() {
            execution.checkpoint().map_err(source)?;
            if merges::preflight(statement, scope)? || vertices::preflight(statement, scope)? {
                reads = true;
                continue;
            }
            let mutation = match statement {
                GraphWriteChainStep::Mutation(mutation) => Some(mutation.as_ref()),
                GraphWriteChainStep::MutationQuery(query) => Some(query.mutation()),
                _ => None,
            };
            if let Some(mutation) = mutation {
                // Match the standalone mutation contract even when the target
                // identity comes from a creation earlier in this statement.
                reads = true;
                if let Some(pattern) = mutation.selection() {
                    admit_pattern(pattern, scope).map_err(source)?;
                }
                continue;
            }
            let insertion = match statement {
                GraphWriteChainStep::Insert(insertion) => insertion,
                GraphWriteChainStep::Query(query) => {
                    for binding in query.bindings() {
                        execution.checkpoint().map_err(source)?;
                        if let GraphInsertBinding::VertexProperty { key, .. }
                        | GraphInsertBinding::EdgeProperty { key, .. } = binding
                            && !scope.allows_property(*key)
                        {
                            return Err(source(WriteTxnError::Authorization(Error::ScopeDenied)));
                        }
                    }
                    query.insertion()
                }
                _ => {
                    return Err(source(WriteTxnError::AuthorizedChainStepUnsupported {
                        step,
                    }));
                }
            };
            reads |= insertion.requires_read();
            if let Some(pattern) = insertion.selection() {
                admit_pattern(pattern, scope).map_err(source)?;
            }
        }
        if reads && !scope.rights().can_read() {
            return Err(source(WriteTxnError::Authorization(
                Error::PermissionDenied,
            )));
        }
        commit_cx
            .with_restriction_async(async {
                let mut workspace = Workspace(Some(
                    self.begin(txn_cx)
                        .map_err(|error| source(WriteTxnError::Write(error)))?,
                ));
                workspace.transaction().program_multi_relation = true;
                let mut used = GraphInsertStats {
                    selection: GqlExecutionStats {
                        snapshot_records: 0,
                        result_rows: 0,
                    },
                    evaluator: GlaExecutionStats::default(),
                    created_vertices: 0,
                    created_edges: 0,
                };
                // The complete definition was authorized above. Input
                // materialization now spends both the query's cumulative
                // allowance and the live signed work meter before copying.
                let mut rows = match chain.initial_rows_governed(policy.query, || {
                    query_cx.checkpoint().map_err(WriteTxnError::Interrupted)?;
                    execution.checkpoint()
                })? {
                    Some(input) => {
                        used.evaluator = input.evaluator;
                        Some(input.value)
                    }
                    None => None,
                };
                let mut result = None;
                let mut mutation_effects = 0;
                let last = chain.steps().len().saturating_sub(1);
                let mut frozen = frozen_chain(chain).then(FrozenCreations::default);
                for (at, statement) in chain.steps().iter().enumerate() {
                    let allowance = remaining(
                        policy,
                        &used,
                        mutation_effects,
                        at == last && chain.returns_rows() && continuation.is_none(),
                    );
                    let (stats, returning, effects) = if matches!(
                        statement,
                        GraphWriteChainStep::VertexMerge(_)
                            | GraphWriteChainStep::VertexMergeQuery(_)
                    ) {
                        vertices::execute(
                            workspace.transaction(),
                            self,
                            query_cx,
                            statement,
                            allowance,
                            rows.take(),
                            scope,
                            &mut execution,
                            frozen.as_mut(),
                            at != last,
                        )?
                    } else if matches!(
                        statement,
                        GraphWriteChainStep::EdgeUpsert(_)
                            | GraphWriteChainStep::EdgeUpsertRows { .. }
                            | GraphWriteChainStep::EdgeUpsertQuery { .. }
                    ) {
                        merges::execute_actions(
                            workspace.transaction(),
                            self,
                            query_cx,
                            statement,
                            allowance,
                            rows.take(),
                            scope,
                            &mut execution,
                        )?
                    } else if matches!(
                        statement,
                        GraphWriteChainStep::EdgeMerge(_)
                            | GraphWriteChainStep::EdgeMergeRows { .. }
                            | GraphWriteChainStep::EdgeMergeQuery(_)
                    ) {
                        let (stats, returning) = merges::execute(
                            workspace.transaction(),
                            self,
                            query_cx,
                            statement,
                            allowance,
                            rows.take(),
                            scope,
                            &mut execution,
                            frozen.as_mut(),
                        )?;
                        (stats, returning, 0)
                    } else if let GraphWriteChainStep::Query(query) = statement {
                        let collection = query_cx
                            .with_restriction(|| {
                                let controls = RefCell::new(&mut execution);
                                let database = RefCell::new(&mut *self);
                                query.collect_governed_over(
                                    rows.take(),
                                    allowance,
                                    ChainSource {
                                        transaction: workspace.transaction(),
                                        database: &database,
                                        cx: query_cx,
                                        scope,
                                        execution: &controls,
                                    },
                                    |request| {
                                        database.borrow_mut().allocate_identity(query_cx, request)
                                    },
                                    || {
                                        query_cx
                                            .checkpoint()
                                            .map_err(WriteTxnError::Interrupted)?;
                                        controls.borrow_mut().checkpoint()
                                    },
                                )
                            })
                            .map_err(|error| error.map_source(GraphWriteChainError::Insert))?;
                        let (creation, output) = collection.into_parts();
                        // The frozen-chain classifier excludes projected
                        // CREATE. Its complete bag must be visible to output.
                        let (stats, _, _) = insert::stage_proposal(
                            workspace.transaction(),
                            self,
                            query_cx,
                            query.insertion(),
                            creation,
                            &mut execution,
                            false,
                            None,
                        )
                        .map_err(|error| {
                            error.map_source(|error| {
                                GraphWriteChainError::Insert(GraphInsertQueryError::Insertion(
                                    error,
                                ))
                            })
                        })?;
                        let returning = insert::finish_output(
                            workspace.transaction(),
                            self,
                            query_cx,
                            output,
                            scope,
                            &mut execution,
                        )
                        .map_err(|error| {
                            error.map_source(|error| {
                                GraphWriteChainError::Insert(GraphInsertQueryError::Returning(
                                    error,
                                ))
                            })
                        })?;
                        (stats, returning, 0)
                    } else {
                        let (proposal, mut returning) = query_cx.with_restriction(|| {
                            let controls = RefCell::new(&mut execution);
                            let database = RefCell::new(&mut *self);
                            let mut graph = ChainSource {
                                transaction: workspace.transaction(),
                                database: &database,
                                cx: query_cx,
                                scope,
                                execution: &controls,
                            };
                            let transaction = graph.transaction;
                            let elements = |element, key| {
                                // A named hidden field retains the chain's
                                // explicit-refusal contract. Allowed keys still
                                // admit arbitrary carried identities and both
                                // edge endpoints before reading a property.
                                if !scope.allows_property(key) {
                                    return Err(WriteTxnError::Authorization(Error::ScopeDenied));
                                }
                                selection::mutation_element_property(
                                    transaction,
                                    &database.borrow(),
                                    query_cx,
                                    element,
                                    key,
                                    scope,
                                    &controls,
                                )
                            };
                            let allocate = |request| {
                                database.borrow_mut().allocate_identity(query_cx, request)
                            };
                            let checkpoint = || {
                                query_cx.checkpoint().map_err(WriteTxnError::Interrupted)?;
                                controls.borrow_mut().checkpoint()
                            };
                            match statement {
                                GraphWriteChainStep::Insert(insertion) => {
                                    let proposal = insertion
                                        .execute_governed_over(
                                            rows.take(),
                                            allowance,
                                            graph,
                                            allocate,
                                            checkpoint,
                                        )
                                        .map_err(|error| {
                                            error.map_source(|error| {
                                                GraphWriteChainError::Insert(
                                                    GraphInsertQueryError::Insertion(error),
                                                )
                                            })
                                        })?;
                                    let stats = proposal.stats();
                                    let returning = GqlQueryExecution {
                                        value: Vec::new(),
                                        rows: GqlExecutionStats {
                                            snapshot_records: stats.selection.snapshot_records,
                                            result_rows: 0,
                                        },
                                        evaluator: stats.evaluator,
                                    };
                                    Ok((Proposal::Insert(insertion, proposal), returning))
                                }
                                GraphWriteChainStep::Mutation(mutation) => {
                                    let policy = GraphMutationPolicy::new(
                                        allowance.query,
                                        allowance.max_mutation_effects,
                                    );
                                    let proposal = mutation
                                        .execute_governed_over_reading(
                                            rows.take(),
                                            policy,
                                            |pattern, budget| graph.pattern(pattern, budget),
                                            elements,
                                            checkpoint,
                                        )
                                        .map_err(|error| error.map_source(mutation_error))?;
                                    let stats = proposal.stats();
                                    let returning = GqlQueryExecution {
                                        value: Vec::new(),
                                        rows: GqlExecutionStats {
                                            snapshot_records: stats.selection.snapshot_records,
                                            result_rows: 0,
                                        },
                                        evaluator: stats.evaluator,
                                    };
                                    Ok((Proposal::Mutation(mutation, proposal, policy), returning))
                                }
                                GraphWriteChainStep::MutationQuery(query) => {
                                    let policy = GraphMutationPolicy::new(
                                        allowance.query,
                                        allowance.max_mutation_effects,
                                    );
                                    let proposal = query
                                        .execute_governed_over_reading(
                                            rows.take(),
                                            policy,
                                            |pattern, budget| graph.pattern(pattern, budget),
                                            elements,
                                            checkpoint,
                                        )
                                        .map_err(|error| {
                                            error.map_source(GraphWriteChainError::Mutation)
                                        })?;
                                    let (mutation, returning) = proposal.into_parts();
                                    Ok((
                                        Proposal::Mutation(query.mutation(), mutation, policy),
                                        returning,
                                    ))
                                }
                                _ => Err(source(WriteTxnError::AuthorizedChainStepUnsupported {
                                    step: at,
                                })),
                            }
                        })?;
                        let (stats, effects) = match proposal {
                            Proposal::Insert(insertion, proposal) => {
                                let (stats, _, _) = insert::stage_proposal(
                                    workspace.transaction(),
                                    self,
                                    query_cx,
                                    insertion,
                                    proposal,
                                    &mut execution,
                                    false,
                                    frozen.as_mut(),
                                )
                                .map_err(|error| {
                                    error.map_source(|error| {
                                        GraphWriteChainError::Insert(
                                            GraphInsertQueryError::Insertion(error),
                                        )
                                    })
                                })?;
                                (stats, 0)
                            }
                            Proposal::Mutation(mutation, proposal, policy) => {
                                let (staging_policy, tail) = MutationProjectionTail::reserve(
                                    policy,
                                    proposal.stats(),
                                    &returning,
                                )
                                .map_err(|error| GqlQueryError::Source(mutation_error(error)))?;
                                let (staged, _, _) = mutation::stage_proposal(
                                    workspace.transaction(),
                                    self,
                                    query_cx,
                                    mutation,
                                    proposal,
                                    staging_policy,
                                    scope,
                                    &mut execution,
                                    false,
                                )
                                .map_err(|error| error.map_source(mutation_error))?;
                                tail.complete(staged, &mut returning).map_err(|error| {
                                    GqlQueryError::Source(mutation_error(error))
                                })?;
                                (
                                    GraphInsertStats {
                                        selection: returning.rows,
                                        evaluator: returning.evaluator,
                                        created_vertices: 0,
                                        created_edges: 0,
                                    },
                                    staged.effects,
                                )
                            }
                        };

                        (stats, returning, effects)
                    };
                    let observed = u128::from(mutation_effects) + u128::from(effects);
                    if observed > u128::from(policy.max_mutation_effects) {
                        return Err(GqlQueryError::Source(mutation_error(
                            GraphMutationError::EffectLimit {
                                limit: policy.max_mutation_effects,
                                observed,
                            },
                        )));
                    }
                    mutation_effects = observed as u64;
                    // Query counters include their write prefix and complete
                    // WITH tail. Do not lose that work or count it twice.
                    used.selection.snapshot_records = used
                        .selection
                        .snapshot_records
                        .saturating_add(returning.rows.snapshot_records);
                    used.evaluator.work_units = used
                        .evaluator
                        .work_units
                        .saturating_add(returning.evaluator.work_units);
                    used.evaluator.scratch_entries = used
                        .evaluator
                        .scratch_entries
                        .saturating_add(returning.evaluator.scratch_entries);
                    used.created_vertices =
                        used.created_vertices.saturating_add(stats.created_vertices);
                    used.created_edges = used.created_edges.saturating_add(stats.created_edges);
                    if at == last {
                        result = Some(returning);
                    } else {
                        rows = Some(returning.value);
                    }
                }
                if let Some(frozen) = frozen {
                    finish_frozen(
                        workspace.transaction(),
                        self,
                        query_cx,
                        frozen,
                        &mut execution,
                    )
                    .map_err(source)?;
                }
                let Some(mut result) = result else {
                    return Err(source(WriteTxnError::NoPreparedWrite));
                };
                // A continuation is an input to another write, never an
                // implicit public RETURN. Keep this boundary explicit even
                // when a future final step retains private rows internally.
                if !chain.returns_rows() {
                    result.value.clear();
                    result.rows.result_rows = 0;
                }
                result.rows.snapshot_records = used.selection.snapshot_records;
                result.evaluator = used.evaluator;
                if let Some(continuation) = continuation {
                    result = query_cx.with_restriction(|| {
                        let controls = RefCell::new(&mut execution);
                        let database = RefCell::new(&mut *self);
                        let transaction = workspace.transaction();
                        let graph = fgdb_gql::WithElementReads {
                            source: ChainSource {
                                transaction,
                                database: &database,
                                cx: query_cx,
                                scope,
                                execution: &controls,
                            },
                            elements: |element, key| {
                                selection::returning_element_property(
                                    transaction,
                                    &database.borrow(),
                                    query_cx,
                                    element,
                                    key,
                                    scope,
                                    &controls,
                                )
                            },
                        };
                        chain.finish_read_continuation(
                            continuation,
                            result,
                            policy.query,
                            graph,
                            || {
                                query_cx.checkpoint().map_err(WriteTxnError::Interrupted)?;
                                controls.borrow_mut().checkpoint()
                            },
                        )
                    })?;
                    used.selection.snapshot_records = result.rows.snapshot_records;
                    used.evaluator = result.evaluator;
                }
                used.selection.result_rows = result.rows.result_rows;
                let delivered = u64::try_from(result.value.len())
                    .map_err(|_| source(WriteTxnError::Authorization(Error::TooLarge)))?;
                execution
                    .permit
                    .charge_rows_at((execution.clock)(), delivered)
                    .map_err(|error| source(WriteTxnError::Authorization(error)))?;
                query_cx
                    .with_restriction(|| {
                        admit(&result.value, &mut |work| {
                            query_cx.checkpoint().map_err(WriteTxnError::Interrupted)?;
                            execution.work(work)
                        })
                    })
                    .map_err(source)?;
                execution.bind_decision(workspace.transaction());
                let completion = workspace
                    .transaction()
                    .complete_controlled(self, commit_cx, None, false, || {
                        query_cx.checkpoint().map_err(WriteTxnError::Interrupted)?;
                        execution.checkpoint()
                    })
                    .await
                    .map_err(source)?;
                Ok((used, result, completion))
            })
            .await
    }
}
