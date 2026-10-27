//! Query-selected updates and detach deletion over the shared masked source,
//! ordinary simultaneous-assignment collector, and authorized native staging.

use super::{Authority, CapabilityToken, Database, Error, Execution, Vfs};
use super::{Workspace, WriteBatch, WriteTxn, WriteTxnError, selection, stage};
use crate::write_txn::MutationProjectionTail;
use fgdb_delta_types::ElementId;
use fgdb_gql::{
    GqlQueryError, GraphMutationError, GraphMutationIntent, GraphMutationPolicy,
    GraphMutationQueryError, GraphMutationStats, PreparedGraphMutation, PreparedGraphMutationQuery,
};
use fgdb_types::{CommitCx, EId, EmbeddedTxnCompletion, QueryCx, TxnCx, VId};
use fgdb_warden::PlannerPredicates;
use std::cell::RefCell;
use std::collections::BTreeSet;

#[path = "frozen_updates.rs"]
mod frozen_updates;
pub(super) use frozen_updates::apply_frozen_program;
#[path = "mutation_program.rs"]
mod program;
pub use program::{AuthorizedBoundWriteBatch, AuthorizedPreparedWrite, AuthorizedWriteSession};

type Fault = GqlQueryError<GraphMutationError<WriteTxnError>, WriteTxnError>;
type QueryFault = GqlQueryError<GraphMutationQueryError<WriteTxnError>, WriteTxnError>;
type Receipt = (
    GraphMutationStats,
    Vec<VId>,
    Vec<EId>,
    EmbeddedTxnCompletion,
);

fn source(error: WriteTxnError) -> Fault {
    GqlQueryError::Source(GraphMutationError::Source(error))
}

impl<V: Vfs + Clone> Database<V> {
    /// Execute a mutation and its RETURN projection under one ReadWrite permit.
    ///
    /// The native collector reads the masked selection once and computes the
    /// post-statement values, DISTINCT, ordering and page before staging. Every
    /// original intent still passes before/after image authorization, including
    /// hidden-field writes and restricted DETACH DELETE. Hidden fields read as
    /// NULL; RETURN never rescans the graph or observes another generation.
    ///
    /// Only final projected rows consume the signed result allowance. LIMIT 0
    /// therefore suppresses rows without suppressing effects. Query, quota,
    /// authorization and cancellation failures discard the private workspace.
    /// Native completion alone publishes the effects and releases the rows;
    /// committed/unknown/recovery outcomes are never rewritten by a later check.
    #[allow(clippy::too_many_arguments)]
    pub async fn execute_graph_mutation_query_authorized(
        &mut self,
        txn_cx: &TxnCx,
        query_cx: &QueryCx,
        commit_cx: &CommitCx,
        authority: &Authority,
        token: &CapabilityToken,
        branch: &str,
        query: &PreparedGraphMutationQuery,
        policy: GraphMutationPolicy,
        clock: impl FnMut() -> u64,
    ) -> Result<
        (
            GraphMutationStats,
            fgdb_gql::GqlQueryExecution<fgdb_gql::algebra::GraphValueRow>,
            EmbeddedTxnCompletion,
        ),
        QueryFault,
    > {
        self.execute_graph_mutation_query_authorized_checked(
            txn_cx,
            query_cx,
            commit_cx,
            authority,
            token,
            branch,
            query,
            policy,
            clock,
            |_, _| Ok(()),
        )
        .await
    }

