//! Warden-scoped reads of the embedded database's authenticated generation.
//!
//! Only admitted vertices/edges reach GLA. User predicates see masked labels
//! and properties, independently of their Boolean shape. Historical winners
//! are selected BEFORE authorization; a hidden successor cannot revive an old
//! visible version. No alternate query evaluator or synthetic snapshot exists.
//!
//! This is a resident-source boundary. Existing authenticated history merges
//! may inspect mixed-scope blocks, and their work is still charged. It does not
//! establish descriptor-I/O, timing or resource-failure noninterference, nor
//! install authorization on the privileged, ordinary Database APIs. The host
//! must not expose those APIs, its Authority, or its clock to token holders.

use super::{Cancel, QueryError};
use crate::gql_exec::{
    AdmissionUsage,
    property_maps::{self, PropertyMaps},
    source::{self, SourceEvent},
};
use crate::{Database, GqlError, ReadError, Snapshot, VertexRow};
use asupersync::fs::Vfs;
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GlaOutput, GlaPlan, GraphValue, PreparedGraphPattern, VertexPredicate};
use fgdb_gql::{GovernedCheckpoint, GqlQueryError, GqlQueryExecution, GqlQueryPolicy};
use fgdb_types::{CanonicalScalar, CommitSeq, EId, QueryCx, VId};
use fgdb_warden::{Authority, CapabilityToken, ExecutionPermit, PlannerPredicates, ReadAccess};
use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::sync::Arc;

#[path = "query_authorized/admission.rs"]
mod admission;
#[path = "query_authorized/analytics.rs"]
mod analytics;
#[path = "query_authorized/beacon.rs"]
mod beacon;
#[path = "query_authorized/overlay.rs"]
mod overlay;
#[path = "query_authorized/positions.rs"]
pub(crate) mod positions;
#[path = "query_authorized/relational.rs"]
mod relational;
#[path = "query_authorized/tape.rs"]
mod tape;
pub use relational::{
    AuthorizedAggregateCursor, AuthorizedAsyncAggregateCursor, AuthorizedBeaconIndex,
    AuthorizedOrderExecution, AuthorizedOrderedReadSession, AuthorizedPreparedFnxCall,
    AuthorizedPreparedRead, AuthorizedReadSession, AuthorizedRowCursor,
    AuthorizedSendAggregateCursor, AuthorizedSendRowCursor, AuthorizedSortLimits,
    AuthorizedSpoolCursor,
};

use admission::{ADMITTED, AdmissionKey, AdmittedCore, CoreEdge, Recording};
use tape::Step;

#[cfg(test)]
thread_local! {
    /// Full-scan admissions on this thread: the tests' physical reuse witness.
    static SCANS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

type Fault = GqlQueryError<ReadError, QueryError>;
type Governed<T> = Result<GqlQueryExecution<T>, Fault>;
type Edge<'a> = (
    (EId, VId, RelationId, VId),
    &'a [(PropertyKeyId, CanonicalScalar)],
);

// The sole live permit spans source admission, evaluation and final delivery.
// RefCell permits sequential callbacks to share it, never concurrent borrowing
// or independent per-operand allowance resets. Neither it nor its data escapes.
struct Execution<'cx, 'permit, Clock> {
    cx: &'cx QueryCx,
    permit: ExecutionPermit<'permit, ReadAccess>,
    clock: Clock,
    // Pull cursors borrow the actual clock type so a Send clock can remain
    // Send across suspension. The session still remembers every sampled time.
    clock_floor: Option<&'cx mut u64>,
    // Warden stops a permit after its first failed charge. Retain that exact
    // cause across outer completion checks instead of losing expiry/retirement
    // to ExecutionStopped and accidentally restoring a terminal session.
    failure: Option<fgdb_warden::Error>,
    // The pull root most recently admitted and charged its node. Its own
    // record, field and incidence reads do not charge that node again.
    pull_root: Option<VId>,
    // The newest host clock sample and how many more charges may share it
    // (QueryCx::host_clock_interval, fgdb-er71y). None until the first charge.
    sampled: Option<u64>,
    until_sample: u32,
    // Signed work units spent but not yet charged, and how many more the
    // current run may defer before the next charge (`work_units`).
    deferred: u64,
    run_left: u64,
    // Inside a `work_runs` phase: only there may work be deferred.
    in_runs: bool,
}

/// Signed work units charged in one Warden call while the allowance covers
/// them (fgdb-62mzm option 1).
const WORK_RUN: u64 = 64;

#[cfg(test)]
thread_local! {
    /// Off: every work unit is charged alone, the per-unit oracle.
    static WORK_RUNS: std::cell::Cell<bool> = const { std::cell::Cell::new(true) };
}

