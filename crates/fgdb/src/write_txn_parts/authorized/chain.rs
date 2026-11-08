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
    GlaExecutionEvent, GlaExecutionStats, GlaLimitDimension, GlaLimitExceeded, GqlExecutionStats,
    GqlQueryError, GqlQueryExecution, GqlQueryPolicy, GraphInsertBinding, GraphInsertQueryError,
    GraphMutationBatch, GraphMutationError, GraphMutationPolicy, GraphMutationQueryError,
    GraphPatternFactorExecution, GraphSetSource, GraphWriteChainCursor, GraphWriteChainError,
    GraphWriteChainStep, GraphWriteChainTask, PreparedGraphMutation, PreparedGraphPatternFactors,
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
type BatchFault =
    fgdb_gql::GraphWriteScriptExecutionError<WriteTxnError, WriteTxnError, WriteTxnError>;

fn source(error: WriteTxnError) -> Fault {
    GqlQueryError::Source(GraphWriteChainError::Insert(
        GraphInsertQueryError::Insertion(GraphInsertError::Source(error)),
    ))
}

pub(super) fn batch_error(
    source: Fault,
    argument_set: Option<usize>,
    source_len: usize,
) -> BatchFault {
    BatchFault::BatchChain {
        location: argument_set.map(|argument_set| fgdb_gql::GraphWriteScriptBatchLocation {
            argument_set,
            statement: 0,
            span: 0..source_len,
        }),
        source,
    }
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

/// Visit the complete typed program before any identity allocation or source
/// evaluation. Loop bodies use this same declaration admission even when their
/// eventual input or list is empty; carrying a private row grants no authority.
fn preflight_steps<Clock: FnMut() -> u64>(
    steps: &[GraphWriteChainStep],
    query_cx: &QueryCx,
    scope: &PlannerPredicates,
    execution: &mut Execution<'_, '_, Clock>,
) -> Result<bool, Fault> {
    let mut reads = false;
    for (step, statement) in steps.iter().enumerate() {
        execution.checkpoint().map_err(source)?;
        if let GraphWriteChainStep::Foreach(foreach) = statement {
            if foreach
                .read_element_keys()
                .iter()
                .any(|key| !scope.allows_property(*key))
            {
                return Err(source(WriteTxnError::Authorization(Error::ScopeDenied)));
            }
            foreach
                .admit_patterns(|pattern| admit_pattern(pattern, scope))
                .map_err(source)?;
            reads |= foreach.requires_read();
            reads |= preflight_chain(foreach.body(), None, query_cx, scope, execution)?;
            continue;
        }
        if let GraphWriteChainStep::UnitCall(call) = statement {
            if call
                .read_element_keys()
                .iter()
                .any(|key| !scope.allows_property(*key))
            {
                return Err(source(WriteTxnError::Authorization(Error::ScopeDenied)));
            }
            call.admit_patterns(|pattern| admit_pattern(pattern, scope))
                .map_err(source)?;
            reads |= call.requires_read();
            reads |= preflight_chain(call.body(), None, query_cx, scope, execution)?;
            continue;
        }
        if let GraphWriteChainStep::ReturningCall(call) = statement {
            if call
                .read_element_keys()
                .iter()
                .any(|key| !scope.allows_property(*key))
            {
                return Err(source(WriteTxnError::Authorization(Error::ScopeDenied)));
            }
            call.admit_patterns(|pattern| admit_pattern(pattern, scope))
                .map_err(source)?;
            // Exports remain private inputs to the surrounding statement.
            // The call reports actual graph/element demand, including its
            // body's terminal read, separately from its returned bindings.
            reads |= call.requires_read();
            reads |= preflight_chain(call.body(), None, query_cx, scope, execution)?;
            continue;
        }
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
            mutation::admit_declared_mutation_fields(query_cx, mutation, execution).map_err(
                |error| {
                    GqlQueryError::Source(GraphWriteChainError::Mutation(
                        GraphMutationQueryError::Mutation(GraphMutationError::Source(error)),
                    ))
                },
            )?;
            mutation
                .admit_patterns(|pattern| admit_pattern(pattern, scope))
                .map_err(source)?;
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
    Ok(reads)
}

/// Admit each chain's terminal read as well as its writes. A returning CALL's
/// body has its own read tail and named property keys, even when its caller
/// selects no occurrences. Public delivery authority belongs to the outer
/// statement; a scope's `requires_read` accounts for its private read demand.
fn preflight_chain<Clock: FnMut() -> u64>(
    chain: &PreparedGraphWriteChain,
    continuation: Option<&fgdb_gql::PreparedGraphSet>,
    query_cx: &QueryCx,
    scope: &PlannerPredicates,
    execution: &mut Execution<'_, '_, Clock>,
) -> Result<bool, Fault> {
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
    if let Some(read) = continuation {
        read.admit_patterns(&mut |pattern| {
            execution.checkpoint()?;
            admit_pattern(pattern, scope)
        })
        .map_err(source)?;
    }
    // Whole-program admission precedes even the first engine ID. The
    // chain is compiler-owned, but its unsupported family is not authority.
    preflight_steps(chain.steps(), query_cx, scope, execution)
}

/// Admit the complete native definition without reading the database or
/// reserving identities. Prepared sessions use the same recursive admission
/// as execution; an empty loop cannot hide a forbidden operation or field.
pub(super) fn admit_definition<Clock: FnMut() -> u64>(
    chain: &PreparedGraphWriteChain,
    continuation: Option<&fgdb_gql::PreparedGraphSet>,
    query_cx: &QueryCx,
    scope: &PlannerPredicates,
    execution: &mut Execution<'_, '_, Clock>,
) -> Result<(), Fault> {
    let continuation = continuation.or_else(|| chain.read_continuation());
    let definition_reads = preflight_chain(chain, continuation, query_cx, scope, execution)?;
    let reads = chain.returns_rows() || continuation.is_some() || definition_reads;
    if reads && !scope.rights().can_read() {
        return Err(source(WriteTxnError::Authorization(
            Error::PermissionDenied,
        )));
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

    fn supports_seeded_patterns(&self) -> bool {
        self.scope.sees_all_incidence() && self.scope.sees_all_fields()
    }

    fn seeded_pattern(
        &mut self,
        pattern: &PreparedGraphPattern<GraphValueRow>,
        seeds: fgdb_gql::GraphSeeds<'_>,
        policy: GqlQueryPolicy,
    ) -> Option<Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<WriteTxnError, WriteTxnError>>>
    {
        if !self.supports_seeded_patterns() {
            return None;
        }
        if let Err(error) = admit_pattern(pattern, self.scope) {
            return Some(Err(GqlQueryError::Source(error)));
        }
        selection::select_seeded_overlay(
            self.transaction,
            &self.database.borrow(),
            self.cx,
            pattern,
            seeds,
            self.scope,
            policy,
            self.execution,
        )
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

    fn element_property(
        &mut self,
        element: fgdb_delta_types::ElementId,
        key: fgdb_delta_types::PropertyKeyId,
    ) -> Option<Result<Option<fgdb_types::CanonicalPropertyValue>, WriteTxnError>> {
        if !self.scope.allows_property(key) {
            return Some(Err(WriteTxnError::Authorization(Error::ScopeDenied)));
        }
        Some(selection::returning_element_property(
            self.transaction,
            &self.database.borrow(),
            self.cx,
            element,
            key,
            self.scope,
            self.execution,
        ))
    }

    fn element_metadata(
        &mut self,
        element: fgdb_delta_types::ElementId,
        kind: fgdb_gql::GraphElementMetadata,
        catalog: &fgdb_gql::ReverseSymbolCatalog,
        control: &mut dyn FnMut(fgdb_gql::GlaExecutionEvent) -> Result<(), ()>,
    ) -> Option<
        Result<
            Option<fgdb_gql::algebra::GraphValue>,
            fgdb_gql::GraphElementMetadataError<WriteTxnError>,
        >,
    > {
        Some(selection::returning_element_metadata(
            self.transaction,
            &self.database.borrow(),
            self.cx,
            element,
            kind,
            catalog,
            self.scope,
            self.execution,
            control,
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

/// Scheduling a loop and copying its private occurrences spend the same live
/// signed work permit and cumulative native evaluator allowance as its writes.
fn cursor_event<Clock: FnMut() -> u64>(
    cx: &QueryCx,
    execution: &mut Execution<'_, '_, Clock>,
    policy: GqlQueryPolicy,
    stats: &mut GlaExecutionStats,
    event: GlaExecutionEvent,
) -> Result<(), Fault> {
    cx.checkpoint()
        .map_err(WriteTxnError::Interrupted)
        .map_err(GqlQueryError::Interrupted)?;
    execution.checkpoint().map_err(GqlQueryError::Interrupted)?;
    let work = u128::from(stats.work_units) + 1;
    let scratch =
        u128::from(stats.scratch_entries) + u128::from(event == GlaExecutionEvent::ScratchEntry);
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
            return Err(GqlQueryError::Evaluator(GlaLimitExceeded {
                dimension,
                limit,
                observed,
            }));
        }
    }
    stats.work_units = work as u64;
    stats.scratch_entries = scratch as u64;
    Ok(())
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
        let execution = Execution {
            cx: commit_cx,
            permit,
            clock,
        };
        self.complete_authorized_chain(
            txn_cx,
            query_cx,
            commit_cx,
            chain,
            continuation,
            policy,
            verified.predicates(),
            execution,
            |_, rows, work| admit(rows, work),
        )
        .await
    }

    /// Execute a finite batch of independently bound, non-returning chains in
    /// argument-set order. Each statement sees the preceding statements' graph
    /// effects but none of their local variables. One live capability, one
    /// cumulative native allowance and one Chronicle completion cover the batch.
    /// The complete definitions are admitted before graph access or identity
    /// allocation. A failure never commits an earlier input record.
    ///
    /// `source_len` is the caller's original template length in UTF-8 bytes,
    /// before parameter-map lowering. It supplies diagnostics only: every
    /// independently bound record is statement zero of that template.
    ///
    /// The host admits its final statistics representation before publication.
    /// The callback must keep its output private and must not publish or retry.
    #[allow(clippy::too_many_arguments)]
    pub async fn execute_graph_write_chain_batch_authorized_checked(
        &mut self,
        txn_cx: &TxnCx,
        query_cx: &QueryCx,
        commit_cx: &CommitCx,
        authority: &Authority,
        token: &CapabilityToken,
        branch: &str,
        chains: &[PreparedGraphWriteChain],
        source_len: usize,
        max_statements: usize,
        policy: fgdb_gql::GraphWriteProgramPolicy,
        mut clock: impl FnMut() -> u64,
        admit: impl FnOnce(
            &fgdb_gql::GraphWriteProgramStats,
            &mut dyn FnMut(u64) -> Result<(), WriteTxnError>,
        ) -> Result<(), WriteTxnError>,
    ) -> Result<(fgdb_gql::GraphWriteProgramStats, EmbeddedTxnCompletion), Box<BatchFault>> {
        let refusal = |error| batch_error(source(error), None, source_len);
        if authority.namespace() != self.keys.namespace {
            return Err(Box::new(refusal(WriteTxnError::Authorization(
                Error::WrongAuthority,
            ))));
        }
        let now = clock();
        let verified = authority
            .verify_at(token, branch, now)
            .map_err(|error| refusal(WriteTxnError::Authorization(error)))?;
        let permit = verified
            .begin_write_at(branch, now)
            .map_err(|error| refusal(WriteTxnError::Authorization(error)))?;
        let mut execution = Execution {
            cx: commit_cx,
            permit,
            clock,
        };
        query_cx
            .checkpoint()
            .map_err(WriteTxnError::Interrupted)
            .map_err(refusal)?;
        execution.checkpoint().map_err(refusal)?;
        if chains.is_empty() {
            return Err(Box::new(BatchFault::BatchBinding(
                fgdb_gql::GraphWriteScriptBatchError::Empty,
            )));
        }
        let max_statements =
            max_statements.min(fgdb_gql::PreparedGraphWriteScript::MAX_BATCH_STATEMENTS);
        if chains.len() > max_statements {
            return Err(Box::new(BatchFault::BatchBinding(
                fgdb_gql::GraphWriteScriptBatchError::TooManyStatements {
                    limit: max_statements,
                    observed: chains.len() as u128,
                },
            )));
        }
        for (argument_set, chain) in chains.iter().enumerate() {
            let refusal = |error| batch_error(source(error), Some(argument_set), source_len);
            execution.checkpoint().map_err(refusal)?;
            if chain.returns_rows() {
                return Err(Box::new(refusal(WriteTxnError::AuthorizedMutationRefused)));
            }
        }
        let mut stats = None;
        let mut active_record = None;
        let (_, _, completion) = self
            .complete_authorized_chains(
                txn_cx,
                query_cx,
                commit_cx,
                chains,
                None,
                policy.insertion_policy(),
                verified.predicates(),
                execution,
                &mut active_record,
                |value, _, work| {
                    admit(&value, work)?;
                    stats = Some(value);
                    Ok(())
                },
            )
            .await
            .map_err(|error| batch_error(error, active_record, source_len))?;
        Ok((
            stats.expect("batch statistics admitted before publication"),
            completion,
        ))
    }

    /// Continue native text admission with its existing live permit. The same
    /// preflight, workspace, output admission and completion own every chain.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn complete_authorized_chain<Clock: FnMut() -> u64>(
        &mut self,
        txn_cx: &TxnCx,
        query_cx: &QueryCx,
        commit_cx: &CommitCx,
        chain: &PreparedGraphWriteChain,
        continuation: Option<&fgdb_gql::PreparedGraphSet>,
        policy: GraphInsertPolicy,
        scope: &PlannerPredicates,
        execution: Execution<'_, '_, Clock>,
        admit: impl FnOnce(
            fgdb_gql::GraphWriteProgramStats,
            &[GraphValueRow],
            &mut dyn FnMut(u64) -> Result<(), WriteTxnError>,
        ) -> Result<(), WriteTxnError>,
    ) -> Result<Receipt, Fault> {
        let mut active_record = None;
        self.complete_authorized_chains(
            txn_cx,
            query_cx,
            commit_cx,
            core::slice::from_ref(chain),
            continuation,
            policy,
            scope,
            execution,
            &mut active_record,
            admit,
        )
        .await
    }

    /// Execute independently bound statements in input order over one private
    /// transaction. Statement-local rows never cross a boundary; the canonical
    /// graph overlay and all native and signed allowances do. The full batch is
    /// admitted before opening its workspace, and only the final completion
    /// may publish. A multi-statement batch has no implicit RETURN channel.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn complete_authorized_chains<Clock: FnMut() -> u64>(
        &mut self,
        txn_cx: &TxnCx,
        query_cx: &QueryCx,
        commit_cx: &CommitCx,
        chains: &[PreparedGraphWriteChain],
        continuation: Option<&fgdb_gql::PreparedGraphSet>,
        policy: GraphInsertPolicy,
        scope: &PlannerPredicates,
        mut execution: Execution<'_, '_, Clock>,
        active_record: &mut Option<usize>,
        admit: impl FnOnce(
            fgdb_gql::GraphWriteProgramStats,
            &[GraphValueRow],
            &mut dyn FnMut(u64) -> Result<(), WriteTxnError>,
        ) -> Result<(), WriteTxnError>,
    ) -> Result<Receipt, Fault> {
        *active_record = None;
        if chains.is_empty()
            || (chains.len() != 1
                && (continuation.is_some() || chains.iter().any(|chain| chain.returns_rows())))
        {
            return Err(source(WriteTxnError::AuthorizedMutationRefused));
        }
        for (argument_set, chain) in chains.iter().enumerate() {
            *active_record = Some(argument_set);
            admit_definition(chain, continuation, query_cx, scope, &mut execution)?;
        }
        // Opening the workspace is a whole-batch boundary, not part of the
        // last definition's admission. The same reset precedes completion.
        *active_record = None;
        commit_cx
            .with_restriction_async(async {
                let mut workspace = Workspace(Some(
                    self.begin(txn_cx)
                        .map_err(|error| source(WriteTxnError::Write(error)))?,
                ));
                workspace.transaction().program_multi_relation = true;
                workspace.transaction().chain_retirements = Some(Default::default());
                let mut used = GraphInsertStats {
                    selection: GqlExecutionStats {
                        snapshot_records: 0,
                        result_rows: 0,
                    },
                    evaluator: GlaExecutionStats::default(),
                    created_vertices: 0,
                    created_edges: 0,
                };
                let mut mutation_effects = 0;
                let mut last_result = None;
                for (argument_set, chain) in chains.iter().enumerate() {
                *active_record = Some(argument_set);
                let continuation = continuation.or_else(|| chain.read_continuation());
                // The complete definition was authorized above. Input
                // materialization now spends both the query's cumulative
                // allowance and the live signed work meter before copying.
                let input_policy = remaining(policy, &used, mutation_effects, false).query;
                let mut rows = match chain.initial_rows_governed(input_policy, || {
                    query_cx.checkpoint().map_err(WriteTxnError::Interrupted)?;
                    execution.checkpoint()
                })? {
                    Some(input) => {
                        used.evaluator.work_units = used
                            .evaluator
                            .work_units
                            .saturating_add(input.evaluator.work_units);
                        used.evaluator.scratch_entries = used
                            .evaluator
                            .scratch_entries
                            .saturating_add(input.evaluator.scratch_entries);
                        Some(input.value)
                    }
                    None => None,
                };
                // Independent creation loops have no reads of their held
                // prefix. stage_batch still checks every original intent's
                // native before/after images on this same live permit before
                // the next iteration, then prepares the ordered effects once.
                let mut frozen = (frozen_chain(chain)
                    || chain.independent_create_relation().is_some())
                .then(FrozenCreations::default);
                let mut cursor = GraphWriteChainCursor::new(chain, continuation.is_none());
                let mut at = 0;
                while let Some(task) = cursor.next(&mut rows, &mut |event| {
                    cursor_event(
                        query_cx,
                        &mut execution,
                        policy.query,
                        &mut used.evaluator,
                        event,
                    )
                })? {
                    if let GraphWriteChainTask::Read(read) = &task {
                        let allowance =
                            remaining(policy, &used, mutation_effects, read.delivers_rows());
                        let returning = query_cx
                            .with_restriction(|| {
                                let controls = RefCell::new(&mut execution);
                                let database = RefCell::new(&mut *self);
                                let graph = ChainSource {
                                    transaction: workspace.transaction(),
                                    database: &database,
                                    cx: query_cx,
                                    scope,
                                    execution: &controls,
                                };
                                let checkpoint = || {
                                    query_cx.checkpoint().map_err(WriteTxnError::Interrupted)?;
                                    controls.borrow_mut().checkpoint()
                                };
                                read.execute_governed(
                                    rows.take(),
                                    allowance.query,
                                    graph,
                                    checkpoint,
                                )
                            })
                            .map_err(|error| error.map_source(GraphWriteChainError::Read))?;
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
                        rows = Some(returning.value);
                        continue;
                    }
                    let GraphWriteChainTask::Write {
                        step: statement,
                        continues,
                        delivers,
                    } = task
                    else {
                        unreachable!("scoped read phases use the native collector above");
                    };
                    let allowance = remaining(policy, &used, mutation_effects, delivers);
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
                            continues,
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
                        // A held projected CREATE is admitted only by the
                        // independent profile, whose output cannot read the
                        // graph. Bindings already carry its checked fields.
                        let (stats, _, _) = insert::stage_proposal(
                            workspace.transaction(),
                            self,
                            query_cx,
                            query.insertion(),
                            creation,
                            &mut execution,
                            false,
                            frozen.as_mut(),
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
                        // Element-reader traits carry only a source error. Keep
                        // native cancellation separately, as the standalone
                        // mutation reader does, so a replacement-map read cannot
                        // turn an interruption into an ordinary source failure.
                        let element_failure: RefCell<Option<Box<asupersync::error::Error>>> =
                            RefCell::new(None);
                        let collected = query_cx.with_restriction(|| {
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
                            let elements = fgdb_gql::with_element_metadata(
                                |element, key| {
                                    if let Some(error) = element_failure.borrow().as_ref() {
                                        return Err(WriteTxnError::Interrupted(error.clone()));
                                    }
                                    // A named hidden field retains the chain's
                                    // explicit-refusal contract. Allowed keys still
                                    // admit arbitrary carried identities and both
                                    // edge endpoints before reading a property.
                                    if !scope.allows_property(key) {
                                        return Err(WriteTxnError::Authorization(
                                            Error::ScopeDenied,
                                        ));
                                    }
                                    match selection::mutation_element_property(
                                        transaction,
                                        &database.borrow(),
                                        query_cx,
                                        element,
                                        key,
                                        scope,
                                        &controls,
                                    ) {
                                        Err(WriteTxnError::Interrupted(error)) => {
                                            element_failure.borrow_mut().get_or_insert(error);
                                            Ok(None)
                                        }
                                        result => result,
                                    }
                                },
                                |element, kind, catalog, control| {
                                    use fgdb_gql::GraphElementMetadataError;
                                    if let Some(error) = element_failure.borrow().as_ref() {
                                        return Err(GraphElementMetadataError::Source(
                                            WriteTxnError::Interrupted(error.clone()),
                                        ));
                                    }
                                    let result = selection::returning_element_metadata(
                                        transaction,
                                        &database.borrow(),
                                        query_cx,
                                        element,
                                        kind,
                                        catalog,
                                        scope,
                                        &controls,
                                        control,
                                    );
                                    if let Err(GraphElementMetadataError::Source(
                                        WriteTxnError::Interrupted(error),
                                    )) = &result
                                    {
                                        element_failure
                                            .borrow_mut()
                                            .get_or_insert_with(|| error.clone());
                                    }
                                    result
                                },
                            );
                            let allocate = |request| {
                                database.borrow_mut().allocate_identity(query_cx, request)
                            };
                            let checkpoint = || {
                                if let Some(error) = element_failure.borrow_mut().take() {
                                    return Err(WriteTxnError::Interrupted(error));
                                }
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
                                        .execute_governed_over_reading_with_seeded_source_and_field_admission(
                                            rows.take(),
                                            policy,
                                            |pattern, budget| graph.pattern(pattern, budget),
                                            (scope.sees_all_incidence() && scope.sees_all_fields())
                                                .then_some(
                                                    |pattern: &PreparedGraphPattern<GraphValueRow>,
                                                     seeds: fgdb_gql::GraphSeeds<'_>,
                                                     budget| {
                                                        selection::select_seeded_overlay(
                                                            transaction,
                                                            &database.borrow(),
                                                            query_cx,
                                                            pattern,
                                                            seeds,
                                                            scope,
                                                            budget,
                                                            &controls,
                                                        )
                                                    },
                                                ),
                                            elements,
                                            |key| {
                                                mutation::admit_computed_property(
                                                    query_cx, key, &controls,
                                                )
                                            },
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
                                        .execute_governed_over_reading_with_seeded_source_and_field_admission(
                                            rows.take(),
                                            policy,
                                            |pattern, budget| graph.pattern(pattern, budget),
                                            (scope.sees_all_incidence() && scope.sees_all_fields())
                                                .then_some(
                                                    |pattern: &PreparedGraphPattern<GraphValueRow>,
                                                     seeds: fgdb_gql::GraphSeeds<'_>,
                                                     budget| {
                                                        selection::select_seeded_overlay(
                                                            transaction,
                                                            &database.borrow(),
                                                            query_cx,
                                                            pattern,
                                                            seeds,
                                                            scope,
                                                            budget,
                                                            &controls,
                                                        )
                                                    },
                                                ),
                                            elements,
                                            |key| {
                                                mutation::admit_computed_property(
                                                    query_cx, key, &controls,
                                                )
                                            },
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
                        });
                        let (proposal, mut returning) = match element_failure.into_inner() {
                            Some(error) => Err(GqlQueryError::Interrupted(
                                WriteTxnError::Interrupted(error),
                            )),
                            None => collected,
                        }?;
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
                    rows = Some(returning.value);
                    at += 1;
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
                let Some(value) = rows else {
                    return Err(source(WriteTxnError::NoPreparedWrite));
                };
                let mut result = GqlQueryExecution {
                    rows: GqlExecutionStats {
                        snapshot_records: used.selection.snapshot_records,
                        result_rows: u64::try_from(value.len())
                            .map_err(|_| source(WriteTxnError::Authorization(Error::TooLarge)))?,
                    },
                    evaluator: used.evaluator,
                    value,
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
                last_result = Some(result);
                }
                *active_record = None;
                let result = last_result.expect("a nonempty chain batch was admitted");
                let delivered = u64::try_from(result.value.len())
                    .map_err(|_| source(WriteTxnError::Authorization(Error::TooLarge)))?;
                execution
                    .permit
                    .charge_rows_at((execution.clock)(), delivered)
                    .map_err(|error| source(WriteTxnError::Authorization(error)))?;
                query_cx
                    .with_restriction(|| {
                        admit(
                            fgdb_gql::GraphWriteProgramStats {
                                completed_statements: chains.len(),
                                selection: used.selection,
                                evaluator: used.evaluator,
                                target_vertex_visits: 0,
                                target_edge_visits: 0,
                                mutation_effects,
                                created_vertices: used.created_vertices,
                                created_edges: used.created_edges,
                            },
                            &result.value,
                            &mut |work| {
                                query_cx.checkpoint().map_err(WriteTxnError::Interrupted)?;
                                execution.work(work)
                            },
                        )
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