    /// Admit a private host representation of the final rows before committing.
    ///
    /// This is the same native mutation, not a second evaluator. The callback
    /// runs once (also for zero rows), after all original intents and result
    /// rows pass authorization. Refusal discards the workspace. The host must
    /// not publish tentative rows or perform external effects inside `admit`.
    /// It cannot replace the native rows or access the database/transaction.
    /// Use its meter before bounded traversal/encoding work: it polls QueryCx
    /// cancellation and charges the existing live signed write permit, including
    /// expiry and retirement. A no-op admission preserves ordinary API charges.
    /// No callback or fallible host admission occurs after native completion.
    #[allow(clippy::too_many_arguments)]
    pub async fn execute_graph_mutation_query_authorized_checked(
        &mut self,
        txn_cx: &TxnCx,
        query_cx: &QueryCx,
        commit_cx: &CommitCx,
        authority: &Authority,
        token: &CapabilityToken,
        branch: &str,
        query: &PreparedGraphMutationQuery,
        policy: GraphMutationPolicy,
        mut clock: impl FnMut() -> u64,
        admit: impl FnOnce(
            &[fgdb_gql::algebra::GraphValueRow],
            &mut dyn FnMut(u64) -> Result<(), WriteTxnError>,
        ) -> Result<(), WriteTxnError>,
    ) -> Result<
        (
            GraphMutationStats,
            fgdb_gql::GqlQueryExecution<fgdb_gql::algebra::GraphValueRow>,
            EmbeddedTxnCompletion,
        ),
        QueryFault,
    > {
        let query_source = |error| {
            GqlQueryError::Source(GraphMutationQueryError::Mutation(
                GraphMutationError::Source(error),
            ))
        };
        if authority.namespace() != self.keys.namespace {
            return Err(query_source(WriteTxnError::Authorization(
                Error::WrongAuthority,
            )));
        }
        let now = clock();
        let verified = authority
            .verify_at(token, branch, now)
            .map_err(|error| query_source(WriteTxnError::Authorization(error)))?;
        let permit = verified
            .begin_write_at(branch, now)
            .map_err(|error| query_source(WriteTxnError::Authorization(error)))?;
        if !verified.predicates().rights().can_read() {
            return Err(query_source(WriteTxnError::Authorization(
                Error::PermissionDenied,
            )));
        }
        commit_cx
            .with_restriction_async(async {
                let mut execution = Execution {
                    cx: commit_cx,
                    permit,
                    clock,
                };
                execution.checkpoint().map_err(query_source)?;
                let mut workspace = Workspace(Some(
                    self.begin(txn_cx)
                        .map_err(|error| query_source(WriteTxnError::Write(error)))?,
                ));
                let proposal = query_cx.with_restriction(|| {
                    let controls = RefCell::new(&mut execution);
                    let transaction: &WriteTxn = workspace.transaction();
                    let element_failure = RefCell::new(None);
                    let result = query.execute_governed_reading(
                        policy,
                        |pattern, allowance| {
                            selection::select_overlay(
                                transaction,
                                self,
                                query_cx,
                                pattern,
                                verified.predicates(),
                                allowance,
                                &controls,
                            )
                        },
                        scoped_element_reader(
                            transaction,
                            self,
                            query_cx,
                            verified.predicates(),
                            &element_failure,
                            &controls,
                        ),
                        || {
                            if let Some(error) = element_failure.borrow_mut().take() {
                                return Err(WriteTxnError::Interrupted(error));
                            }
                            query_cx.checkpoint().map_err(WriteTxnError::Interrupted)?;
                            controls.borrow_mut().checkpoint()
                        },
                    );
                    match element_failure.into_inner() {
                        Some(error) => Err(GqlQueryError::Interrupted(WriteTxnError::Interrupted(
                            error,
                        ))),
                        None => result,
                    }
                })?;
                let (mutation, mut result) = proposal.into_parts();
                let (staging_policy, tail) =
                    MutationProjectionTail::reserve(policy, mutation.stats(), &result).map_err(
                        |error| GqlQueryError::Source(GraphMutationQueryError::Mutation(error)),
                    )?;
                let rows = u64::try_from(result.value.len())
                    .map_err(|_| query_source(WriteTxnError::Authorization(Error::TooLarge)))?;
                execution
                    .permit
                    .charge_rows_at((execution.clock)(), rows)
                    .map_err(|error| query_source(WriteTxnError::Authorization(error)))?;
                let (stats, _, _) = stage_proposal(
                    workspace.transaction(),
                    self,
                    query_cx,
                    query.mutation(),
                    mutation,
                    staging_policy,
                    verified.predicates(),
                    &mut execution,
                    false,
                )
                .map_err(|error| error.map_source(GraphMutationQueryError::Mutation))?;
                tail.complete(stats, &mut result).map_err(|error| {
                    GqlQueryError::Source(GraphMutationQueryError::Mutation(error))
                })?;
                query_cx
                    .with_restriction(|| {
                        admit(&result.value, &mut |work| {
                            query_cx.checkpoint().map_err(WriteTxnError::Interrupted)?;
                            execution.work(work)
                        })
                    })
                    .map_err(query_source)?;
                execution.bind_decision(workspace.transaction());
                let completion = workspace
                    .transaction()
                    .complete_controlled(self, commit_cx, None, false, || {
                        query_cx.checkpoint().map_err(WriteTxnError::Interrupted)?;
                        execution.checkpoint()
                    })
                    .await
                    .map_err(query_source)?;
                Ok((stats, result, completion))
            })
            .await
    }