impl<'cx, 'permit, Clock: FnMut() -> u64> Execution<'cx, 'permit, Clock> {
    fn new(cx: &'cx QueryCx, permit: ExecutionPermit<'permit, ReadAccess>, clock: Clock) -> Self {
        Self {
            cx,
            permit,
            clock,
            clock_floor: None,
            failure: None,
            pull_root: None,
            sampled: None,
            until_sample: 0,
            deferred: 0,
            run_left: 0,
            in_runs: false,
        }
    }
    fn tracking(
        cx: &'cx QueryCx,
        permit: ExecutionPermit<'permit, ReadAccess>,
        clock: Clock,
        floor: &'cx mut u64,
    ) -> Self {
        Self {
            clock_floor: Some(floor),
            ..Self::new(cx, permit, clock)
        }
    }
    /// The time a charge is judged at: the newest sample while the host's
    /// interval allows it to be shared, else a fresh one.
    fn now(&mut self) -> u64 {
        match self.sampled {
            Some(now) if self.until_sample > 0 => {
                self.until_sample -= 1;
                now
            }
            _ => self.sample(),
        }
    }
    /// A fresh host clock sample, which the next interval's charges share.
    fn sample(&mut self) -> u64 {
        let now = (self.clock)();
        if let Some(floor) = &mut self.clock_floor {
            **floor = (**floor).max(now);
        }
        self.sampled = Some(now);
        self.until_sample = self.cx.host_clock_interval().get() - 1;
        now
    }
    fn refusal(&mut self, error: fgdb_warden::Error) -> QueryError {
        QueryError::Authorization(*self.failure.get_or_insert(error))
    }
    /// One unit of signed work, charged in runs (`work_units`).
    fn checkpoint(&mut self) -> Result<(), QueryError> {
        self.work_units(1)
    }
    /// Work units the signed allowance still admits.
    fn remaining_work(&self) -> u64 {
        let limit = self.permit.predicates().limits().max_work;
        limit.saturating_sub(self.permit.usage().work)
    }
    /// `n` units of signed work, metered like `n` checkpoints but charged in
    /// runs (fgdb-62mzm option 1). A run starts only while the allowance
    /// covers two whole runs, so its up to WORK_RUN units are deferred and
    /// then charged in one Warden call with one cancellation poll. Below that,
    /// every unit is charged alone. Every other charge, reservation and
    /// delivery settles the run first, so charges reach the permit in their
    /// per-unit order. The unit that exhausts any allowance therefore refuses
    /// at its own ordinal, and the totals are the per-unit totals.
    /// Cancellation, retirement and expiry are observed at least once per
    /// run instead of once per unit.
    fn work_units(&mut self, n: u64) -> Result<(), QueryError> {
        if n <= self.run_left {
            self.run_left -= n;
            self.deferred += n;
            return Ok(());
        }
        self.next_run(n)
    }
    /// Exactly `n` calls of `work_units(1)`, an open run's worth at a time:
    /// a unit the open run admits is only counted, as `work_units` counts it,
    /// and every other unit is that call itself. `done` counts the units it
    /// completed, so a caller knows where a refusal stopped it.
    fn units_one_by_one(&mut self, n: u64, done: &mut u64) -> Result<(), QueryError> {
        let mut left = n;
        while left > 0 {
            if self.run_left > 0 {
                let take = left.min(self.run_left);
                self.run_left -= take;
                self.deferred += take;
                left -= take;
                *done += take;
            } else {
                self.work_units(1)?;
                left -= 1;
                *done += 1;
            }
        }
        Ok(())
    }
    #[inline(never)]
    fn next_run(&mut self, n: u64) -> Result<(), QueryError> {
        self.settle()?;
        #[cfg(test)]
        let runs = WORK_RUNS.with(std::cell::Cell::get);
        #[cfg(not(test))]
        let runs = true;
        if runs
            && self.in_runs
            && n <= WORK_RUN
            && self.failure.is_none()
            && self.remaining_work() >= 2 * WORK_RUN
        {
            self.deferred = n;
            self.run_left = WORK_RUN - n;
            return Ok(());
        }
        // A larger run (an evaluator's) is one Warden call while the
        // allowance covers it twice over, as a deferred run must: no unit of
        // it can refuse. Below that every unit is still charged alone.
        if runs
            && self.in_runs
            && n > WORK_RUN
            && self.failure.is_none()
            && self.remaining_work() >= 2 * n
        {
            return self.charge_work(n);
        }
        for _ in 0..n {
            self.charge_work(1)?;
        }
        Ok(())
    }
    /// Drop the deferred units of a stopped phase uncharged.
    fn discard_runs(&mut self) {
        self.deferred = 0;
        self.run_left = 0;
    }
    /// Charge the deferred units: in one call when the allowance covers them
    /// all, else one at a time so a refusal keeps its ordinal.
    fn settle(&mut self) -> Result<(), QueryError> {
        self.run_left = 0;
        let deferred = std::mem::take(&mut self.deferred);
        if deferred == 0 {
            return Ok(());
        }
        if self.remaining_work() >= deferred {
            return self.charge_work(deferred);
        }
        for _ in 0..deferred {
            self.charge_work(1)?;
        }
        Ok(())
    }
    // Reserve a bounded private physical phase on this same signed permit,
    // after any deferred run. Reservations are never refunded after failure,
    // cancellation or drop.
    fn reserve_work(&mut self, units: u64) -> Result<(), QueryError> {
        self.settle()?;
        self.charge_work(units)
    }
    fn charge_work(&mut self, units: u64) -> Result<(), QueryError> {
        self.poll()?;
        let now = self.now();
        let charged = self.permit.charge_work_at(now, units);
        charged.map_err(|error| self.refusal(error))
    }
    /// Cancellation and a retained refusal only; no signed or native charge.
    /// Scanning history the capability cannot see goes through here, so what
    /// hidden records cost is never observable through a resource limit.
    fn poll(&mut self) -> Result<(), QueryError> {
        if let Some(error) = self.failure {
            return Err(QueryError::Authorization(error));
        }
        self.cx.checkpoint().map_err(interrupted)
    }
    fn node(&mut self) -> Result<(), QueryError> {
        self.checkpoint()?;
        self.settle()?;
        let now = self.now();
        let charged = self.permit.charge_nodes_at(now, 1);
        charged.map_err(|error| self.refusal(error))
    }
    /// A pull root costs one node when admitted, whether or not anything
    /// reads its record, as an eager table admits each vertex once. An
    /// identity-only RETURN or count(*) therefore cannot bypass max_nodes.
    fn root_node(&mut self, vid: VId) -> Result<(), QueryError> {
        self.node()?;
        self.pull_root = Some(vid);
        Ok(())
    }
    /// An admitted vertex read. The current pull root was charged on
    /// admission; every other vertex is charged here.
    fn vertex_node(&mut self, vid: VId) -> Result<(), QueryError> {
        if self.pull_root == Some(vid) {
            return Ok(());
        }
        self.node()
    }
    fn deliver(&mut self, rows: usize) -> Result<(), QueryError> {
        self.checkpoint()?;
        self.settle()?;
        let rows = u64::try_from(rows).map_err(|_| self.refusal(fgdb_warden::Error::TooLarge))?;
        // Delivery is never judged on a shared sample: nothing is released
        // after expiry, however the host paces its clock.
        let now = self.sample();
        let charged = self.permit.charge_rows_at(now, rows);
        charged.map_err(|error| self.refusal(error))?;
        self.checkpoint()?;
        self.settle()
    }
}
/// Run `phase` with its signed work charged in runs (`Execution::work_units`),
/// then settle. The permit therefore holds the per-unit totals again whenever
/// control leaves the phase. A phase is one engine call over admitted data:
/// admission, evaluation, a Prism projection or kernel. Session callbacks,
/// clock boundaries and deliveries stay outside phases and keep per-unit
/// observation; a delivery or node charge inside one settles first.
fn work_runs<Clock: FnMut() -> u64, T, E>(
    execution: &RefCell<Execution<'_, '_, Clock>>,
    settle_error: impl FnOnce(QueryError) -> E,
    phase: impl FnOnce() -> Result<T, E>,
) -> Result<T, E> {
    let outer = std::mem::replace(&mut execution.borrow_mut().in_runs, true);
    let result = phase();
    let mut live = execution.borrow_mut();
    live.in_runs = outer;
    match result {
        Ok(value) => {
            live.settle().map_err(settle_error)?;
            Ok(value)
        }
        // A stopped phase stops as a per-unit one does, with no further
        // poll or charge: its permit ends with the statement, so work it
        // spent but had not yet charged is observable nowhere.
        Err(error) => {
            live.discard_runs();
            Err(error)
        }
    }
}

fn interrupted(error: Cancel) -> QueryError {
    QueryError::Pattern(GqlQueryError::Interrupted(error))
}

