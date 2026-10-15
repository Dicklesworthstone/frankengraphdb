//! Prism consumes the capability's induced graph, never a post-filtered answer.
//!
//! Reuse the authenticated historical visitors, the owned projection builder,
//! and the registered native kernels. This path is explicitly DECODED_CACHE:
//! it does not confer authority on raw sealed images or publish a reusable view
//! that could outlive the single live Warden execution allowance.
//!
//! An unrestricted scope's projection is retained in a trusted-host sidecar
//! keyed by the live snapshot allocation (fgdb-0tbrz), never handed to a token
//! holder. Every reuse spends the build's exact sequence of live calls again,
//! so it confers nothing a fresh build would not.

use super::tape::{Step, Tape};
use super::{Execution, authorized_with_errors};
use crate::gql_exec::source::{self, SourceEvent};
use crate::query::prism::{
    PROJECTION_CACHE_BYTES, ProjectionCache, ProjectionRequest, push_staged, source_admit,
};
use crate::{Database, QueryError, ReadError, Snapshot};
use asupersync::fs::Vfs;
use fgdb_prism::{
    Directedness, FnxCallSpec, FnxExecutionError, FnxGraphKind, FnxMeter, FnxParameters,
    FnxReadError, FnxReadOptions, FnxValue, ParallelEdgePolicy, ProjectionBuildError,
    ProjectionEdge, ProjectionError, SelfLoopPolicy, SnapshotBinding, SnapshotGraphView,
};
use fgdb_types::{QueryCx, VId};
use fgdb_warden::{Authority, CapabilityToken, PlannerPredicates};
use std::cell::{Cell, RefCell};
use std::sync::Arc;

type Error = FnxReadError<ReadError, QueryError>;

/// Projections reused across authorized calls on one live snapshot
/// allocation, each with the live calls its build made (fgdb-0tbrz).
static REUSE: ProjectionCache<Snapshot, Arc<Tape>> = ProjectionCache::new(PROJECTION_CACHE_BYTES);

#[cfg(test)]
thread_local! {
    /// Projection builds on this thread: the tests' physical reuse witness.
    static BUILDS: Cell<usize> = const { Cell::new(0) };
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
                execute(snapshot, binding, call, options, scope, execution, |_| {
                    Ok(())
                })
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
    observe: impl FnMut(SourceEvent) -> Result<(), QueryError>,
) -> Result<Vec<Vec<FnxValue>>, Error> {
    let observe = RefCell::new(observe);
    let checkpoint = || {
        execution.borrow_mut().checkpoint()?;
        observe.borrow_mut()(SourceEvent::Work)
    };
    let (work, scratch) = (Cell::new(0u64), Cell::new(0u64));
    let control = |event: SourceEvent| -> Result<(), Error> {
        execution.borrow_mut().checkpoint().map_err(control_error)?;
        observe.borrow_mut()(event).map_err(control_error)?;
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
        observe.borrow_mut()(SourceEvent::SnapshotRecord).map_err(control_error)?;
        observe.borrow_mut()(SourceEvent::ScratchEntry).map_err(control_error)
    };
    // An unrestricted scope sees the whole snapshot, so its projection and
    // live call sequence depend only on the snapshot and this request. A
    // scoped capability always builds its own.
    let request = ProjectionRequest {
        binding,
        selection: options.selection,
        projection: options.projection,
        projection_limits: options.projection_limits,
        source_limits: options.source_limits,
    };
    let reusable = scope.sees_all_incidence() && scope.sees_all_fields();
    let graph = match reusable.then(|| REUSE.lookup(snapshot, request)).flatten() {
        Some((graph, tape)) => {
            tape.replay(poll, spend)?;
            graph
        }
        None => {
            let tape = reusable.then(|| RefCell::new(Tape::default()));
            let graph = build(
                snapshot,
                binding,
                options,
                scope,
                tape.as_ref(),
                poll,
                spend,
            )?;
            if let Some(tape) = tape {
                let tape = tape.into_inner().finish();
                let bytes = tape.bytes();
                REUSE.remember_graph(snapshot, request, &graph, Arc::new(tape), bytes);
            }
            graph
        }
    };
    let signed_rows = scope.limits().max_rows;
    let mut limits = options.execution_limits;
    limits.max_result_rows = limits
        .max_result_rows
        .min(usize::try_from(signed_rows).unwrap_or(usize::MAX));
    // The kernel's units are charged to the signed permit in runs, and the
    // caller meter still sees each one. Nothing else borrows either while the
    // kernel runs.
    let result = call.execute(
        &graph,
        limits,
        KernelMeter {
            execution: &mut execution.borrow_mut(),
            observe: &mut *observe.borrow_mut(),
        },
    );
    // Every unit the kernel spent is charged before its result or its own
    // error is judged, as it was when each was charged on the spot. A kernel
    // our control stopped keeps that refusal.
    let settled = execution.borrow_mut().settle();
    if !matches!(result, Err(FnxExecutionError::Cancelled(_))) {
        settled.map_err(control_error)?;
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
    let result = result.map_err(|error| match error {
        FnxExecutionError::Cancelled(error) => control_error(error),
        other => Error::Execution(other),
    })?;
    // The kernel's certificate is trusted-host provenance, not a capability
    // response. Releasing it could expose the unmasked root/history identity.
    Ok(result.rows)
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
    let mut edges = Vec::new();
    source::visit_edges_with_properties(snapshot, binding.as_of, &mut scan, |entry, props, _| {
        if !scope.allows_relation(entry.relation)
            || options
                .selection
                .relation
                .is_some_and(|relation| relation != entry.relation)
            || vertices.binary_search(&entry.src).is_err()
            || vertices.binary_search(&entry.dst).is_err()
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
    })?;
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

/// A Prism kernel's meter over the live execution: work units charged in runs
/// (fgdb-62mzm), each still observed by the caller meter in order.
struct KernelMeter<'m, 'cx, 'permit, Clock, Observe> {
    execution: &'m mut Execution<'cx, 'permit, Clock>,
    observe: &'m mut Observe,
}

impl<Clock, Observe> FnxMeter<QueryError> for KernelMeter<'_, '_, '_, Clock, Observe>
where
    Clock: FnMut() -> u64,
    Observe: FnMut(SourceEvent) -> Result<(), QueryError>,
{
    fn unit(&mut self) -> Result<(), QueryError> {
        self.units(1)
    }
    fn units(&mut self, n: usize) -> Result<(), QueryError> {
        self.execution.kernel_units(n as u64)?;
        for _ in 0..n {
            (self.observe)(SourceEvent::Work)?;
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "analytics_tests.rs"]
mod tests;
