//! An explicit read owner with one pin and one cumulative signed allowance.
//! The ordinary session deliberately has per-execution allowances; this owner
//! instead moves its permit into the shared native executor for each request.
//! Both owners share the checked graph-output boundary below, so metadata
//! hydration never starts a second execution or resets a signed allowance.

use super::*;
use fgdb_delta_types::ElementId;
use fgdb_gql::{
    GlaExecutionEvent, GlaLimitDimension, GlaLimitExceeded, GraphElementMetadata,
    GraphElementMetadataError, ReverseSymbolCatalog,
};
use std::cell::Cell;

/// Capability-scoped graph metadata during a checked read-output callback.
///
/// An identity is re-admitted at the owner's exact snapshot, including both
/// endpoints of a relationship. Hidden identities return None; fields are
/// masked before catalog access or copying. The trusted reverse catalog names
/// admitted fields and does not grant authority. Calls spend the same live
/// signed node/work allowance as the statement, never a new read session.
/// Returned values stay private until the enclosing query succeeds.
pub trait AuthorizedReadOutput {
    fn metadata(
        &mut self,
        element: ElementId,
        kind: GraphElementMetadata,
        catalog: &ReverseSymbolCatalog,
    ) -> Result<Option<fgdb_gql::algebra::GraphValue>, QueryError>;
}

struct MetadataReader<F>(F);

impl<F> AuthorizedReadOutput for MetadataReader<F>
where
    F: FnMut(
        ElementId,
        GraphElementMetadata,
        &ReverseSymbolCatalog,
    ) -> Result<Option<fgdb_gql::algebra::GraphValue>, QueryError>,
{
    fn metadata(
        &mut self,
        element: ElementId,
        kind: GraphElementMetadata,
        catalog: &ReverseSymbolCatalog,
    ) -> Result<Option<fgdb_gql::algebra::GraphValue>, QueryError> {
        (self.0)(element, kind, catalog)
    }
}

#[derive(Default)]
struct MetadataBudget {
    work: u64,
    scratch: u64,
}

impl MetadataBudget {
    fn charge(
        &mut self,
        event: GlaExecutionEvent,
        policy: GqlQueryPolicy,
    ) -> Result<(), QueryError> {
        let work = u128::from(self.work) + 1;
        let scratch =
            u128::from(self.scratch) + u128::from(matches!(event, GlaExecutionEvent::ScratchEntry));
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
                return Err(QueryError::Pattern(fgdb_gql::GqlQueryError::Evaluator(
                    GlaLimitExceeded {
                        dimension,
                        limit,
                        observed,
                    },
                )));
            }
        }
        self.work = work as u64;
        self.scratch = scratch as u64;
        Ok(())
    }
}

