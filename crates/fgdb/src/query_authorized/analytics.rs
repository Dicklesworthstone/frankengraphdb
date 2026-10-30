//! Prism consumes the capability's induced graph, never a post-filtered answer.
//!
//! Reuse the authenticated historical visitors, the owned projection builder,
//! and the registered native kernels. The default and every restricted scope
//! use DECODED_CACHE. An owning sealed session may supply its retained image
//! for compatible calls only after complete incidence/field visibility is
//! proved; that path keeps the same live permit and final delivery boundary.
//! Neither path publishes a reusable graph or treats an image as authority.
//!
//! Projections are retained in a trusted-host sidecar keyed by the live
//! snapshot allocation and exact request (fgdb-0tbrz), never handed to a token
//! holder. Scoped entries also bind every compiled predicate exactly. Every
//! reuse spends the build's exact sequence of live calls again under its
//! current permit, so it confers nothing a fresh build would not.

use super::tape::{Step, Tape};
use super::{Execution, authorized_with_errors};
use crate::gql_exec::AdmissionUsage;
use crate::gql_exec::source::{self, SourceEvent};
use crate::query::prism::{LiveCache, ProjectionRequest, push_staged, source_admit};
use crate::{Database, QueryError, ReadError, Snapshot};
use asupersync::fs::Vfs;
use fgdb_gql::GqlQueryPolicy;
use fgdb_prism::{
    Directedness, FnxCallSpec, FnxExecutionError, FnxGraphKind, FnxMeter, FnxParameters,
    FnxReadError, FnxReadOptions, FnxRowCursor, FnxRowSink, FnxValue, ParallelEdgePolicy,
    ProjectionBuildError, ProjectionEdge, ProjectionError, SelfLoopPolicy, SnapshotBinding,
    SnapshotGraphView, SortedVertexLookup,
};
use fgdb_types::{QueryCx, VId};
use fgdb_warden::{Authority, CapabilityToken, PlannerPredicates};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;

#[path = "analytics/sealed.rs"]
pub(super) mod sealed;

type Error = FnxReadError<ReadError, QueryError>;

/// Projections reused across authorized calls on one live snapshot
/// allocation, each with the live calls its build made (fgdb-0tbrz).
pub(crate) type AuthorizedProjectionCache =
    LiveCache<Snapshot, ProjectionKey, (SnapshotGraphView, Arc<Tape>)>;

/// Serializes tests that change shared simulation instrumentation. Production
/// reuse belongs to each database and does not depend on this test lock.
#[cfg(test)]
pub(crate) fn reuse_test_turn() -> std::sync::MutexGuard<'static, ()> {
    static TURN: std::sync::Mutex<()> = std::sync::Mutex::new(());
    TURN.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// None retains the existing whole-snapshot key: when no field or incidence
/// is masked, changing only live allowances does not change the graph or its
/// replay tape. A scoped key keeps the complete, exact compiled program; no
/// principal, token spelling or digest stands in for its actual predicates.
#[derive(PartialEq, Eq)]
pub(crate) struct ProjectionKey {
    request: ProjectionRequest,
    predicates: Option<Vec<u8>>,
}

impl ProjectionKey {
    fn new(request: ProjectionRequest, scope: &PlannerPredicates) -> Option<Self> {
        let predicates = if scope.sees_all_incidence() && scope.sees_all_fields() {
            None
        } else {
            Some(scope.cache_identity()?)
        };
        Some(Self {
            request,
            predicates,
        })
    }

    fn retained_bytes(&self) -> usize {
        self.predicates.as_ref().map_or(0, Vec::capacity)
    }
}

fn remember_projection_controlled<Owner, E>(
    cache: &LiveCache<Owner, ProjectionKey, (SnapshotGraphView, Arc<Tape>)>,
    owner: &Arc<Owner>,
    key: ProjectionKey,
    graph: &SnapshotGraphView,
    tape: Tape,
    poll: &mut impl FnMut() -> Result<(), E>,
) -> Result<(), E> {
    // Retain the ordinary projection cache's source/spec checks as well as
    // the scoped key and complete-tape requirement. A failed or truncated
    // build can never seed a different request's reusable entry.
    if graph.binding() != key.request.binding
        || graph.spec() != key.request.projection
        || !tape.is_complete()
    {
        return Ok(());
    }
    if let Some(bytes) = graph
        .charged_resident_bytes()
        .checked_add(tape.bytes())
        .and_then(|bytes| bytes.checked_add(key.retained_bytes()))
    {
        cache.remember(owner, key, (graph.clone(), Arc::new(tape)), bytes, poll)?;
    }
    Ok(())
}