// Keep original source/budget/cancellation classes. The private interruption
// carrier also transports a live Warden refusal without flattening its cause.
fn query_error(error: GqlQueryError<ReadError, QueryError>) -> QueryError {
    match error {
        GqlQueryError::Interrupted(error) => error,
        GqlQueryError::Source(error) => {
            QueryError::Pattern(GqlQueryError::Source(GqlError::Read(error)))
        }
        GqlQueryError::Rows(error) => QueryError::Pattern(GqlQueryError::Rows(error)),
        GqlQueryError::Evaluator(error) => QueryError::Pattern(GqlQueryError::Evaluator(error)),
        GqlQueryError::IdentifiedEdgesRequired => {
            QueryError::Pattern(GqlQueryError::IdentifiedEdgesRequired)
        }
        GqlQueryError::Data(error) => QueryError::Pattern(GqlQueryError::Data(error)),
    }
}

#[allow(clippy::too_many_arguments)]
fn authorized<V: Vfs + Clone, Row, Clock: FnMut() -> u64>(
    database: &Database<V>,
    cx: &QueryCx,
    authority: &Authority,
    token: &CapabilityToken,
    branch: &str,
    as_of: Option<CommitSeq>,
    clock: Clock,
    evaluate: impl FnOnce(
        &Arc<Snapshot>,
        CommitSeq,
        &PlannerPredicates,
        &RefCell<Execution<'_, '_, Clock>>,
    ) -> Result<Vec<Row>, QueryError>,
) -> Result<Vec<Row>, QueryError> {
    authorized_with_errors(
        database,
        cx,
        authority,
        token,
        branch,
        as_of,
        clock,
        core::convert::identity,
        evaluate,
    )
}

// All governed consumers share authentication, one live allowance and final
// delivery. Only their typed error carrier differs; no source/permit escapes.
#[allow(clippy::too_many_arguments)]
fn authorized_with_errors<V: Vfs + Clone, Row, Clock: FnMut() -> u64, Error>(
    database: &Database<V>,
    cx: &QueryCx,
    authority: &Authority,
    token: &CapabilityToken,
    branch: &str,
    as_of: Option<CommitSeq>,
    mut clock: Clock,
    map_error: fn(QueryError) -> Error,
    evaluate: impl FnOnce(
        &Arc<Snapshot>,
        CommitSeq,
        &PlannerPredicates,
        &RefCell<Execution<'_, '_, Clock>>,
    ) -> Result<Vec<Row>, Error>,
) -> Result<Vec<Row>, Error> {
    // The host picks its issuer, not the request. Reject a different database
    // namespace before reading a frontier, source, or catalog. Graph/catalog/
    // branch-name routing remains the host's trusted registry responsibility.
    if authority.namespace() != database.keys.namespace {
        return Err(map_error(QueryError::Authorization(
            fgdb_warden::Error::WrongAuthority,
        )));
    }
    let now = clock();
    let verified = authority
        .verify_at(token, branch, now)
        .map_err(QueryError::Authorization)
        .map_err(map_error)?;
    let permit = verified
        .begin_read_at(branch, now)
        .map_err(QueryError::Authorization)
        .map_err(map_error)?;
    let execution = RefCell::new(Execution::new(cx, permit, clock));
    execution.borrow_mut().checkpoint().map_err(map_error)?;
    database
        .ensure_readable()
        .map_err(QueryError::Read)
        .map_err(map_error)?;
    let at = as_of.unwrap_or(database.snapshot.frontier);
    database
        .snapshot
        .check_frontier(at)
        .map_err(QueryError::Read)
        .map_err(map_error)?;
    cx.with_restriction(|| {
        let rows = evaluate(&database.snapshot, at, verified.predicates(), &execution)?;
        // No result prefix, source rows or private statistics were released.
        // Empty outputs still recheck expiry, retirement and cancellation.
        execution
            .borrow_mut()
            .deliver(rows.len())
            .map_err(map_error)?;
        Ok(rows)
    })
}

/// Vertex tables at least this long index their sealed rows for the edge
/// phase's endpoint lookups; smaller ones search.
const POSITIONED_VERTICES: usize = 4096;