/// Admit one result representation against its actual selected cut. This is
/// shared by ordinary sessions (whose cut may be historical) and explicit
/// transactions (whose cut is their fixed BEGIN basis). The caller owns final
/// delivery; all result rows have already been reserved when this returns.
#[allow(clippy::too_many_arguments)]
pub(super) fn admit_graph_output<T, E>(
    view: &EmbeddedReadView,
    at: CommitSeq,
    scope: &PlannerPredicates,
    policy: GqlQueryPolicy,
    execution: &Live<'_, '_, '_>,
    result: QueryResult,
    rows: usize,
    output: impl FnOnce(
        QueryResult,
        &mut dyn FnMut(u64) -> Result<(), QueryError>,
        &mut dyn AuthorizedReadOutput,
    ) -> Result<T, E>,
    map_error: impl Fn(QueryError) -> E,
) -> Result<T, E> {
    // Reserve before a second representation is allocated. Final delivery
    // validates callback work without charging these same user rows twice.
    execution
        .borrow_mut()
        .admit_rows(rows)
        .map_err(&map_error)?;
    let metadata_failed = Cell::new(false);
    let metadata_failure = RefCell::new(None);
    let mut metadata_budget = MetadataBudget::default();
    let mut metadata = MetadataReader(|element, kind, catalog: &ReverseSymbolCatalog| {
        if metadata_failed.get() {
            return Err(stopped());
        }
        let result = (|| {
            execution.borrow_mut().checkpoint()?;
            let value = scoped_element_metadata(
                &view.snapshot,
                at,
                scope,
                execution,
                &metadata_failure,
                element,
                kind,
                catalog,
                &mut |event| {
                    let charged = (|| {
                        execution.borrow_mut().checkpoint()?;
                        metadata_budget.charge(event, policy)
                    })();
                    charged.map_err(|error| {
                        metadata_failure.borrow_mut().get_or_insert(error);
                    })
                },
            );
            if let Some(error) = metadata_failure.borrow_mut().take() {
                return Err(error);
            }
            let value = value.map_err(|error| match error {
                GraphElementMetadataError::Source(error) => {
                    QueryError::Pattern(fgdb_gql::GqlQueryError::Source(error))
                }
                GraphElementMetadataError::Interrupted => stopped(),
            })?;
            execution.borrow_mut().checkpoint()?;
            Ok(value)
        })();
        if result.is_err() {
            metadata_failed.set(true);
        }
        result
    });
    let admitted = output(
        result,
        &mut |units| execution.borrow_mut().work_units(units),
        &mut metadata,
    );
    execution.borrow_mut().checkpoint().map_err(&map_error)?;
    let admitted = admitted?;
    if metadata_failed.get() {
        return Err(map_error(stopped()));
    }
    Ok(admitted)
}

struct State<'issuer, Resolver, Clock> {
    view: EmbeddedReadView,
    permit: ExecutionPermit<'issuer, ReadAccess>,
    scope: PlannerPredicates,
    branch: String,
    resolver: Resolver,
    policy: GqlQueryPolicy,
    clock: Clock,
}

/// Sequential reads of one authorized immutable snapshot. All input, native
/// execution, result rows and checked output spend the SAME signed allowance.
/// The trusted host fixes its resolver, branch, clock and per-statement native
/// policy at admission. No database or request-context borrow is retained.
///
/// Every query error, output refusal, cancellation or unwind terminally closes
/// the owner. Dropping or rolling it back releases the pin without publishing
/// anything. This is an in-process snapshot owner, not a durable transaction
/// lease or a claim of write-conflict validation.
#[must_use = "dropping an authorized read transaction releases its snapshot pin"]
pub struct AuthorizedReadTransaction<'issuer, Resolver, Clock> {
    state: Option<State<'issuer, Resolver, Clock>>,
    basis: CommitSeq,
    completed_statements: u64,
}

impl<R, C> core::fmt::Debug for AuthorizedReadTransaction<'_, R, C> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AuthorizedReadTransaction")
            .field("active", &self.is_active())
            .field("basis", &self.basis)
            .field("completed_statements", &self.completed_statements)
            .finish_non_exhaustive()
    }
}

impl<R, C> AuthorizedReadTransaction<'_, R, C> {
    #[must_use]
    pub const fn basis(&self) -> CommitSeq {
        self.basis
    }

    #[must_use]
    pub const fn completed_statements(&self) -> u64 {
        self.completed_statements
    }

    #[must_use]
    pub fn is_active(&self) -> bool {
        self.state.is_some()
    }

    /// Discard the read owner. There are no staged mutations to publish.
    pub fn rollback(mut self) {
        drop(self.state.take());
    }
}