#[cfg(test)]
thread_local! {
    /// Projection builds on this thread: the tests' physical reuse witness.
    static BUILDS: Cell<usize> = const { Cell::new(0) };
    /// Records a reuse replayed in one live-execution loop.
    static BULK_RECORDS: Cell<u64> = const { Cell::new(0) };
}

// Like governed GQL, the interruption carrier preserves a live authorization
// refusal. Keep source/frontier errors in their ordinary source error class.
fn control_error(error: QueryError) -> Error {
    match error {
        QueryError::Read(error) => Error::Read(error),
        other => Error::Cancelled(other),
    }
}

pub(super) fn preflight(call: &FnxCallSpec, direction: Directedness) -> Result<(), Error> {
    let required = call.signature().graph_kind;
    let compatible = match required {
        FnxGraphKind::Any => true,
        FnxGraphKind::Directed => direction != Directedness::Undirected,
        FnxGraphKind::Undirected => direction == Directedness::Undirected,
    };
    if compatible {
        Ok(())
    } else {
        Err(Error::Execution(FnxExecutionError::GraphKind { required }))
    }
}

impl<V: Vfs + Clone> Database<V> {
    /// Bind CALL/YIELD, then execute on the current capability-visible graph.
    /// `options.as_of` selects history under the SAME current capability.
    ///
    /// Rows follow the bound YIELD order; use `FnxCallSpec::outputs` for the
    /// public schema. No source root, projection, certificate, raw cardinality,
    /// witness or resource counter is returned. Unknown procedures fail during
    /// binding without touching the graph. No result prefix escapes on failure.
    /// See `execute_fnx_authorized` for the host/security and resource contract.
    #[allow(clippy::too_many_arguments)]
    pub fn call_fnx_authorized(
        &self,
        cx: &QueryCx,
        authority: &Authority,
        token: &CapabilityToken,
        branch: &str,
        text: &str,
        parameters: &FnxParameters,
        options: FnxReadOptions,
        clock: impl FnMut() -> u64,
    ) -> Result<Vec<Vec<FnxValue>>, FnxReadError<ReadError, QueryError>> {
        let call = FnxCallSpec::bind(text, parameters).map_err(Error::Bind)?;
        self.execute_fnx_authorized(cx, authority, token, branch, &call, options, clock)
    }

    /// Execute a registered Prism call under Warden's signed read allowance.
    ///
    /// The trusted host chooses Authority, branch mapping and a monotone clock
    /// in the issuer's millisecond epoch, as for authorized GQL. Namespace,
    /// signature, rights and validity are checked before source access. Keep
    /// the ordinary Database APIs, issuer and clock out of token-holder reach.
    ///
    /// Select historical winners first; apply all original label clauses next.
    /// Only permitted labels can satisfy the user's label selector. Both
    /// selected endpoints and the relation must admit before weight resolution
    /// or adjacency assembly. A forbidden weight property behaves as absent,
    /// including its explicit Reject/Unit/Zero missing policy. CollapseUnit and
    /// dropped loops never observe their discarded properties. All directions,
    /// multigraph laws and kernels are the ordinary Prism implementation.
    ///
    /// Signed nodes count capability-admitted vertices before user selection;
    /// signed work spans source, builder, kernel and delivery checkpoints.
    /// Signed rows cap kernel result admission before retaining output, then
    /// are charged at final delivery. Native source/staging/projection/kernel
    /// limits ALSO apply. Authorization refusals remain inspectable as
    /// `FnxReadError::Cancelled(QueryError::Authorization(..))`; query-context
    /// cancellation retains its native QueryError. No partially built graph or
    /// reusable permit is returned, including for an empty result.
    ///
    /// This is resident-source, in-core decoded execution, not compressed or
    /// external-memory authorization. Mixed-scope history may be inspected and
    /// charged by existing source visitors. This does NOT prove descriptor-I/O,
    /// timing, error-detail or resource-failure noninterference, or authorize
    /// the other privileged database APIs. A `CALL fnx.*` composed inside an
    /// authorized native read runs through this same `execute` over the same
    /// masked source (fgdb-7qznp).
    #[allow(clippy::too_many_arguments)]
    pub fn execute_fnx_authorized(
        &self,
        cx: &QueryCx,
        authority: &Authority,
        token: &CapabilityToken,
        branch: &str,
        call: &FnxCallSpec,
        options: FnxReadOptions,
        clock: impl FnMut() -> u64,
    ) -> Result<Vec<Vec<FnxValue>>, FnxReadError<ReadError, QueryError>> {
        authorized_with_errors(
            self,
            cx,
            authority,
            token,
            branch,
            options.as_of,
            clock,
            control_error,
            |snapshot, at, scope, execution| {
                preflight(call, options.projection.directedness)?;
                // The database is immutably borrowed throughout this execution.
                // Bind the actual retained root only after authority admission;
                // this private provenance and the built graph never escape.
                let view = self.read_session().map_err(Error::Read)?;
                let binding = SnapshotBinding {
                    root: view.partition_root().0,
                    as_of: at,
                };
                execute(
                    snapshot, binding, call, options, scope, execution, Unmetered,
                )
            },
        )
    }
}