// Admitted records in visitor order, sorted by identity once sealed. Full-scan
// visitors already yield ascending VIds and EIds, so sealing is one pass; a
// sorted vector replaces per-record ordered-map inserts and lookups (fgdb-61p1n).
struct Tables<'a> {
    vertices: Vec<(VId, &'a VertexRow)>,
    /// Whether `vertices` is strictly ascending, so endpoint checks can search it.
    vertices_sealed: bool,
    /// Identity to position in the sealed `vertices`, shared through the
    /// cached core this table was refilled from; absent, lookups search.
    positions: Option<Arc<positions::VertexPositions>>,
    edges: Vec<Edge<'a>>,
    /// A refilled core whose sealed inline identities stand in for `edges`
    /// when only their (source, relation, destination) triples are read.
    edge_core: Option<Arc<AdmittedCore>>,
    labels: BTreeMap<VId, Vec<GraphValue>>,
    types: BTreeMap<EId, CanonicalScalar>,
    property_maps: PropertyMaps,
    records: u64,
}
impl<'a> Tables<'a> {
    /// Only ADMITTED records are charged (FG-INV-20). The history visitors walk
    /// mixed-scope blocks and patches through `scan`, which polls cancellation
    /// and charges nothing; each admitted vertex then costs one unit of work
    /// plus one per label the capability may see, and each admitted edge one.
    /// Every signed and native charge is therefore a function of the visible
    /// graph alone, so a holder who attenuates MaxWork, MaxNodes or MaxRows
    /// cannot count hidden records by finding where a query starts to refuse.
    /// Scanning hidden history still takes time: that timing channel is not
    /// closed here, and the host bounds it with the query's own Cx budget.
    ///
    /// `poll` is that charge-free scan call. Under a scope that hides nothing,
    /// a full scan's records and live calls depend only on the snapshot and
    /// the cut, so the first one on a snapshot is recorded and later ones
    /// replay its calls and refill from it (`admission.rs`, fgdb-61p1n).
    // `triples_only` is the caller's read mode beside the cut: a struct for
    // one flag would only rename it.
    #[allow(clippy::too_many_arguments)]
    fn admit<Row: GlaOutput>(
        snapshot: &'a Arc<Snapshot>,
        plan: &GlaPlan<Row>,
        at: CommitSeq,
        triples_only: bool,
        predicates: &PlannerPredicates,
        mut node: impl FnMut() -> Result<(), Fault>,
        poll: &mut impl FnMut() -> Result<(), Fault>,
        control: &mut impl FnMut(SourceEvent) -> Result<(), Fault>,
    ) -> Result<Self, Fault> {
        if let Some(tables) = Self::admit_bound(
            snapshot,
            plan,
            at,
            predicates,
            &mut node,
            &mut |_| poll(),
            control,
        )? {
            return Ok(tables);
        }
        let reusable = predicates.sees_all_incidence() && predicates.sees_all_fields();
        let key = AdmissionKey {
            at,
            edges: plan.reads_edges(),
        };
        let mut tables = Self::new();
        let mut reused = None;
        if let Some(core) = reusable.then(|| ADMITTED.lookup(snapshot, key)).flatten() {
            // Spend first: every limit refuses before the tables are refilled.
            core.tape.replay(&mut *poll, |step| match step {
                Step::Vertex(labels) => charge_vertex(labels as usize, &mut node, control),
                Step::Edge => charge_edge(control),
                Step::Checkpoint => Ok(()),
            })?;
            // A reader of triples alone takes a core's positioned edges or
            // sealed inline identities in place instead of copying every edge.
            let in_place = triples_only
                && (core.positioned.is_some() || core.identities.is_some())
                && !plan.projects_types()
                && !plan.projects_edge_property_maps();
            tables.refill(snapshot, &core, !in_place);
            if in_place {
                tables.edge_core = Some(Arc::clone(&core));
            }
            reused = Some(core);
        } else {
            #[cfg(test)]
            SCANS.with(|scans| scans.set(scans.get() + 1));
            let recording = reusable.then(|| RefCell::new(Recording::default()));
            let mut scan = |_| {
                if let Some(recording) = &recording {
                    recording.borrow_mut().poll();
                }
                poll()
            };
            source::visit_vertex_coordinates(
                &snapshot.patches,
                at,
                &mut scan,
                |row, patch, row_at, _| {
                    let admitted = tables.admit_vertex(row, predicates, &mut node, control)?;
                    if let (Some(labels), Some(recording)) = (admitted, &recording) {
                        recording
                            .borrow_mut()
                            .vertex(row.vid, patch, row_at, labels);
                    }
                    Ok(())
                },
            )?;
            if plan.reads_edges() {
                source::visit_edges_with_coordinates(
                    snapshot,
                    at,
                    &mut scan,
                    |edge, properties, block, row_at, _| {
                        let identity = (edge.eid, edge.src, edge.relation, edge.dst);
                        if let Some(positions) =
                            tables.admit_edge((identity, properties), predicates, control)?
                            && let Some(recording) = &recording
                        {
                            recording
                                .borrow_mut()
                                .edge(identity, block, row_at, positions);
                        }
                        Ok(())
                    },
                )?;
            }
            if let Some(core) =
                recording.and_then(|recording| recording.into_inner().finish(tables.records))
            {
                let bytes = core.bytes();
                ADMITTED.remember(snapshot, key, Arc::new(core), bytes);
            }
        }
        tables.seal();
        if let Some(core) = reused {
            // Every statement refilling this core seals the same table, so
            // the first one's index serves them all.
            tables.positions = core
                .positions
                .get_or_init(|| positions::VertexPositions::build(&tables.vertices).map(Arc::new))
                .clone();
        }
        tables.metadata(plan, predicates, control)?;
        Ok(tables)
    }

    /// Fill empty tables from a core recorded on this same snapshot allocation.
    /// Each edge's identity is read back from the statement its coordinates
    /// name, which is the one the recording scan admitted.
    fn refill(&mut self, snapshot: &'a Snapshot, core: &AdmittedCore, edges: bool) {
        self.vertices.extend(
            core.vertices
                .iter()
                .map(|&(vid, patch, row)| (vid, &snapshot.patches[patch as usize][row as usize])),
        );
        self.records = core.records;
        if edges {
            self.refill_edges(snapshot, core);
        }
    }

    /// Copy the core's edges into `edges`, as a refill that reads them does.
    fn refill_edges(&mut self, snapshot: &'a Snapshot, core: &AdmittedCore) {
        let properties = |block: u32, row: u32| {
            source::edge_properties_at(&snapshot.block_props, block as usize, row as usize)
        };
        match &core.identities {
            Some(identities) => {
                self.edges.extend(identities.iter().zip(&core.edges).map(
                    |(&identity, &CoreEdge { block, row })| (identity, properties(block, row)),
                ))
            }
            None => self
                .edges
                .extend(core.edges.iter().map(|&CoreEdge { block, row }| {
                    let edge = &snapshot.blocks[block as usize][row as usize];
                    (
                        (edge.eid, edge.src, edge.relation, edge.dst),
                        properties(block, row),
                    )
                })),
        }
    }

    /// The edge table for a reader that cannot take the refilled core's
    /// positioned edges after all (they did not validate against this
    /// table, which every refill seals alike, so this is a guard only):
    /// without inline identities to read in place, copy the edges.
    fn settle_unpositioned(&mut self, snapshot: &'a Snapshot) {
        if let Some(core) = self.edge_core.take_if(|core| core.identities.is_none()) {
            self.refill_edges(snapshot, &core);
            seal_by_key(&mut self.edges, |&((eid, ..), _)| eid);
        }
    }

    /// The owner source's index-bound selection, for a capability that hides
    /// nothing. Under such a scope every label, relation and property is
    /// visible, so the unmasked predicates that pick candidates are exactly the
    /// masked ones the algebra evaluates, and each admitted record's charge is
    /// a function of the visible graph. Selection itself runs on `scan`
    /// (cancellation only) and admission charges what it keeps, as the full
    /// scan does. A scoped capability, an unbound or unseekable root, a plan
    /// whose steps the per-slot closure does not model, or a nested vertex
    /// scan returns `None`, leaving the complete admission below.
    fn admit_bound<Row: GlaOutput>(
        snapshot: &'a Snapshot,
        plan: &GlaPlan<Row>,
        at: CommitSeq,
        predicates: &PlannerPredicates,
        node: &mut impl FnMut() -> Result<(), Fault>,
        scan: &mut impl FnMut(SourceEvent) -> Result<(), Fault>,
        control: &mut impl FnMut(SourceEvent) -> Result<(), Fault>,
    ) -> Result<Option<Self>, Fault> {
        if !predicates.sees_all_incidence() || !predicates.sees_all_fields() {
            return Ok(None);
        }
        let mut tables = Self::new();
        if !plan.scans_edges() && plan.reads_edges() {
            // A bound vertex root followed by flat steps (a quantified reach)
            // admits its per-slot closure: the rows its slots can bind, which
            // contain both endpoints of every closure edge, then those edges.
            let Some((edges, reached)) =
                source::bound_vertex_closure(snapshot, plan, at, true, scan)?
            else {
                return Ok(None);
            };
            let complete =
                tables.admit_found(snapshot, &reached, at, predicates, node, scan, control)?;
            for edge in edges {
                tables.admit_closure_edge(edge, predicates, complete, control)?;
            }
        } else if !plan.scans_edges() {
            let Some((rows, _)) = source::bound_vertices(snapshot, plan, at, scan)? else {
                return Ok(None);
            };
            for row in rows {
                tables.admit_vertex(row, predicates, node, control)?;
            }
        } else {
            let Some(edges) = source::bound_edges(snapshot, plan, at, true, scan)? else {
                return Ok(None);
            };
            // The owner source hydrates only the endpoint rows a plan reads;
            // an authorized edge also needs both endpoints admitted. Each
            // distinct endpoint is found and admitted once, in identity order:
            // the same vertices and charges as admitting them in edge order,
            // with no ordered-set probe and insert per endpoint (a third of a
            // pokec hub two-hop) and the table already sealed (fgdb-61p1n).
            let mut endpoints: Vec<VId> = edges
                .iter()
                .flat_map(|&((_, src, _, dst), _)| [src, dst])
                .collect();
            fgdb_gql::id_sort::sort_by_leading_identity(
                &mut endpoints,
                |vid| vid.0,
                <[_]>::sort_unstable,
            );
            endpoints.dedup();
            let complete =
                tables.admit_found(snapshot, &endpoints, at, predicates, node, scan, control)?;
            for edge in edges {
                tables.admit_closure_edge(edge, predicates, complete, control)?;
            }
        }
        tables.seal();
        tables.metadata(plan, predicates, control)?;
        Ok(Some(tables))
    }

