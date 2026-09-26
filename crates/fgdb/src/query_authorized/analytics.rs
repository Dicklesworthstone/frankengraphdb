//! Prism consumes the capability's induced graph, never a post-filtered answer.
//!
//! Reuse the authenticated historical visitors, the owned projection builder,
//! and the registered native kernels. This path is explicitly DECODED_CACHE:
//! it does not confer authority on raw sealed images or publish a reusable view
//! that could outlive the single live Warden execution allowance.

use super::{Execution, authorized_with_errors};
use crate::gql_exec::source::{self, SourceEvent};
use crate::query::prism::{push_staged, source_admit};
use crate::{Database, QueryError, ReadError, Snapshot};
use asupersync::fs::Vfs;
use fgdb_prism::{
    Directedness, FnxCallSpec, FnxExecutionError, FnxGraphKind, FnxParameters, FnxReadError,
    FnxReadOptions, FnxValue, ParallelEdgePolicy, ProjectionBuildError, ProjectionEdge,
    ProjectionError, SelfLoopPolicy, SnapshotBinding, SnapshotGraphView,
};
use fgdb_types::{QueryCx, VId};
use fgdb_warden::{Authority, CapabilityToken, PlannerPredicates};
use std::cell::RefCell;

type Error = FnxReadError<ReadError, QueryError>;

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
    /// timing, error-detail or resource-failure noninterference, provide general
    /// GQL CALL composition, or authorize the other privileged database APIs.
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
    snapshot: &Snapshot,
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
    let mut work = 0u64;
    let mut scratch = 0u64;
    let mut control = |event| {
        execution.borrow_mut().checkpoint().map_err(control_error)?;
        observe.borrow_mut()(event).map_err(control_error)?;
        let (counter, limit, resource) = match event {
            SourceEvent::Work | SourceEvent::SnapshotRecord => (
                &mut work,
                options.source_limits.max_work_units,
                "work units",
            ),
            SourceEvent::ScratchEntry => (
                &mut scratch,
                options.source_limits.max_scratch_entries,
                "scratch entries",
            ),
        };
        *counter = counter.checked_add(1).ok_or(Error::SizeOverflow)?;
        source_admit(resource, u128::from(*counter), u128::from(limit))
    };
    // FG-INV-20: history the capability cannot see is walked through `scan`,
    // which polls cancellation and charges nothing. Only records that pass the
    // scope (and, for edges, whose endpoints were admitted) are charged, so no
    // signed or source limit's refusal point depends on hidden data.
    let mut scan = |_| execution.borrow_mut().poll().map_err(control_error);
    let mut vertices: Vec<VId> = Vec::new();
    let mut staged_bytes = 0;
    source::visit_vertices(&snapshot.patches, binding.as_of, &mut scan, |row, _| {
        if !scope.allows_vertex(&row.labels) {
            return Ok(());
        }
        control(SourceEvent::Work)?;
        for &label in &row.labels {
            if scope.allows_label(label) {
                control(SourceEvent::Work)?;
            }
        }
        execution.borrow_mut().node().map_err(control_error)?;
        observe.borrow_mut()(SourceEvent::SnapshotRecord).map_err(control_error)?;
        observe.borrow_mut()(SourceEvent::ScratchEntry).map_err(control_error)?;
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
        control(SourceEvent::Work)?;
        observe.borrow_mut()(SourceEvent::SnapshotRecord).map_err(control_error)?;
        observe.borrow_mut()(SourceEvent::ScratchEntry).map_err(control_error)?;
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
    let graph = SnapshotGraphView::build_owned_with_checkpoint(
        binding,
        vertices,
        edges,
        options.projection,
        options.projection_limits,
        checkpoint,
    )
    .map_err(|error| match error {
        ProjectionBuildError::Cancelled(error) => control_error(error),
        ProjectionBuildError::Projection(error) => Error::Projection(error),
    })?;
    let signed_rows = scope.limits().max_rows;
    let mut limits = options.execution_limits;
    limits.max_result_rows = limits
        .max_result_rows
        .min(usize::try_from(signed_rows).unwrap_or(usize::MAX));
    let result = call.execute(&graph, limits, checkpoint);
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