    /// Execute MATCH-selected SET, REMOVE, or DETACH DELETE under one capability.
    ///
    /// ReadWrite rights are required before database access, including empty
    /// selections. Matching and every RHS expression see the same masked graph
    /// as authorized reads. The ordinary mutation collector freezes selection,
    /// evaluates simultaneous assignments, ignores null OPTIONAL targets, and
    /// rejects conflicting duplicates before any native intent is staged.
    ///
    /// Every surviving element/field intent then passes original before/after
    /// image authorization, even when SET or REMOVE would normalize to a no-op.
    /// Hidden fields are preserved, never replaced with their masked read values.
    /// Edge updates authorize the actual relation and both original endpoints,
    /// not just the statement's default relation. A scope escape or refused tail
    /// discards the whole private workspace. DETACH DELETE retains the native
    /// rule requiring a capability that hides no labels, properties or relations;
    /// a restricted holder cannot probe hidden incidence through delete success.
    ///
    /// Selection, collection, staging and final validation share ONE signed
    /// allowance. The native policy separately bounds query work and effects.
    /// Stats are proposal/control counts, not graph result rows, so max_rows=0
    /// is supported. An empty proposal closes read-only without a new commit.
    /// Only native completion acknowledges publication; no fallible permission
    /// check follows commit admission. Unknown/recovery outcomes are unchanged.
    ///
    /// The trusted host owns issuer selection, branch routing and a monotone
    /// issuer-epoch clock. This is not a mandatory facade over privileged APIs,
    /// durable revocation/audit ordering, timing isolation or full SSI.
    #[allow(clippy::too_many_arguments)]
    pub async fn execute_graph_mutation_authorized(
        &mut self,
        txn_cx: &TxnCx,
        query_cx: &QueryCx,
        commit_cx: &CommitCx,
        authority: &Authority,
        token: &CapabilityToken,
        branch: &str,
        mutation: &PreparedGraphMutation,
        policy: GraphMutationPolicy,
        clock: impl FnMut() -> u64,
    ) -> Result<(GraphMutationStats, EmbeddedTxnCompletion), Fault> {
        self.mutation_authorized_inner(
            txn_cx, query_cx, commit_cx, authority, token, branch, mutation, policy, clock, false,
        )
        .await
        .map(|(stats, _, _, completion)| (stats, completion))
    }