// The optional caller meter adds fixed session policy to the SAME source,
// builder and kernel. It never replaces the live signed execution permit.
pub(super) fn execute<Clock: FnMut() -> u64>(
    snapshot: &Arc<Snapshot>,
    binding: SnapshotBinding,
    call: &FnxCallSpec,
    options: FnxReadOptions,
    scope: &PlannerPredicates,
    execution: &RefCell<Execution<'_, '_, Clock>>,
    observe: impl Observe,
) -> Result<Vec<Vec<FnxValue>>, Error> {
    execute_with_sink(
        snapshot,
        binding,
        call,
        options,
        scope,
        execution,
        observe,
        ResultSink::Collected,
    )
}

/// Keep result conversion inside the trusted host while preserving the
/// projection's exact live replay and the kernel's signed-work settlement.
/// The sink is discard-only until this function and the delivery gate succeed.
#[allow(clippy::too_many_arguments)]
pub(super) fn execute_into<Clock: FnMut() -> u64>(
    snapshot: &Arc<Snapshot>,
    binding: SnapshotBinding,
    call: &FnxCallSpec,
    options: FnxReadOptions,
    scope: &PlannerPredicates,
    execution: &RefCell<Execution<'_, '_, Clock>>,
    observe: impl Observe,
    sink: &mut dyn FnxRowSink<QueryError>,
) -> Result<(), Error> {
    execute_with_sink(
        snapshot,
        binding,
        call,
        options,
        scope,
        execution,
        observe,
        ResultSink::Mapped(sink),
    )
    .map(|_| ())
}

/// Complete the same masked source and kernel, retaining its existing value
/// arrays only inside the current authorized operation. The caller must
/// reserve the additional pull pass before exposing its first downstream row.
/// Neither this cursor nor its private certificate is a capability response.
#[allow(clippy::too_many_arguments)]
pub(super) fn execute_rows<Clock: FnMut() -> u64>(
    snapshot: &Arc<Snapshot>,
    binding: SnapshotBinding,
    call: &FnxCallSpec,
    options: FnxReadOptions,
    scope: &PlannerPredicates,
    execution: &RefCell<Execution<'_, '_, Clock>>,
    observe: impl Observe,
) -> Result<FnxRowCursor, Error> {
    let mut retained = None;
    execute_with_sink(
        snapshot,
        binding,
        call,
        options,
        scope,
        execution,
        observe,
        ResultSink::Retained(&mut retained),
    )?;
    retained.ok_or(Error::Execution(FnxExecutionError::InvalidUpstreamResult))
}

/// Charge the complete visible-vertex pull pass to both meters, before any
/// consumer. Sparse distances still inspect unreachable visible vertices.
/// Real pulls use fresh uncharged permission/cancellation checks; they must
/// not spend this reservation a second time. Hidden graph size never enters it.
pub(super) fn reserve_cursor_pull<Clock: FnMut() -> u64>(
    vertices: usize,
    execution: &RefCell<Execution<'_, '_, Clock>>,
    mut observe: impl Observe,
) -> Result<(), Error> {
    super::work_runs(execution, control_error, || {
        let mut live = execution.borrow_mut();
        let mut meter = KernelMeter {
            execution: &mut live,
            observe: &mut observe,
        };
        for start in (0..vertices).step_by(fgdb_prism::FNX_METER_RUN) {
            meter
                .units((vertices - start).min(fgdb_prism::FNX_METER_RUN))
                .map_err(control_error)?;
        }
        meter.poll().map_err(control_error)
    })
}