    fn new() -> Self {
        Self {
            vertices: Vec::new(),
            vertices_sealed: true,
            positions: None,
            edges: Vec::new(),
            edge_core: None,
            labels: BTreeMap::new(),
            types: BTreeMap::new(),
            property_maps: PropertyMaps::default(),
            records: 0,
        }
    }

    /// The refilled core's sealed inline identities, in identity order, when
    /// this table took them in place of `edges`.
    fn inline_triples(&self) -> Option<&[admission::EdgeIdentity]> {
        self.edge_core
            .as_ref()
            .and_then(|core| core.identities.as_deref())
    }

    /// The refilled core's edges by position in this sealed vertex table,
    /// when this table took them in place of `edges`. The core's positions
    /// name the table its recording scan sealed, which is this one: every
    /// refill seals the same admitted vertices in the same order.
    fn positioned_graph(&self) -> Option<fgdb_gql::PositionedGraph<'_, &'a VertexRow>> {
        let positioned = self.edge_core.as_ref()?.positioned.as_ref()?;
        fgdb_gql::PositionedGraph::new(&self.vertices, &positioned.relations, &positioned.edges)
    }

    /// Sort both tables by identity. A repeated identity keeps its last
    /// admission, as the ordered-map insert this replaces did. Charges nothing:
    /// every record was charged when it was admitted.
    fn seal(&mut self) {
        seal_by_key(&mut self.vertices, |&(vid, _)| vid);
        self.vertices_sealed = true;
        seal_by_key(&mut self.edges, |&((eid, ..), _)| eid);
    }