    /// Return distinct proposed target identities only after native completion.
    /// Vertex and edge IDs are independently sorted, just as on the ordinary
    /// returning mutation surface. Several fields or duplicate MATCH rows cost
    /// one result row per distinct target, not one per assignment. Admission
    /// precedes receipt allocation and publication: insufficient max_rows cannot
    /// commit successfully and then refuse the response. Equal-to-current SET
    /// targets are included; this is not a count of physically changed records.
    #[allow(clippy::too_many_arguments)]
    pub async fn execute_graph_mutation_returning_authorized(
        &mut self,
        txn_cx: &TxnCx,
        query_cx: &QueryCx,
        commit_cx: &CommitCx,
        authority: &Authority,
        token: &CapabilityToken,
        branch: &str,
        mutation: &PreparedGraphMutation,
        policy: GraphMutationPolicy,
        clock: impl FnMut() -> u64,
    ) -> Result<Receipt, Fault> {
        self.mutation_authorized_inner(
            txn_cx, query_cx, commit_cx, authority, token, branch, mutation, policy, clock, true,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn mutation_authorized_inner(
        &mut self,
        txn_cx: &TxnCx,
        query_cx: &QueryCx,
        commit_cx: &CommitCx,
        authority: &Authority,
        token: &CapabilityToken,
        branch: &str,
        mutation: &PreparedGraphMutation,
        policy: GraphMutationPolicy,
        mut clock: impl FnMut() -> u64,
        returning: bool,
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
        if !verified.predicates().rights().can_read() {
            return Err(source(WriteTxnError::Authorization(
                Error::PermissionDenied,
            )));
        }
        commit_cx
            .with_restriction_async(async {
                let mut execution = Execution {
                    cx: commit_cx,
                    permit,
                    clock,
                };
                execution.checkpoint().map_err(source)?;
                let mut workspace = Workspace(Some(
                    self.begin(txn_cx)
                        .map_err(|error| source(WriteTxnError::Write(error)))?,
                ));
                let (stats, vertices, edges) = apply(
                    workspace.transaction(),
                    self,
                    query_cx,
                    mutation,
                    policy,
                    verified.predicates(),
                    &mut execution,
                    returning,
                )?;
                execution.bind_decision(workspace.transaction());
                let completion = workspace
                    .transaction()
                    .complete_controlled(self, commit_cx, None, false, || {
                        query_cx.checkpoint().map_err(WriteTxnError::Interrupted)?;
                        execution.checkpoint()
                    })
                    .await
                    .map_err(source)?;
                Ok((stats, vertices, edges, completion))
            })
            .await
    }
}

// One statement executor serves autocommit and ordered programs. The caller
// owns the private workspace and is solely responsible for native completion.
#[allow(clippy::too_many_arguments)]
pub(super) fn apply<V: Vfs + Clone, Clock: FnMut() -> u64>(
    transaction: &mut WriteTxn,
    database: &mut Database<V>,
    cx: &QueryCx,
    mutation: &PreparedGraphMutation,
    policy: GraphMutationPolicy,
    scope: &PlannerPredicates,
    execution: &mut Execution<'_, '_, Clock>,
    returning: bool,
) -> Result<(GraphMutationStats, Vec<VId>, Vec<EId>), Fault> {
    let proposal = select_proposal(
        transaction,
        database,
        cx,
        mutation,
        policy,
        scope,
        execution,
    )?;
    stage_proposal(
        transaction,
        database,
        cx,
        mutation,
        proposal,
        policy,
        scope,
        execution,
        returning,
    )
}

// Frozen UNWIND input uses the very same masked source and native collector,
// retaining its proposals until every row has observed the entry overlay.
#[allow(clippy::too_many_arguments)]
fn select_proposal<V: Vfs + Clone, Clock: FnMut() -> u64>(
    transaction: &mut WriteTxn,
    database: &mut Database<V>,
    cx: &QueryCx,
    mutation: &PreparedGraphMutation,
    policy: GraphMutationPolicy,
    scope: &PlannerPredicates,
    execution: &mut Execution<'_, '_, Clock>,
) -> Result<fgdb_gql::GraphMutationBatch, Fault> {
    cx.with_restriction(|| {
        // These sequential callback borrows end before staging and
        // before any await. No RefCell or query result escapes.
        let execution = RefCell::new(&mut *execution);
        let element_failure = RefCell::new(None);
        let result = mutation.execute_governed_reading(
            policy,
            |pattern, allowance| {
                selection::select_overlay(
                    transaction,
                    database,
                    cx,
                    pattern,
                    scope,
                    allowance,
                    &execution,
                )
            },
            scoped_element_reader(
                transaction,
                database,
                cx,
                scope,
                &element_failure,
                &execution,
            ),
            || {
                if let Some(error) = element_failure.borrow_mut().take() {
                    return Err(WriteTxnError::Interrupted(error));
                }
                cx.checkpoint().map_err(WriteTxnError::Interrupted)?;
                execution.borrow_mut().checkpoint()
            },
        );
        match element_failure.into_inner() {
            Some(error) => Err(GqlQueryError::Interrupted(WriteTxnError::Interrupted(
                error,
            ))),
            None => result,
        }
    })
}

/// Admit arbitrary computed identities through the same signed scope as the
/// graph source, preserving the query's typed interruption channel. Parameter
/// lists and chain rows carry no authority of their own (FG-INV-20).
#[allow(clippy::too_many_arguments)]
fn scoped_element_property<V: Vfs + Clone, Clock: FnMut() -> u64>(
    transaction: &WriteTxn,
    database: &Database<V>,
    cx: &QueryCx,
    scope: &PlannerPredicates,
    failure: &RefCell<Option<Box<asupersync::error::Error>>>,
    element: ElementId,
    key: fgdb_delta_types::PropertyKeyId,
    execution: &RefCell<&mut Execution<'_, '_, Clock>>,
) -> Result<Option<fgdb_types::CanonicalPropertyValue>, WriteTxnError> {
    match selection::mutation_element_property(
        transaction,
        database,
        cx,
        element,
        key,
        scope,
        execution,
    ) {
        Err(WriteTxnError::Interrupted(error)) => {
            *failure.borrow_mut() = Some(error);
            Ok(None)
        }
        result => result,
    }
}

fn scoped_element_reader<'a, V: Vfs + Clone, Clock: FnMut() -> u64>(
    transaction: &'a WriteTxn,
    database: &'a Database<V>,
    cx: &'a QueryCx,
    scope: &'a PlannerPredicates,
    failure: &'a RefCell<Option<Box<asupersync::error::Error>>>,
    execution: &'a RefCell<&mut Execution<'_, '_, Clock>>,
) -> impl fgdb_gql::GraphElementReader<WriteTxnError> + 'a {
    fgdb_gql::with_element_metadata(
        move |element, key| {
            scoped_element_property(
                transaction,
                database,
                cx,
                scope,
                failure,
                element,
                key,
                execution,
            )
        },
        move |element, kind, catalog, control| {
            use fgdb_gql::GraphElementMetadataError;
            if let Some(error) = failure.borrow().as_ref() {
                return Err(GraphElementMetadataError::Source(
                    WriteTxnError::Interrupted(error.clone()),
                ));
            }
            let result = selection::returning_element_metadata(
                transaction,
                database,
                cx,
                element,
                kind,
                catalog,
                scope,
                execution,
                control,
            );
            if let Err(GraphElementMetadataError::Source(WriteTxnError::Interrupted(error))) =
                &result
            {
                failure.borrow_mut().get_or_insert_with(|| error.clone());
            }
            result
        },
    )
}