/// One synchronous statement's private checkpoint mode. An owned cursor may
/// retain this flag, but never its borrowed execution permit. While it pulls
/// prepaid source values, its host callback checks fresh authority without
/// charging those values again; every ordinary evaluator callback still pays.
#[derive(Default)]
pub(super) struct CursorControl(Rc<Cell<bool>>);

impl CursorControl {
    pub(super) fn checkpoint<Clock: FnMut() -> u64>(
        &self,
        execution: &RefCell<Execution<'_, '_, Clock>>,
    ) -> Result<(), QueryError> {
        let mut live = execution.borrow_mut();
        if self.0.get() {
            poll_live(&mut live)
        } else {
            live.checkpoint()
        }
    }

    pub(super) fn wrap(
        &self,
        rows: crate::query::prism::ProcedureRows,
    ) -> Box<dyn fgdb_gql::GraphProcedureRowCursor<crate::GqlError, QueryError>> {
        Box::new(AuthorizedRows {
            rows,
            prepaid: Rc::clone(&self.0),
        })
    }
}

struct AuthorizedRows {
    rows: crate::query::prism::ProcedureRows,
    prepaid: Rc<Cell<bool>>,
}

impl fgdb_gql::GraphProcedureRowCursor<crate::GqlError, QueryError> for AuthorizedRows {
    fn next_row(
        &mut self,
        checkpoint: &mut dyn FnMut() -> Result<(), QueryError>,
    ) -> Result<
        Option<fgdb_gql::algebra::GraphValueRow>,
        fgdb_gql::GqlQueryError<crate::GqlError, QueryError>,
    > {
        struct Restore<'a>(&'a Cell<bool>, bool);
        impl Drop for Restore<'_> {
            fn drop(&mut self) {
                self.0.set(self.1);
            }
        }
        let _restore = Restore(&self.prepaid, self.prepaid.replace(true));
        let row = self.rows.next_row(checkpoint)?;
        if row.is_none() {
            // Even an empty kernel or an exhausted sparse suffix observes
            // current cancellation, retirement and expiry at the drain edge.
            checkpoint().map_err(fgdb_gql::GqlQueryError::Interrupted)?;
        }
        Ok(row)
    }
}