    /// The admitted vertex identities in table order. Each one yielded is
    /// kept in `last`, so testing or reading the vertex a scan has just
    /// yielded takes its row instead of finding it again (`vertex_after`).
    fn scan<'s>(
        &'s self,
        last: &'s Cell<Option<(VId, &'a VertexRow)>>,
    ) -> impl Iterator<Item = VId> + 's {
        self.vertices.iter().map(move |&(vid, row)| {
            last.set(Some((vid, row)));
            vid
        })
    }

    /// `vertex`, answered by the row `scan` last yielded when that row is
    /// `vid`'s. A sealed table holds each identity once, so it is the row
    /// `vertex` would find.
    fn vertex_after(
        &self,
        last: &Cell<Option<(VId, &'a VertexRow)>>,
        vid: VId,
    ) -> Option<&'a VertexRow> {
        match last.get() {
            Some((at, row)) if at == vid && self.vertices_sealed => Some(row),
            _ => self.vertex(vid),
        }
    }

    fn vertex(&self, vid: VId) -> Option<&'a VertexRow> {
        self.position(vid).map(|at| self.vertices[at].1)
    }

    /// Where `vid` is in the sealed vertex table.
    fn position(&self, vid: VId) -> Option<usize> {
        match &self.positions {
            Some(positions) => positions.find(&self.vertices, vid),
            None => self
                .vertices
                .binary_search_by_key(&vid, |&(vid, _)| vid)
                .ok(),
        }
    }

    fn edge(&self, eid: EId) -> Option<&Edge<'a>> {
        self.edges
            .binary_search_by_key(&eid, |&((eid, ..), _)| eid)
            .ok()
            .map(|at| &self.edges[at])
    }

    // Snapshot visitors and canonical transaction rows use identical masking
    // and logical admission charges. No input source can bypass this boundary.
    /// Returns the number of labels charged when the vertex is admitted.
    /// Find and admit a bound admission's vertices in order; whether every one
    /// was found and admitted. `scan` only polls: a vertex's history is walked
    /// uncharged (FG-INV-20), so each lookup observes cancellation once, not
    /// once per node its descent visits, whose number followed the shape of a
    /// history index that includes versions the capability cannot see. The
    /// lookups are pure reads, made a chunk at a time before the chunk is
    /// admitted: independent lookups whose cache misses overlap, where one at
    /// a time each waited on the last (most of a pokec hub_degree,
    /// fgdb-61p1n). Every poll and charge keeps its per-vertex order.
    #[allow(clippy::too_many_arguments)]
    fn admit_found(
        &mut self,
        snapshot: &'a Snapshot,
        vids: &[VId],
        at: CommitSeq,
        predicates: &PlannerPredicates,
        node: &mut impl FnMut() -> Result<(), Fault>,
        scan: &mut impl FnMut(SourceEvent) -> Result<(), Fault>,
        control: &mut impl FnMut(SourceEvent) -> Result<(), Fault>,
    ) -> Result<bool, Fault> {
        const CHUNK: usize = 16;
        let mut complete = true;
        for chunk in vids.chunks(CHUNK) {
            let mut rows = [None; CHUNK];
            for (slot, &vid) in rows.iter_mut().zip(chunk) {
                *slot = source::find_vertex(snapshot, vid, at, &mut |_| Ok::<(), Fault>(()))?;
            }
            for row in &rows[..chunk.len()] {
                scan(SourceEvent::Work)?;
                complete &= match row {
                    Some(row) => self.admit_vertex(row, predicates, node, control)?.is_some(),
                    None => false,
                };
            }
        }
        Ok(complete)
    }

    fn admit_vertex(
        &mut self,
        row: &'a VertexRow,
        predicates: &PlannerPredicates,
        node: &mut impl FnMut() -> Result<(), Fault>,
        control: &mut impl FnMut(SourceEvent) -> Result<(), Fault>,
    ) -> Result<Option<usize>, Fault> {
        if !predicates.allows_vertex(&row.labels) {
            return Ok(None);
        }
        let labels = row
            .labels
            .iter()
            .filter(|&&label| predicates.allows_label(label))
            .count();
        charge_vertex(labels, node, control)?;
        self.records += 1;
        self.vertices_sealed &= self.vertices.last().is_none_or(|&(last, _)| last < row.vid);
        self.vertices.push((row.vid, row));
        // An index over the table before this vertex no longer describes it.
        self.positions = None;
        Ok(Some(labels))
    }

    /// Endpoint admission is checked against the sealed vertex table; an edge
    /// phase seals it first (a no-op pass when the visitor already sorted it).
    /// Returns the positions of an admitted edge's endpoints in that table.
    fn admit_edge(
        &mut self,
        edge: Edge<'a>,
        predicates: &PlannerPredicates,
        control: &mut impl FnMut(SourceEvent) -> Result<(), Fault>,
    ) -> Result<Option<[usize; 2]>, Fault> {
        let ((_, src, relation, dst), _) = edge;
        self.prepare_edges();
        if !predicates.allows_relation(relation) {
            return Ok(None);
        }
        let (Some(source), Some(destination)) = (self.position(src), self.position(dst)) else {
            return Ok(None);
        };
        charge_edge(control)?;
        self.records += 1;
        self.edges.push(edge);
        Ok(Some([source, destination]))
    }

    /// Seal the vertex table before an edge phase (a no-op pass when the
    /// visitor already sorted it). Each admitted edge's endpoints are looked
    /// up in it, and in a large table a binary search is a chain of cache
    /// misses (30% of a web-google edge count), so the first edge indexes the
    /// sealed table once.
    fn prepare_edges(&mut self) {
        if !self.vertices_sealed {
            seal_by_key(&mut self.vertices, |&(vid, _)| vid);
            self.vertices_sealed = true;
        }
        if self.positions.is_none() && self.vertices.len() >= POSITIONED_VERTICES {
            self.positions = positions::VertexPositions::build(&self.vertices).map(Arc::new);
        }
    }

    /// `admit_edge` for an edge of a bound closure, whose endpoint positions
    /// no caller reads. When the caller admitted every vertex the closure
    /// holds, and so both ends of each of its edges, the two position lookups
    /// would only confirm them: 17% of a pokec three-hop reach (fgdb-61p1n).
    /// Debug builds still check them.
    fn admit_closure_edge(
        &mut self,
        edge: Edge<'a>,
        predicates: &PlannerPredicates,
        endpoints_admitted: bool,
        control: &mut impl FnMut(SourceEvent) -> Result<(), Fault>,
    ) -> Result<(), Fault> {
        if !endpoints_admitted {
            return self.admit_edge(edge, predicates, control).map(drop);
        }
        let ((_, src, relation, dst), _) = edge;
        self.prepare_edges();
        debug_assert!(
            self.position(src).is_some() && self.position(dst).is_some(),
            "a closure edge whose endpoint was not admitted"
        );
        if !predicates.allows_relation(relation) {
            return Ok(());
        }
        charge_edge(control)?;
        self.records += 1;
        self.edges.push(edge);
        Ok(())
    }

    fn metadata<Row: GlaOutput>(
        &mut self,
        plan: &GlaPlan<Row>,
        predicates: &PlannerPredicates,
        control: &mut impl FnMut(SourceEvent) -> Result<(), Fault>,
    ) -> Result<(), Fault> {
        // Resolve names only after both topology and metadata scopes apply.
        // A forbidden unmapped label/type cannot cause a data-dependent error.
        if plan.projects_labels() {
            for &(_, row) in &self.vertices {
                control(SourceEvent::Work)?;
                let mut labels = Vec::new();
                for &label in &row.labels {
                    // Masked labels are skipped before any charge: their count
                    // on a visible vertex is hidden data too.
                    if !predicates.allows_label(label) {
                        continue;
                    }
                    control(SourceEvent::Work)?;
                    let name = plan
                        .reverse_catalog
                        .as_deref()
                        .and_then(|catalog| catalog.labels.get(&label))
                        .ok_or_else(|| GqlQueryError::Source(ReadError::UnmappedLabel(label)))?;
                    let name = text(name, control).map_err(|error| match error {
                        Some(error) => error,
                        None => GqlQueryError::Source(ReadError::UnmappedLabel(label)),
                    })?;
                    labels.push(GraphValue::Scalar(name));
                }
                control(SourceEvent::ScratchEntry)?;
                self.labels.insert(row.vid, labels);
            }
        }
        if plan.projects_types() {
            for &((eid, _, relation, _), _) in &self.edges {
                control(SourceEvent::Work)?;
                let name = plan
                    .reverse_catalog
                    .as_deref()
                    .and_then(|catalog| catalog.relations.get(&relation))
                    .ok_or_else(|| GqlQueryError::Source(ReadError::UnmappedRelation(relation)))?;
                let name = text(name, control).map_err(|error| match error {
                    Some(error) => error,
                    None => GqlQueryError::Source(ReadError::UnmappedRelation(relation)),
                })?;
                control(SourceEvent::ScratchEntry)?;
                self.types.insert(eid, name);
            }
        }
        if plan.projects_vertex_property_maps() {
            for &(_, row) in &self.vertices {
                let map = property_maps::collect(
                    row.props.iter().map(|(key, value)| (*key, value)),
                    plan.reverse_catalog.as_deref(),
                    |key| predicates.allows_property(key),
                    control,
                    &GqlQueryError::Source,
                )?;
                control(SourceEvent::ScratchEntry)?;
                self.property_maps.vertices.insert(row.vid, map);
            }
        }
        if plan.projects_edge_property_maps() {
            for &((eid, ..), properties) in &self.edges {
                let map = property_maps::collect(
                    properties.iter().map(|(key, value)| (*key, value)),
                    plan.reverse_catalog.as_deref(),
                    |key| predicates.allows_property(key),
                    control,
                    &GqlQueryError::Source,
                )?;
                control(SourceEvent::ScratchEntry)?;
                self.property_maps.edges.insert(eid, map);
            }
        }
        Ok(())
    }

    fn matches(&self, vid: VId, required: &[VertexPredicate], scope: &PlannerPredicates) -> bool {
        Self::row_matches(self.vertex(vid), required, scope)
    }
    /// `matches` for the vertex at `at` of the sealed table.
    fn matches_at(&self, at: u32, required: &[VertexPredicate], scope: &PlannerPredicates) -> bool {
        Self::row_matches(
            self.vertices.get(at as usize).map(|&(_, row)| row),
            required,
            scope,
        )
    }
    /// `matches` through `vertex_after`, for a vertex-only scan.
    #[inline(never)]
    fn matches_after(
        &self,
        last: &Cell<Option<(VId, &'a VertexRow)>>,
        vid: VId,
        required: &[VertexPredicate],
        scope: &PlannerPredicates,
    ) -> bool {
        Self::row_matches(self.vertex_after(last, vid), required, scope)
    }
    fn property(
        &self,
        vid: VId,
        key: PropertyKeyId,
        scope: &PlannerPredicates,
    ) -> Option<&'a CanonicalScalar> {
        Self::row_property(self.vertex(vid), key, scope)
    }
    /// `property` through `vertex_after`, for a vertex-only scan.
    #[inline(never)]
    fn property_after(
        &self,
        last: &Cell<Option<(VId, &'a VertexRow)>>,
        vid: VId,
        key: PropertyKeyId,
        scope: &PlannerPredicates,
    ) -> Option<&'a CanonicalScalar> {
        Self::row_property(self.vertex_after(last, vid), key, scope)
    }
    fn row_matches(
        row: Option<&VertexRow>,
        required: &[VertexPredicate],
        scope: &PlannerPredicates,
    ) -> bool {
        row.is_some_and(|row| {
            required.iter().all(|predicate| {
                predicate.matches_borrowed(
                    row.labels
                        .iter()
                        .copied()
                        .filter(|label| scope.allows_label(*label)),
                    row.props
                        .iter()
                        .filter(|(key, _)| scope.allows_property(*key))
                        .map(|(key, value)| (*key, value)),
                )
            })
        })
    }
    fn row_property<'r>(
        row: Option<&'r VertexRow>,
        key: PropertyKeyId,
        scope: &PlannerPredicates,
    ) -> Option<&'r CanonicalScalar> {
        if !scope.allows_property(key) {
            return None;
        }
        let row = row?;
        row.props
            .binary_search_by_key(&key, |(key, _)| *key)
            .ok()
            .map(|at| &row.props[at].1)
    }
    fn edge_property(
        &self,
        eid: EId,
        key: PropertyKeyId,
        scope: &PlannerPredicates,
    ) -> Option<&CanonicalScalar> {
        if !scope.allows_property(key) {
            return None;
        }
        let (_, row) = self.edge(eid)?;
        row.binary_search_by_key(&key, |(key, _)| *key)
            .ok()
            .map(|at| &row[at].1)
    }
}

