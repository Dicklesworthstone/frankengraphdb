//! Write chains compose the native collectors and authorized staging owners.
//! Private WITH rows never mint authority, a new allowance or a commit.

use super::{
    Authority, CapabilityToken, Database, Error, Execution, Vfs, Workspace, WriteTxn,
    WriteTxnError, insert, mutation, selection,
};
use fgdb_gql::algebra::{GraphValueRow, PreparedGraphPattern, ValueProjection};
use fgdb_gql::insertion::{
    GraphInsertBatch, GraphInsertError, GraphInsertPolicy, GraphInsertStats, PreparedGraphInsert,
};
use fgdb_gql::{
    GlaExecutionStats, GqlExecutionStats, GqlQueryError, GqlQueryExecution, GqlQueryPolicy,
    GraphInsertBinding, GraphInsertQueryError, GraphMutationBatch, GraphMutationError,
    GraphMutationPolicy, GraphMutationQueryError, GraphMutationStats, GraphPatternFactorExecution,
    GraphSetSource, GraphWriteChainError, GraphWriteChainStep, PreparedGraphMutation,
    PreparedGraphPatternFactors, PreparedGraphWriteChain,
};
use fgdb_types::{CommitCx, EmbeddedTxnCompletion, QueryCx, TxnCx};
use fgdb_warden::PlannerPredicates;
use std::cell::RefCell;

#[path = "chain_merges.rs"]
mod merges;

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

fn invalid_mutation_statistics() -> Fault {
    GqlQueryError::Source(mutation_error(GraphMutationError::InvalidSourceStatistics))
}

struct MutationTail {
    snapshot_records: u64,
    work_units: u64,
    scratch_entries: u64,
}

// The query result already includes its mutation prefix. Plain DELETE's
// authorization owner adds an incidence proof after collection. Reserve the
// projection's extra usage before that owner checks or allocates anything.
fn mutation_staging_policy(
    policy: GraphMutationPolicy,
    prefix: GraphMutationStats,
    result: &GqlQueryExecution<GraphValueRow>,
) -> Result<(GraphMutationPolicy, MutationTail), Fault> {
    let tail = MutationTail {
        snapshot_records: result
            .rows
            .snapshot_records
            .checked_sub(prefix.selection.snapshot_records)
            .ok_or_else(invalid_mutation_statistics)?,
        work_units: result
            .evaluator
            .work_units
            .checked_sub(prefix.evaluator.work_units)
            .ok_or_else(invalid_mutation_statistics)?,
        scratch_entries: result
            .evaluator
            .scratch_entries
            .checked_sub(prefix.evaluator.scratch_entries)
            .ok_or_else(invalid_mutation_statistics)?,
    };
    let reserved = GqlQueryPolicy::new(
        policy
            .query
            .rows
            .max_snapshot_records()
            .unwrap_or(u64::MAX)
            .checked_sub(tail.snapshot_records)
            .ok_or_else(invalid_mutation_statistics)?,
        policy.query.rows.max_result_rows().unwrap_or(u64::MAX),
        policy
            .query
            .evaluator
            .max_work_units
            .checked_sub(tail.work_units)
            .ok_or_else(invalid_mutation_statistics)?,
        policy
            .query
            .evaluator
            .max_scratch_entries
            .checked_sub(tail.scratch_entries)
            .ok_or_else(invalid_mutation_statistics)?,
    );
    Ok((GraphMutationPolicy::new(reserved, policy.max_effects), tail))
}

fn finish_mutation_statistics(
    result: &mut GqlQueryExecution<GraphValueRow>,
    staged: GraphMutationStats,
    tail: MutationTail,
) -> Result<(), Fault> {
    result.rows.snapshot_records = staged
        .selection
        .snapshot_records
        .checked_add(tail.snapshot_records)
        .ok_or_else(invalid_mutation_statistics)?;
    result.evaluator.work_units = staged
        .evaluator
        .work_units
        .checked_add(tail.work_units)
        .ok_or_else(invalid_mutation_statistics)?;
    result.evaluator.scratch_entries = staged
        .evaluator
        .scratch_entries
        .checked_add(tail.scratch_entries)
        .ok_or_else(invalid_mutation_statistics)?;
    Ok(())
}

impl<V: Vfs + Clone> Database<V> {
    /// Execute creation, mutation and relationship MERGE chains under one capability.
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
    /// parallel matches and private continuation rows. Vertex MERGE still refuses
    /// during preflight, before opening a transaction or allocating an identity.
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
        // Whole-program admission precedes even the first engine ID. The
        // chain is compiler-owned, but its unsupported family is not authority.
        let mut reads = chain.returns_rows();
        for (step, statement) in chain.steps().iter().enumerate() {
            execution.checkpoint().map_err(source)?;
            if merges::preflight(statement, scope)? {
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
                let mut rows = None;
                let mut result = None;
                let mut mutation_effects = 0;
                let last = chain.steps().len().saturating_sub(1);
                for (at, statement) in chain.steps().iter().enumerate() {
                    let allowance = remaining(
                        policy,
                        &used,
                        mutation_effects,
                        at == last && chain.returns_rows(),
                    );
                    let (stats, returning, effects) = if matches!(
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
                        )?;
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
                            let allocate = |request| {
                                database.borrow_mut().allocate_identity(query_cx, request)
                            };
                            let checkpoint = || {
                                query_cx.checkpoint().map_err(WriteTxnError::Interrupted)?;
                                controls.borrow_mut().checkpoint()
                            };
                            match statement {
                                GraphWriteChainStep::Query(query) => {
                                    let proposal = query
                                        .execute_governed_over(
                                            rows.take(),
                                            allowance,
                                            graph,
                                            allocate,
                                            checkpoint,
                                        )
                                        .map_err(|error| {
                                            error.map_source(GraphWriteChainError::Insert)
                                        })?;
                                    let (creation, returning) = proposal.into_parts();
                                    Ok((Proposal::Insert(query.insertion(), creation), returning))
                                }
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
                                        .execute_governed_over(
                                            rows.take(),
                                            policy,
                                            |pattern, budget| graph.pattern(pattern, budget),
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
                                        .execute_governed_over(
                                            rows.take(),
                                            policy,
                                            |pattern, budget| graph.pattern(pattern, budget),
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
                                let (staging_policy, tail) =
                                    mutation_staging_policy(policy, proposal.stats(), &returning)?;
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
                                finish_mutation_statistics(&mut returning, staged, tail)?;
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
                let Some(mut result) = result else {
                    return Err(source(WriteTxnError::NoPreparedWrite));
                };
                result.rows.snapshot_records = used.selection.snapshot_records;
                result.evaluator = used.evaluator;
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