enum ResultSink<'a> {
    Collected,
    Mapped(&'a mut dyn FnxRowSink<QueryError>),
    Retained(&'a mut Option<FnxRowCursor>),
}

#[allow(clippy::too_many_arguments)]
fn execute_with_sink<Clock: FnMut() -> u64>(
    snapshot: &Arc<Snapshot>,
    binding: SnapshotBinding,
    call: &FnxCallSpec,
    options: FnxReadOptions,
    scope: &PlannerPredicates,
    execution: &RefCell<Execution<'_, '_, Clock>>,
    observe: impl Observe,
    sink: ResultSink<'_>,
) -> Result<Vec<Vec<FnxValue>>, Error> {
    // The owning sealed session fixes this context; request code cannot add
    // an image. Restricted or incompatible calls keep the ordinary masked
    // source, including its exact cache key and observable admission law.
    let retained = execution.borrow().sealed.clone();
    if let Some(retained) = retained
        && retained.eligible(snapshot, binding, call, options, scope)
    {
        return sealed::execute(
            &retained, snapshot, binding, call, options, scope, execution, observe, sink,
        );
    }
    let observe = RefCell::new(observe);
    let checkpoint = || {
        execution.borrow_mut().checkpoint()?;
        observe.borrow_mut().event(SourceEvent::Work)
    };
    let (work, scratch) = (Cell::new(0u64), Cell::new(0u64));
    let control = |event: SourceEvent| -> Result<(), Error> {
        execution.borrow_mut().checkpoint().map_err(control_error)?;
        observe.borrow_mut().event(event).map_err(control_error)?;
        let (counter, limit, resource) = match event {
            SourceEvent::Work | SourceEvent::SnapshotRecord => {
                (&work, options.source_limits.max_work_units, "work units")
            }
            SourceEvent::ScratchEntry => (
                &scratch,
                options.source_limits.max_scratch_entries,
                "scratch entries",
            ),
        };
        let count = counter.get().checked_add(1).ok_or(Error::SizeOverflow)?;
        counter.set(count);
        source_admit(resource, u128::from(count), u128::from(limit))
    };
    // FG-INV-20: history the capability cannot see is walked through `poll`,
    // which polls cancellation and charges nothing. Only records that pass the
    // scope (and, for edges, whose endpoints were admitted) are charged, so no
    // signed or source limit's refusal point depends on hidden data.
    let poll = || execution.borrow_mut().poll().map_err(control_error);
    // Each admitted record's and builder checkpoint's live calls. A build and
    // a reuse both spend through here, so their sequences cannot drift apart.
    let spend = |step: Step| -> Result<(), Error> {
        match step {
            Step::Vertex(labels) => {
                control(SourceEvent::Work)?;
                for _ in 0..labels {
                    control(SourceEvent::Work)?;
                }
                execution.borrow_mut().node().map_err(control_error)?;
            }
            Step::Edge => control(SourceEvent::Work)?,
            Step::Checkpoint => return checkpoint().map_err(control_error),
        }
        observe
            .borrow_mut()
            .event(SourceEvent::SnapshotRecord)
            .map_err(control_error)?;
        observe
            .borrow_mut()
            .event(SourceEvent::ScratchEntry)
            .map_err(control_error)
    };
    // The graph and its live call sequence are functions of the exact source
    // allocation/cut, request and compiled masks. Authentication has already
    // produced this execution's fresh permit; a cached graph grants nothing.
    // Different principals may share derived data only when those inputs are
    // exactly equal, and all live charges still reach their own permits.
    let request = ProjectionRequest {
        binding,
        selection: options.selection,
        projection: options.projection,
        projection_limits: options.projection_limits,
        source_limits: options.source_limits,
    };
    let key = ProjectionKey::new(request, scope);
    let cache = &snapshot.read_caches.authorized;
    // Admission and the projection's replay or build are one phase: their
    // signed work is charged in runs.
    let graph = super::work_runs(execution, control_error, || {
        let retained = match key.as_ref() {
            Some(key) => cache.lookup_controlled(snapshot, key, &mut || poll())?,
            None => None,
        };
        Ok::<_, Error>(match retained {
            Some((graph, tape)) => {
                replay(&tape, execution, &observe, &work, options, poll, spend)?;
                graph
            }
            None => {
                // A fragmented masked history may not run-length encode
                // compactly. Bound temporary replay metadata by both the
                // source staging ceiling and this cache's allowance. If
                // it cannot fit, the build succeeds without caching.
                let tape = key.as_ref().map(|key| {
                    RefCell::new(Tape::bounded(
                        options
                            .source_limits
                            .max_staging_bytes
                            .min(cache.max_bytes())
                            .saturating_sub(key.retained_bytes()),
                    ))
                });
                let graph = build(
                    snapshot,
                    binding,
                    options,
                    scope,
                    tape.as_ref(),
                    poll,
                    spend,
                )?;
                if let (Some(key), Some(tape)) = (key, tape) {
                    let tape = tape.into_inner().finish();
                    remember_projection_controlled(
                        cache,
                        snapshot,
                        key,
                        &graph,
                        tape,
                        &mut || poll(),
                    )?;
                }
                graph
            }
        })
    })?;
    let signed_rows = scope.limits().max_rows;
    let mut limits = options.execution_limits;
    limits.max_result_rows = limits
        .max_result_rows
        .min(usize::try_from(signed_rows).unwrap_or(usize::MAX));
    // The kernel's units are charged to the signed permit in runs, and the
    // caller meter still sees each one. Nothing else borrows either while the
    // kernel runs.
    let outer = std::mem::replace(&mut execution.borrow_mut().in_runs, true);
    let result = {
        let meter = KernelMeter {
            execution: &mut execution.borrow_mut(),
            observe: &mut *observe.borrow_mut(),
        };
        match sink {
            ResultSink::Mapped(sink) => call
                .execute_into(&graph, limits, meter, sink)
                .map(|_| Vec::new()),
            ResultSink::Collected => call
                .execute(&graph, limits, meter)
                .map(|result| result.rows),
            ResultSink::Retained(retained) => {
                call.execute_rows(&graph, limits, meter).map(|cursor| {
                    *retained = Some(cursor);
                    Vec::new()
                })
            }
        }
    };
    // Every unit the kernel spent is charged before its result or its own
    // error is judged, as it was when each was charged on the spot. A kernel
    // our control stopped keeps that refusal and charges nothing more.
    {
        let mut live = execution.borrow_mut();
        live.in_runs = outer;
        if matches!(result, Err(FnxExecutionError::Cancelled(_))) {
            live.discard_runs();
        } else {
            live.settle().map_err(control_error)?;
        }
    }
    // Translate a signed-row refusal back to Warden, without allocating a
    // forbidden result first. The shared delivery gate still checks successful
    // results with fresh time; zero rows do not bypass retirement or expiry.
    if let Err(FnxExecutionError::LimitExceeded {
        resource: "result rows",
        requested,
        ..
    }) = &result
        && *requested as u128 > u128::from(signed_rows)
    {
        execution
            .borrow_mut()
            .deliver(*requested)
            .map_err(control_error)?;
    }
    let rows = result.map_err(|error| match error {
        FnxExecutionError::Cancelled(error) => control_error(error),
        other => Error::Execution(other),
    })?;
    // The kernel's certificate is trusted-host provenance, not a capability
    // response. Releasing it could expose the unmasked root/history identity.
    Ok(rows)
}

/// Replay `tape` exactly as `Tape::replay` through `poll` and `spend`, but a
/// run whose caller and source meters provably cannot refuse anywhere in it
/// (every one of its events fits their allowances) is replayed as one loop
/// over the live execution: each item's polls, work units and node in their
/// order, so every signed charge, cancellation poll and clock read happens
/// exactly where it did. Only the meters' pure counters, which no event of
/// the run can trip, are added once - for the items the loop completed, as
/// item-by-item replay would have left them.
fn replay<Clock: FnMut() -> u64, O: Observe>(
    tape: &Tape,
    execution: &RefCell<Execution<'_, '_, Clock>>,
    observe: &RefCell<O>,
    work: &Cell<u64>,
    options: FnxReadOptions,
    poll: impl Fn() -> Result<(), Error>,
    spend: impl Fn(Step) -> Result<(), Error>,
) -> Result<(), Error> {
    for (polls, step, count) in tape.runs() {
        // Per item, as `spend` charges it: its live work units, whether it
        // is a signed node, and whether it is an admitted record. Each unit
        // of a record is also a source work unit and a caller Work event,
        // and the record then a SnapshotRecord and a ScratchEntry event,
        // each of which the caller meter counts as work too. A builder
        // checkpoint is one live unit and one caller Work event; polls alone
        // charge nothing.
        let (units, node, record) = match step {
            Some(Step::Vertex(labels)) => (1 + u64::from(labels), true, true),
            Some(Step::Edge) => (1, false, true),
            Some(Step::Checkpoint) => (1, false, false),
            None => (0, false, false),
        };
        if count > 1 {
            let n = u64::from(count);
            let (records, source, meter) = if record {
                (n, units.checked_mul(n), (units + 2).checked_mul(n))
            } else {
                (0, Some(0), units.checked_mul(n))
            };
            let source_fits = source
                .and_then(|more| work.get().checked_add(more))
                .is_some_and(|total| total <= options.source_limits.max_work_units);
            let meter_fits =
                meter.is_some_and(|more| observe.borrow().absorbs(records, more, records));
            if source_fits && meter_fits {
                // Items completed, and live units of the item a refusal
                // stopped, each of which `spend` would already have counted.
                let (mut done, mut partial) = (0_u64, 0_u64);
                let ran = (|| {
                    let mut live = execution.borrow_mut();
                    // Nothing comes between these items' live units, so they
                    // are one stretch of units; a remainder is the units of
                    // the item a refusal stopped.
                    if let Some(total) = units.checked_mul(n).filter(|_| polls == 0 && !node) {
                        let mut taken = 0;
                        let stretch = live.units_one_by_one(total, &mut taken);
                        if let (Some(items), Some(rest)) =
                            (taken.checked_div(units), taken.checked_rem(units))
                        {
                            (done, partial) = (items, rest);
                        }
                        return stretch.map_err(control_error);
                    }
                    for _ in 0..count {
                        for _ in 0..polls {
                            live.poll().map_err(control_error)?;
                        }
                        for _ in 0..units {
                            live.checkpoint().map_err(control_error)?;
                            partial += 1;
                        }
                        if node {
                            live.node().map_err(control_error)?;
                        }
                        (done, partial) = (done + 1, 0);
                    }
                    Ok::<_, Error>(())
                })();
                #[cfg(test)]
                if record {
                    BULK_RECORDS.with(|bulk| bulk.set(bulk.get() + done));
                }
                let (records, meter) = if record {
                    work.set(work.get() + units * done + partial);
                    (done, (units + 2) * done + partial)
                } else {
                    (0, units * done + partial)
                };
                observe.borrow_mut().absorb(records, meter, records);
                ran?;
                continue;
            }
        }
        for _ in 0..count {
            for _ in 0..polls {
                poll()?;
            }
            if let Some(step) = step {
                spend(step)?;
            }
        }
    }
    Ok(())
}

/// Visit, admit and build. `tape`, when present, records every live call in
/// order for a later reuse.
fn build(
    snapshot: &Snapshot,
    binding: SnapshotBinding,
    options: FnxReadOptions,
    scope: &PlannerPredicates,
    tape: Option<&RefCell<Tape>>,
    poll: impl Fn() -> Result<(), Error>,
    spend: impl Fn(Step) -> Result<(), Error>,
) -> Result<SnapshotGraphView, Error> {
    #[cfg(test)]
    BUILDS.with(|builds| builds.set(builds.get() + 1));
    let mut scan = |_| {
        if let Some(tape) = tape {
            tape.borrow_mut().poll();
        }
        poll()
    };
    let admit = |step| {
        if let Some(tape) = tape {
            tape.borrow_mut().emit(Some(step));
        }
        spend(step)
    };
    let mut vertices: Vec<VId> = Vec::new();
    let mut staged_bytes = 0;
    source::visit_vertices(&snapshot.patches, binding.as_of, &mut scan, |row, _| {
        if !scope.allows_vertex(&row.labels) {
            return Ok(());
        }
        let labels = row
            .labels
            .iter()
            .filter(|&&label| scope.allows_label(label))
            .count();
        admit(Step::Vertex(
            u32::try_from(labels).map_err(|_| Error::SizeOverflow)?,
        ))?;
        if options.selection.vertex_label.is_some_and(|label| {
            !scope.allows_label(label) || row.labels.binary_search(&label).is_err()
        }) {
            return Ok(());
        }
        push_staged(
            &mut vertices,
            row.vid,
            "vertices",
            options.projection_limits.max_vertices,
            &mut staged_bytes,
            options.source_limits.max_staging_bytes,
        )
    })?;
    let selected_vertices = SortedVertexLookup::from_sorted(&vertices);
    let mut edges = Vec::new();
    source::visit_edges_in_relation_with_properties(
        snapshot,
        binding.as_of,
        options.selection.relation,
        &mut scan,
        |entry, props, _| {
            if !scope.allows_relation(entry.relation)
                || !selected_vertices.contains(entry.src)
                || !selected_vertices.contains(entry.dst)
            {
                return Ok(());
            }
            admit(Step::Edge)?;
            if entry.src == entry.dst {
                match options.projection.self_loops {
                    SelfLoopPolicy::Drop => return Ok(()),
                    SelfLoopPolicy::Reject => {
                        return Err(Error::Projection(ProjectionError::SelfLoop(entry.eid)));
                    }
                    SelfLoopPolicy::Keep => {}
                }
            }
            let weight = if options.projection.parallel_edges == ParallelEdgePolicy::CollapseUnit {
                1.0
            } else {
                let value = options
                    .selection
                    .weight
                    .property_key()
                    .filter(|&key| scope.allows_property(key))
                    .and_then(|key| props.binary_search_by_key(&key, |(key, _)| *key).ok())
                    .map(|index| &props[index].1);
                options
                    .selection
                    .weight
                    .resolve(value)
                    .map_err(|reason| Error::Weight {
                        edge: entry.eid,
                        reason,
                    })?
            };
            push_staged(
                &mut edges,
                ProjectionEdge {
                    eid: entry.eid,
                    source: entry.src,
                    target: entry.dst,
                    weight,
                },
                "input edges",
                options.projection_limits.max_input_edges,
                &mut staged_bytes,
                options.source_limits.max_staging_bytes,
            )
        },
    )?;
    SnapshotGraphView::build_owned_with_checkpoint(
        binding,
        vertices,
        edges,
        options.projection,
        options.projection_limits,
        || admit(Step::Checkpoint),
    )
    .map_err(|error| match error {
        ProjectionBuildError::Cancelled(error) => error,
        ProjectionBuildError::Projection(error) => Error::Projection(error),
    })
}

/// The caller meter an analytics call reports to, every event in order.
/// `work(n)` is n Work events, with the same total and refusal ordinal; a
/// counting meter answers it in one step.
pub(super) trait Observe {
    fn event(&mut self, event: SourceEvent) -> Result<(), QueryError>;
    fn work(&mut self, n: usize) -> Result<(), QueryError> {
        (0..n).try_for_each(|_| self.event(SourceEvent::Work))
    }
    /// Whether `records` admitted records, `work` Work events and `scratch`
    /// scratch entries (each record also one SnapshotRecord event) can all be
    /// observed with no refusal among them. A meter that cannot tell says no,
    /// and every record is then observed event by event.
    fn absorbs(&self, _records: u64, _work: u64, _scratch: u64) -> bool {
        false
    }
    /// Observe counts [`Self::absorbs`] admitted, in one step.
    fn absorb(&mut self, _records: u64, _work: u64, _scratch: u64) {
        unreachable!("absorb follows a successful absorbs")
    }
}

impl<F: FnMut(SourceEvent) -> Result<(), QueryError>> Observe for F {
    fn event(&mut self, event: SourceEvent) -> Result<(), QueryError> {
        self(event)
    }
}

/// No caller meter beside the signed permit: it observes nothing, so no
/// count can refuse.
struct Unmetered;

impl Observe for Unmetered {
    fn event(&mut self, _: SourceEvent) -> Result<(), QueryError> {
        Ok(())
    }
    fn absorbs(&self, _records: u64, _work: u64, _scratch: u64) -> bool {
        true
    }
    fn absorb(&mut self, _records: u64, _work: u64, _scratch: u64) {}
}

/// A caller meter over one [`AdmissionUsage`] allowance.
pub(super) struct UsageObserver<'u> {
    pub(super) usage: &'u mut AdmissionUsage,
    pub(super) policy: GqlQueryPolicy,
}