/// One admitted vertex's live calls: a unit of work, one more per label the
/// scope shows, the signed node, then its record and its scratch entry. A
/// fresh admission and a replayed one both spend through here.
fn charge_vertex(
    labels: usize,
    node: &mut impl FnMut() -> Result<(), Fault>,
    control: &mut impl FnMut(SourceEvent) -> Result<(), Fault>,
) -> Result<(), Fault> {
    control(SourceEvent::Work)?;
    for _ in 0..labels {
        control(SourceEvent::Work)?;
    }
    node()?;
    control(SourceEvent::SnapshotRecord)?;
    control(SourceEvent::ScratchEntry)
}

/// One admitted edge's live calls, likewise shared by admission and replay.
fn charge_edge(control: &mut impl FnMut(SourceEvent) -> Result<(), Fault>) -> Result<(), Fault> {
    control(SourceEvent::Work)?;
    control(SourceEvent::SnapshotRecord)?;
    control(SourceEvent::ScratchEntry)
}

/// Sort `items` by `key`, keeping the LAST of any repeated key (a map insert
/// overwrites). Already strictly ascending input, a full scan's, is one pass.
fn seal_by_key<T, K: Ord>(items: &mut Vec<T>, key: impl Fn(&T) -> K) {
    if items.windows(2).all(|pair| key(&pair[0]) < key(&pair[1])) {
        return;
    }
    // Stable, so a repeated key's admissions keep their order.
    items.sort_by_key(|item| key(item));
    let mut kept = 0;
    for at in 0..items.len() {
        if at + 1 < items.len() && key(&items[at]) == key(&items[at + 1]) {
            continue;
        }
        items.swap(kept, at);
        kept += 1;
    }
    items.truncate(kept);
}

// Reserve each copied catalog scalar and payload before constructing it.
fn text(
    name: &str,
    control: &mut impl FnMut(SourceEvent) -> Result<(), Fault>,
) -> Result<CanonicalScalar, Option<Fault>> {
    control(SourceEvent::ScratchEntry).map_err(Some)?;
    for _ in 0..name.len().div_ceil(64) {
        control(SourceEvent::Work).map_err(Some)?;
        control(SourceEvent::ScratchEntry).map_err(Some)?;
    }
    CanonicalScalar::ucs_basic_text(name).map_err(|_| None)
}

/// The governed evaluator's checkpoint over the live execution: one signed
/// unit per event, and an evaluator run's units in one call. Pattern and
/// aggregate hosts share it. The callbacks it runs past are table lookups
/// that charge nothing, and the phase's signed work is already charged in
/// runs (`work_runs`).
struct WorkCheckpoint<'e, 'cx, 'permit, Clock>(&'e RefCell<Execution<'cx, 'permit, Clock>>);

impl<Clock: FnMut() -> u64> GovernedCheckpoint<QueryError> for WorkCheckpoint<'_, '_, '_, Clock> {
    const RUNS: bool = true;
    fn unit(&mut self) -> Result<(), QueryError> {
        self.0.borrow_mut().checkpoint()
    }
    fn units(&mut self, n: u64) -> Result<(), QueryError> {
        self.0.borrow_mut().work_units(n)
    }
}

fn pattern_at<Row: GlaOutput, Clock: FnMut() -> u64>(
    snapshot: &Arc<Snapshot>,
    at: CommitSeq,
    pattern: &PreparedGraphPattern<Row>,
    scope: &PlannerPredicates,
    execution: &RefCell<Execution<'_, '_, Clock>>,
    policy: GqlQueryPolicy,
) -> Governed<Row> {
    // Admission and evaluation are one phase: their signed work is charged
    // in runs.
    work_runs(execution, GqlQueryError::Interrupted, || {
        pattern_at_controlled(
            snapshot,
            at,
            pattern,
            scope,
            policy,
            &mut || execution.borrow_mut().node(),
            &mut || execution.borrow_mut().poll(),
            WorkCheckpoint(execution),
        )
    })
}