// Both mutation surfaces authorize the same original intents. The projected
// RETURN path charges its final rows instead of internal target identities.
#[allow(clippy::too_many_arguments)]
pub(super) fn stage_proposal<V: Vfs + Clone, Clock: FnMut() -> u64>(
    transaction: &mut WriteTxn,
    database: &mut Database<V>,
    query_cx: &QueryCx,
    mutation: &PreparedGraphMutation,
    proposal: fgdb_gql::GraphMutationBatch,
    policy: GraphMutationPolicy,
    scope: &PlannerPredicates,
    execution: &mut Execution<'_, '_, Clock>,
    returning: bool,
) -> Result<(GraphMutationStats, Vec<VId>, Vec<EId>), Fault> {
    let mut stats = proposal.stats();
    if proposal.intents().iter().any(|intent| {
        matches!(
            intent,
            GraphMutationIntent::Delete { .. } | GraphMutationIntent::DeleteEdge { .. }
        )
    }) {
        // A plain DELETE statement has no other action: the same scope
        // rules, incidence proof and staging order as authorized DELETE.
        let mut vertices = Vec::new();
        let mut edges = Vec::new();
        for intent in proposal.into_intents() {
            match intent {
                GraphMutationIntent::Delete { vertex } => vertices.push(vertex),
                GraphMutationIntent::DeleteEdge { edge } => edges.push(edge),
                _ => {
                    return Err(GqlQueryError::Source(
                        GraphMutationError::InvalidSourceStatistics,
                    ));
                }
            }
        }
        vertices.sort_unstable();
        edges.sort_unstable();
        let deleted = fgdb_gql::GraphDeleteStats {
            selection: stats.selection,
            evaluator: stats.evaluator,
            target_vertices: stats.target_vertices,
            target_edges: stats.target_edges,
        };
        let (deleted, vertices, edges) = super::deletion::complete(
            transaction,
            database,
            query_cx,
            mutation.relation(),
            deleted,
            (vertices, edges),
            fgdb_gql::GraphDeletePolicy::new(policy.query, policy.max_effects),
            scope,
            execution,
            returning,
        )
        .map_err(|error| {
            error.map_source(|error| match error {
                fgdb_gql::GraphDeleteError::Source(error) => GraphMutationError::Source(error),
                fgdb_gql::GraphDeleteError::IncidentRelationships => {
                    GraphMutationError::IncidentRelationships
                }
                // The completion only counts; it never re-selects.
                fgdb_gql::GraphDeleteError::InvalidSourceStatistics
                | fgdb_gql::GraphDeleteError::InputSchema { .. }
                | fgdb_gql::GraphDeleteError::TargetLimit { .. } => {
                    GraphMutationError::InvalidSourceStatistics
                }
            })
        })?;
        stats.selection = deleted.selection;
        stats.evaluator = deleted.evaluator;
        return Ok((stats, vertices, edges));
    }
    let mut targets = BTreeSet::new();
    for intent in proposal.into_intents() {
        query_cx
            .checkpoint()
            .map_err(WriteTxnError::Interrupted)
            .map_err(source)?;
        execution.checkpoint().map_err(source)?;
        let mut batch = WriteBatch::new(mutation.relation());
        let target = match intent {
            GraphMutationIntent::Property { vertex, key, value } => {
                batch.set_vertex_property_value(vertex, key, value);
                ElementId::Vertex(vertex)
            }
            GraphMutationIntent::EdgeProperty { edge, key, value } => {
                batch.set_edge_property_value(edge, key, value);
                ElementId::Edge(edge)
            }
            GraphMutationIntent::StoredProperty { target, key, value } => {
                match target {
                    ElementId::Vertex(vertex) => {
                        batch.set_vertex_property_value(vertex, key, Some(value));
                    }
                    ElementId::Edge(edge) => {
                        batch.set_edge_property_value(edge, key, Some(value));
                    }
                }
                target
            }
            GraphMutationIntent::Label {
                vertex,
                label,
                present,
            } => {
                batch.set_vertex_label(vertex, label, present);
                ElementId::Vertex(vertex)
            }
            GraphMutationIntent::DetachDelete { vertex } => {
                batch.delete_vertex(vertex);
                ElementId::Vertex(vertex)
            }
            GraphMutationIntent::Delete { .. } | GraphMutationIntent::DeleteEdge { .. } => {
                unreachable!("plain DELETE statements complete above")
            }
        };
        if returning {
            // Price fixed-width tree lookup before touching it;
            // its size is bounded by the admitted effect count.
            let work = (usize::BITS - targets.len().leading_zeros()).max(1);
            execution.work(u64::from(work)).map_err(source)?;
            if !targets.contains(&target) {
                execution
                    .permit
                    .charge_rows_at((execution.clock)(), 1)
                    .map_err(|error| source(WriteTxnError::Authorization(error)))?;
                targets.insert(target);
            }
        }
        for row in batch.rows {
            stage(transaction, database, batch.relation, row, execution).map_err(source)?;
        }
    }
    // Build the complete private receipt before commit admission.
    // The stats-only path allocated no target entries or vectors.
    let mut vertices = Vec::new();
    let mut edges = Vec::new();
    for target in targets {
        query_cx
            .checkpoint()
            .map_err(WriteTxnError::Interrupted)
            .map_err(source)?;
        execution.checkpoint().map_err(source)?;
        match target {
            ElementId::Vertex(vertex) => vertices.push(vertex),
            ElementId::Edge(edge) => edges.push(edge),
        }
    }
    Ok((stats, vertices, edges))
}

#[cfg(test)]
#[path = "mutation_tests.rs"]
mod tests;