impl<R: GraphSymbolResolver, C: FnMut() -> u64> AuthorizedReadTransaction<'_, R, C> {
    /// Execute through the ordinary native selector, compiler and scoped
    /// source. A bound historical cut must equal this transaction's basis.
    pub fn query(
        &mut self,
        cx: &QueryCx,
        text: &str,
        parameters: &GqlParameters,
    ) -> Result<QueryResult, QueryError> {
        self.query_with_output(cx, text, parameters, |result, _| Ok(result), |error| error)
    }

    /// Admit a host-owned result representation before it leaves the native
    /// owner. `output` is trusted host code: it must charge through `work`
    /// before copying data and must not send or publish the value itself.
    /// The returned value is released only after live authority, cancellation,
    /// deferred work and result-row checks succeed. A swallowed meter failure
    /// still terminally refuses the transaction at that final boundary.
    pub fn query_with_output<T, E>(
        &mut self,
        cx: &QueryCx,
        text: &str,
        parameters: &GqlParameters,
        output: impl FnOnce(QueryResult, &mut dyn FnMut(u64) -> Result<(), QueryError>) -> Result<T, E>,
        map_error: impl Fn(QueryError) -> E,
    ) -> Result<T, E> {
        self.query_with_graph_output(
            cx,
            text,
            parameters,
            |result, work, _| output(result, work),
            map_error,
        )
    }

    /// Admit a host representation with graph metadata from the original pin
    /// and signed permit. There is no database handle or raw snapshot in this
    /// callback. The host must meter its own conversion through `work` and
    /// publish only after this method returns successfully.
    ///
    /// All metadata calls within this output callback share one additional
    /// work/scratch ceiling from the fixed query policy. That ceiling bounds
    /// the metadata phase separately from query evaluation; signed node/work
    /// allowances remain cumulative across both phases and all statements.
    /// Metadata failures, even swallowed by the host, terminally close the
    /// owner and prevent output or pending cache publication.
    pub fn query_with_graph_output<T, E>(
        &mut self,
        cx: &QueryCx,
        text: &str,
        parameters: &GqlParameters,
        output: impl FnOnce(
            QueryResult,
            &mut dyn FnMut(u64) -> Result<(), QueryError>,
            &mut dyn AuthorizedReadOutput,
        ) -> Result<T, E>,
        map_error: impl Fn(QueryError) -> E,
    ) -> Result<T, E> {
        // Take before all fallible work. An error or panic can never restore
        // an owner whose original allowance or last statement was discarded.
        let State {
            view,
            permit,
            scope,
            branch,
            mut resolver,
            policy,
            mut clock,
        } = self.state.take().ok_or_else(|| map_error(stopped()))?;
        let next = self
            .completed_statements
            .checked_add(1)
            .ok_or_else(|| map_error(QueryError::Authorization(AuthorizationError::TooLarge)))?;
        let mut execution = Execution::new(cx, permit, &mut clock as &mut dyn FnMut() -> u64);
        execution.beacon_indexes = Some(view.beacon_indexes.clone());
        execution.required_cut = Some(self.basis);
        let execution = RefCell::new(execution);
        let value = cx.with_restriction(|| -> Result<T, E> {
            execution.borrow_mut().checkpoint().map_err(&map_error)?;
            let result = text_at(
                &view,
                &branch,
                &scope,
                &mut resolver,
                policy,
                &execution,
                text,
                parameters,
            )
            .map_err(&map_error);
            // Resolver and binder errors cannot hide a concurrent retirement
            // or expiry. This also preserves a latched native meter refusal.
            execution.borrow_mut().checkpoint().map_err(&map_error)?;
            let ((result, at), rows) = result?;
            let admitted = admit_graph_output(
                &view, at, &scope, policy, &execution, result, rows, output, &map_error,
            )?;
            execution.borrow_mut().deliver(0).map_err(&map_error)?;
            Ok(admitted)
        })?;
        // deliver settled every deferred charge. Move exactly this permit back
        // into idle storage, without retaining the just-finished request Cx.
        let permit = execution.into_inner().permit;
        self.state = Some(State {
            view,
            permit,
            scope,
            branch,
            resolver,
            policy,
            clock,
        });
        self.completed_statements = next;
        Ok(value)
    }

    /// Close successfully at the original snapshot after final live checks.
    /// No commit sequence is allocated and no write outcome is manufactured.
    pub fn close(mut self, cx: &QueryCx) -> Result<CommitSeq, QueryError> {
        let State {
            view,
            permit,
            mut clock,
            ..
        } = self.state.take().ok_or_else(stopped)?;
        cx.with_restriction(|| {
            let mut execution = Execution::new(cx, permit, &mut clock);
            execution.deliver(0)?;
            drop(view);
            Ok(self.basis)
        })
    }
}