impl Observe for UsageObserver<'_> {
    fn event(&mut self, event: SourceEvent) -> Result<(), QueryError> {
        self.usage
            .observe::<ReadError, QueryError>(self.policy, event)
            .map_err(super::query_error)
    }
    fn work(&mut self, n: usize) -> Result<(), QueryError> {
        self.usage
            .observe_work::<ReadError, QueryError>(self.policy, n as u64)
            .map_err(super::query_error)
    }
    fn absorbs(&self, records: u64, work: u64, scratch: u64) -> bool {
        self.usage.absorbs(self.policy, records, work, scratch)
    }
    fn absorb(&mut self, records: u64, work: u64, scratch: u64) {
        self.usage.absorb(records, work, scratch);
    }
}

/// A Prism kernel's meter over the live execution. Signed and caller work
/// interleave in that order, even when their limits would refuse within the
/// same run. A caller that admits the whole run may count the signed meter's
/// completed prefix in bulk (fgdb-62mzm).
struct KernelMeter<'m, 'cx, 'permit, Clock, O> {
    execution: &'m mut Execution<'cx, 'permit, Clock>,
    observe: &'m mut O,
}

fn poll_live<Clock: FnMut() -> u64>(
    execution: &mut Execution<'_, '_, Clock>,
) -> Result<(), QueryError> {
    execution.poll()?;
    // Arithmetic or cursor traversal has already charged its work. A cached
    // charge-time sample must not hide expiry throughout this physical phase.
    let now = execution.sample();
    let checked = execution.permit.checkpoint_at(now);
    checked.map_err(|error| execution.refusal(error))
}

impl<Clock, O> FnxMeter<QueryError> for KernelMeter<'_, '_, '_, Clock, O>
where
    Clock: FnMut() -> u64,
    O: Observe,
{
    fn poll(&mut self) -> Result<(), QueryError> {
        poll_live(self.execution)
    }
    fn unit(&mut self) -> Result<(), QueryError> {
        self.execution.work_units(1)?;
        self.observe.work(1)
    }
    fn units(&mut self, n: usize) -> Result<(), QueryError> {
        if n > 1 && self.observe.absorbs(0, n as u64, 0) {
            // The caller cannot refuse anywhere in this run, but the signed
            // permit still can. Keep its per-unit settlement/poll boundaries
            // and report every completed unit, including a partial prefix
            // when the signed charge refuses.
            let mut done = 0;
            let result = self.execution.units_one_by_one(n as u64, &mut done);
            self.observe.work(done as usize)?;
            return result;
        }
        // Without that proof, charging all signed units first could hide an
        // earlier caller refusal behind a later signed refusal.
        for _ in 0..n {
            self.unit()?;
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "analytics_tests.rs"]
mod tests;