// One source-admission and GLA body serves both reads and INSERT selection.
// A write supplies its EXISTING permit's controls, not a newly issued read
// allowance. Separate poll and charge callbacks keep hidden history unbilled.
#[allow(clippy::too_many_arguments)]
fn pattern_at_controlled<Row: GlaOutput>(
    snapshot: &Arc<Snapshot>,
    at: CommitSeq,
    pattern: &PreparedGraphPattern<Row>,
    scope: &PlannerPredicates,
    policy: GqlQueryPolicy,
    node: &mut impl FnMut() -> Result<(), QueryError>,
    poll: &mut impl FnMut() -> Result<(), QueryError>,
    mut checkpoint: impl GovernedCheckpoint<QueryError>,
) -> Governed<Row> {
    let mut usage = AdmissionUsage::default();
    let tables = Tables::admit(
        snapshot,
        pattern.plan(),
        at,
        false,
        scope,
        || node().map_err(GqlQueryError::Interrupted),
        &mut || poll().map_err(GqlQueryError::Interrupted),
        &mut |event| {
            checkpoint.unit().map_err(GqlQueryError::Interrupted)?;
            usage.observe::<ReadError, QueryError>(policy, event)
        },
    )?;
    execute_tables(tables, pattern, scope, policy, usage, checkpoint)
}

// One evaluator and all property/metadata accessors serve both source kinds.
fn execute_tables<Row: GlaOutput>(
    tables: Tables<'_>,
    pattern: &PreparedGraphPattern<Row>,
    scope: &PlannerPredicates,
    policy: GqlQueryPolicy,
    usage: AdmissionUsage,
    checkpoint: impl GovernedCheckpoint<QueryError>,
) -> Governed<Row> {
    let result = pattern.plan().execute_governed_with_element_maps(
        tables.records,
        tables.vertices.iter().map(|&(vid, _)| vid),
        tables.edges.iter().map(|(edge, _)| *edge),
        |vid, required| Ok(tables.matches(vid, required, scope)),
        |vid, key| Ok(tables.property(vid, key, scope)),
        |eid, key| Ok(tables.edge_property(eid, key, scope)),
        |vid| Ok(tables.labels.get(&vid).map(Vec::as_slice)),
        |eid| Ok(tables.types.get(&eid)),
        |vid| Ok(tables.property_maps.vertices.get(&vid)),
        |eid| Ok(tables.property_maps.edges.get(&eid)),
        usage.remaining(policy),
        checkpoint,
    );
    usage.finish(policy, result)
}

impl<V: Vfs + Clone> Database<V> {
    /// Private selection adapter for an already authenticated, exclusively
    /// borrowed write. The caller checks ReadWrite rights before entering and
    /// supplies controls backed by its one live write permit. No public API
    /// accepts a predicate program as independent execution authority.
    /// Selection runs before any insertion is staged, so this healthy frontier
    /// is the private transaction's pinned basis. No result is delivered here.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn select_for_authorized_insert<Row: GlaOutput>(
        &self,
        cx: &QueryCx,
        pattern: &PreparedGraphPattern<Row>,
        scope: &PlannerPredicates,
        policy: GqlQueryPolicy,
        mut node: impl FnMut() -> Result<(), QueryError>,
        mut poll: impl FnMut() -> Result<(), QueryError>,
        mut checkpoint: impl FnMut() -> Result<(), QueryError>,
    ) -> Governed<Row> {
        checkpoint().map_err(GqlQueryError::Interrupted)?;
        self.ensure_readable().map_err(GqlQueryError::Source)?;
        cx.with_restriction(|| {
            pattern_at_controlled(
                &self.snapshot,
                self.snapshot.frontier,
                pattern,
                scope,
                policy,
                &mut node,
                &mut poll,
                &mut checkpoint,
            )
        })
    }

    /// Execute a typed GQL pattern on the capability's visible induced graph.
    ///
    /// The trusted host selects its Authority and exact branch mapping; its
    /// clock must be monotone and in the issuer's millisecond epoch. Neither
    /// issuer selection nor clock/branch routing may come from token claims.
    /// The database automatically rejects a mismatched security namespace.
    /// Signature, rights and validity precede source access. Retirement and
    /// expiry are rechecked at governed source/evaluator/delivery boundaries.
    ///
    /// All original label clauses admit vertices conjunctively. Both endpoints
    /// and the relation must admit an edge BEFORE GLA sees it, including path
    /// transit vertices. User predicates/projections see only allowed labels
    /// and properties; forbidden properties behave as absent, not as values
    /// filtered from an already computed result. NULL/OPTIONAL/existence and
    /// bag semantics remain the ordinary GLA executor's responsibility.
    ///
    /// Nodes count admitted vertices, including repeated source admissions;
    /// work counts live source/evaluator/checkpoint boundaries; rows count only
    /// the final result. Both the supplied native policy and signed ceilings
    /// apply. No rows or private execution statistics escape before success.
    ///
    /// This does not authorize ordinary Database methods, bind a durable branch
    /// catalog, or provide storage-descriptor/timing noninterference. Sources
    /// remain resident and their authenticated mixed-scope history work is
    /// charged. Keep the raw database and issuer in the trusted host.
    #[allow(clippy::too_many_arguments)]
    pub fn execute_graph_pattern_authorized<Row: GlaOutput>(
        &self,
        cx: &QueryCx,
        authority: &Authority,
        token: &CapabilityToken,
        branch: &str,
        pattern: &PreparedGraphPattern<Row>,
        policy: GqlQueryPolicy,
        clock: impl FnMut() -> u64,
    ) -> Result<Vec<Row>, QueryError> {
        authorized(
            self,
            cx,
            authority,
            token,
            branch,
            None,
            clock,
            |snapshot, at, scope, execution| {
                pattern_at(snapshot, at, pattern, scope, execution, policy)
                    .map(|value| value.value)
                    .map_err(query_error)
            },
        )
    }

    /// Apply the same current capability to an exact historical cut of this
    /// database. Resolve each winning historical statement before masking it;
    /// never resurrect an older permitted row hidden by a forbidden successor.
    /// A future cut refuses after authentication rather than being clamped.
    #[allow(clippy::too_many_arguments)]
    pub fn execute_graph_pattern_authorized_at<Row: GlaOutput>(
        &self,
        cx: &QueryCx,
        authority: &Authority,
        token: &CapabilityToken,
        branch: &str,
        pattern: &PreparedGraphPattern<Row>,
        as_of: CommitSeq,
        policy: GqlQueryPolicy,
        clock: impl FnMut() -> u64,
    ) -> Result<Vec<Row>, QueryError> {
        authorized(
            self,
            cx,
            authority,
            token,
            branch,
            Some(as_of),
            clock,
            |snapshot, at, scope, execution| {
                pattern_at(snapshot, at, pattern, scope, execution, policy)
                    .map(|value| value.value)
                    .map_err(query_error)
            },
        )
    }
}

#[cfg(test)]
#[path = "query_authorized/tables_tests.rs"]
mod tables_tests;