fn stopped() -> QueryError {
    QueryError::Authorization(AuthorizationError::ExecutionStopped)
}

impl<V: Vfs + Clone> Database<V> {
    /// Begin a read-only transaction using one pin and one cumulative signed
    /// permit. Signature, namespace and read rights precede pin acquisition.
    /// The issuer stays borrowed so retirement also invalidates idle owners.
    #[allow(clippy::too_many_arguments)]
    pub fn begin_authorized_read<'issuer, R: GraphSymbolResolver, C: FnMut() -> u64>(
        &self,
        cx: &QueryCx,
        authority: &'issuer Authority,
        token: &CapabilityToken,
        branch: &str,
        resolver: R,
        policy: GqlQueryPolicy,
        clock: C,
    ) -> Result<AuthorizedReadTransaction<'issuer, R, C>, QueryError> {
        admit(
            self.keys.namespace,
            cx,
            authority,
            token,
            branch,
            resolver,
            policy,
            clock,
            || self.read_session(),
        )
    }
}

impl EmbeddedReadView {
    /// Narrow this exact generation into a read transaction. The host remains
    /// responsible for its recovery fence, as with an authorized read session.
    #[allow(clippy::too_many_arguments)]
    pub fn begin_authorized_read<'issuer, R: GraphSymbolResolver, C: FnMut() -> u64>(
        &self,
        cx: &QueryCx,
        authority: &'issuer Authority,
        token: &CapabilityToken,
        branch: &str,
        resolver: R,
        policy: GqlQueryPolicy,
        clock: C,
    ) -> Result<AuthorizedReadTransaction<'issuer, R, C>, QueryError> {
        admit(
            self.namespace,
            cx,
            authority,
            token,
            branch,
            resolver,
            policy,
            clock,
            || Ok(self.clone()),
        )
    }
}

#[allow(clippy::too_many_arguments)]
fn admit<'issuer, R: GraphSymbolResolver, C: FnMut() -> u64>(
    namespace: fgdb_types::DatabaseSecurityNamespaceId,
    cx: &QueryCx,
    authority: &'issuer Authority,
    token: &CapabilityToken,
    branch: &str,
    resolver: R,
    policy: GqlQueryPolicy,
    mut clock: C,
    pin: impl FnOnce() -> Result<EmbeddedReadView, crate::ReadError>,
) -> Result<AuthorizedReadTransaction<'issuer, R, C>, QueryError> {
    if authority.namespace() != namespace {
        return Err(QueryError::Authorization(
            AuthorizationError::WrongAuthority,
        ));
    }
    cx.with_restriction(|| {
        let now = clock();
        let verified = authority
            .verify_at(token, branch, now)
            .map_err(QueryError::Authorization)?;
        let scope = verified.predicates().clone();
        let permit = verified
            .into_read_at(branch, now)
            .map_err(QueryError::Authorization)?;
        let (view, permit) = {
            let mut execution = Execution::new(cx, permit, &mut clock);
            execution.checkpoint()?;
            let view = pin().map_err(QueryError::Read)?;
            execution.deliver(0)?;
            (view, execution.permit)
        };
        let basis = view.frontier();
        Ok(AuthorizedReadTransaction {
            state: Some(State {
                view,
                permit,
                scope,
                branch: branch.to_owned(),
                resolver,
                policy,
                clock,
            }),
            basis,
            completed_statements: 0,
        })
    })
}
