//! Database-owned session-local maintained queries.
//!
//! Native GQL aggregates and recursive topology views share one owner registry,
//! commit hook, admission meter, failure fence and explicit rebuild lifecycle.
//! Registrations are not durable subscriptions and do not survive reopening.

mod aggregate;
mod output;
mod recursive;

use crate::{Database, ReadError};
use asupersync::fs::Vfs;
use fgdb_delta_types::{LogicalDeltaBatch, RelationId, ZSet, ZSetError, ZSetEvent};
use fgdb_gql::{GqlQueryPolicy, GraphAggregateRow, PreparedGraphAggregate};
use fgdb_types::{CommitCx, CommitSeq, QueryCx, VId};
use std::sync::Arc;

#[derive(Clone, Debug)]
pub struct StandingQueryHandle {
    owner: Arc<()>,
    index: usize,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StandingQueryStats {
    pub delta_rows: u64,
    pub affected_vertices: u64,
    /// Distinct retained/new edge identities examined for a one-hop tick.
    /// Parallel edges count separately; a self-loop counts once.
    /// Recursive views currently report work/scratch and delta_rows only;
    /// their affected_vertices/affected_edges counters remain zero.
    pub affected_edges: u64,
    pub work_units: u64,
    pub scratch_entries: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StandingQueryFailure {
    WorkBudget,
    ScratchBudget,
    SnapshotBudget,
    ResultBudget,
    Interrupted,
    Arithmetic,
    NonIntegerSum,
    NonIntegerHaving,
    /// Computed input column and value-independent scalar failure. A source
    /// binding identity or payload is never included in the diagnostic.
    InputExpression { column: usize, error: fgdb_gql::GraphIntegerError },
    /// Post-HAVING output expression failure; no partial result is published.
    OutputExpression { column: usize, error: fgdb_gql::GraphIntegerError },
    InvalidDelta,
}

#[derive(Debug)]
pub enum StandingQueryError {
    ForeignHandle,
    UnknownHandle,
    Unsupported,
    Unavailable {
        frontier: CommitSeq,
        reason: StandingQueryFailure,
    },
    Read(ReadError),
    Interrupted(Box<asupersync::error::Error>),
    Maintenance(StandingQueryFailure),
}
impl core::fmt::Display for StandingQueryError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::ForeignHandle => f.write_str("standing query belongs to another opened database"),
            Self::UnknownHandle => f.write_str("unknown standing query"),
            Self::Unsupported => {
                f.write_str("standing query kind or definition is unsupported by this operation")
            }
            Self::Unavailable { frontier, reason } => write!(
                f,
                "standing query unavailable after {frontier:?}: {reason:?}"
            ),
            Self::Read(error) => error.fmt(f),
            Self::Interrupted(error) => error.fmt(f),
            Self::Maintenance(reason) => {
                write!(f, "standing query initialization refused: {reason:?}")
            }
        }
    }
}
impl core::error::Error for StandingQueryError {}

/// Borrowed rows from one healthy, current maintained result. Existing GQL
/// callers retain GraphAggregateRow as the default. Recursive topology views
/// carry native (VId, VId) pairs; identities are never narrowed to scalars.
#[derive(Debug)]
pub struct StandingQueryView<'a, Row: Ord = GraphAggregateRow> {
    rows: &'a ZSet<Row>,
    frontier: CommitSeq,
    stats: &'a StandingQueryStats,
}
impl<Row: Ord> StandingQueryView<'_, Row> {
    pub fn frontier(&self) -> CommitSeq { self.frontier }
    /// ALL output collisions have positive multiplicities; DISTINCT output
    /// has weight one per semantic class. This is a bag, not a ranked sequence.
    pub fn rows(&self) -> &ZSet<Row> { self.rows }
    pub fn last_maintenance(&self) -> &StandingQueryStats { self.stats }
}

pub(crate) enum StandingQuery {
    Aggregate(Box<aggregate::StandingQuery>),
    /// The same complete-group producer with a prepared downstream projection.
    ProjectedAggregate {
        source: Box<aggregate::StandingQuery>,
        output: Box<output::State>,
    },
    Reachability(Box<recursive::State>),
}

impl StandingQuery {
    fn status(&self) -> (GqlQueryPolicy, CommitSeq, Option<StandingQueryFailure>) {
        match self {
            Self::Aggregate(query) | Self::ProjectedAggregate { source: query, .. } => {
                (query.policy, query.frontier, query.failure)
            }
            Self::Reachability(query) => (query.policy, query.frontier, query.failure),
        }
    }

    fn record(
        &mut self,
        at: CommitSeq,
        result: Result<(), StandingQueryFailure>,
        stats: StandingQueryStats,
    ) {
        let (frontier, failure, observed) = match self {
            Self::Aggregate(query) | Self::ProjectedAggregate { source: query, .. } => {
                (&mut query.frontier, &mut query.failure, &mut query.stats)
            }
            Self::Reachability(query) => (&mut query.frontier, &mut query.failure, &mut query.stats),
        };
        match result {
            Ok(()) => *frontier = at,
            Err(reason) => *failure = Some(reason),
        }
        *observed = stats;
    }
}

struct Meter<'a> {
    policy: GqlQueryPolicy,
    stats: StandingQueryStats,
    checkpoint: &'a mut dyn FnMut() -> Result<(), StandingQueryFailure>,
}
impl Meter<'_> {
    fn charge(&mut self, event: ZSetEvent) -> Result<(), StandingQueryFailure> {
        (self.checkpoint)()?;
        let (counter, limit, error) = match event {
            ZSetEvent::Work => (
                &mut self.stats.work_units,
                self.policy.evaluator.max_work_units,
                StandingQueryFailure::WorkBudget,
            ),
            ZSetEvent::ScratchEntry => (
                &mut self.stats.scratch_entries,
                self.policy.evaluator.max_scratch_entries,
                StandingQueryFailure::ScratchBudget,
            ),
        };
        *counter = counter.checked_add(1).ok_or(error)?;
        if *counter > limit {
            return Err(error);
        }
        Ok(())
    }
    fn units(&mut self, event: ZSetEvent, units: usize) -> Result<(), StandingQueryFailure> {
        for _ in 0..units {
            self.charge(event)?;
        }
        Ok(())
    }
}
fn zset_error(error: ZSetError<StandingQueryFailure>) -> StandingQueryFailure {
    match error {
        ZSetError::Control(error) | ZSetError::Callback(error) => error,
        _ => StandingQueryFailure::Arithmetic,
    }
}
impl<V: Vfs + Clone> Database<V> {
    /// Register a session-local native aggregate over the current snapshot.
    /// Source expressions, grouping and HAVING remain upstream of final key/
    /// aggregate projection, scalar output expressions and output DISTINCT.
    /// Hidden complete groups remain maintained. Result limits count final ALL
    /// occurrences or DISTINCT classes; work/scratch govern all retained state.
    /// ORDER BY and pagination remain unsupported by this bag-result API.
    pub fn register_standing_query(
        &mut self,
        cx: &QueryCx,
        definition: PreparedGraphAggregate,
        policy: GqlQueryPolicy,
    ) -> Result<StandingQueryHandle, StandingQueryError> {
        let query = self.prepare_registered_aggregate(cx, definition, policy)?;
        Ok(self.store_standing_query(query))
    }

    fn prepare_registered_aggregate(
        &self,
        cx: &QueryCx,
        definition: PreparedGraphAggregate,
        policy: GqlQueryPolicy,
    ) -> Result<StandingQuery, StandingQueryError> {
        cx.checkpoint().map_err(StandingQueryError::Interrupted)?;
        if definition.has_incremental_output_transform() {
            let producer = definition.incremental_source_definition()
                .ok_or(StandingQueryError::Unsupported)?;
            let mut output = output::State::new(definition);
            let source = self.prepare_standing_query_with_output(cx, producer, policy, Some(&mut output))?;
            Ok(StandingQuery::ProjectedAggregate { source: Box::new(source), output: Box::new(output) })
        } else {
            let query = self.prepare_standing_query(cx, definition, policy)?;
            Ok(StandingQuery::Aggregate(Box::new(query)))
        }
    }

    /// Register directed, one-or-more-hop reachability for one relation.
    /// Each native (source, destination) pair has weight one. Parallel edges
    /// retain independent lifetimes; self pairs require a nonempty cycle.
    /// Labels, properties, valid time and path length do not filter this view.
    /// This is an explicit topology API, not a new GQL grammar or a substitute
    /// for bounded WALK/path multiplicity semantics.
    ///
    /// Initialization replays the complete retained, authenticated delta window
    /// under one cumulative work/scratch budget. max_snapshot_records bounds
    /// historical delta rows admitted, not only currently live edges. The
    /// result-row budget bounds the final closure, not intermediate historical
    /// peaks; those still consume work and scratch admission. A retired prefix
    /// refuses. Ordinary maintenance consumes only the newly committed batch.
    ///
    /// The database owns catch-up: after each successful write the view is
    /// current or explicitly unavailable. No caller polling, second commit log,
    /// durable registration, delivery acknowledgement or spill is introduced.
    /// The retained closure can require quadratic space.
    pub fn register_standing_reachability(
        &mut self,
        cx: &QueryCx,
        relation: RelationId,
        policy: GqlQueryPolicy,
    ) -> Result<StandingQueryHandle, StandingQueryError> {
        let query = self.prepare_standing_reachability(cx, relation, policy)?;
        Ok(self.store_standing_query(StandingQuery::Reachability(Box::new(query))))
    }

    fn store_standing_query(&mut self, query: StandingQuery) -> StandingQueryHandle {
        let index = self.standing_queries.len();
        self.standing_queries.push(query);
        StandingQueryHandle { owner: Arc::clone(&self.handle_owner), index }
    }

    /// Repair either kind of maintained result without changing its handle or
    /// definition. Native aggregates rebuild from the authoritative snapshot;
    /// recursive views replay complete retained Chronicle history. Preparation
    /// is private: any read, cancellation, budget or arithmetic refusal leaves
    /// the old rows, frontier, policy and failure untouched. Successful repair
    /// replaces all state together and resumes ordinary commit maintenance.
    /// An already durable write is never rolled back by a maintenance failure.
    pub fn rebuild_standing_query(
        &mut self,
        cx: &QueryCx,
        handle: &StandingQueryHandle,
        policy: GqlQueryPolicy,
    ) -> Result<CommitSeq, StandingQueryError> {
        cx.checkpoint().map_err(StandingQueryError::Interrupted)?;
        if !Arc::ptr_eq(&self.handle_owner, &handle.owner) {
            return Err(StandingQueryError::ForeignHandle);
        }
        self.ensure_readable().map_err(StandingQueryError::Read)?;
        let current = self.standing_queries.get(handle.index)
            .ok_or(StandingQueryError::UnknownHandle)?;
        let replacement = match current {
            StandingQuery::Aggregate(query) => self.prepare_registered_aggregate(cx, query.definition.clone(), policy)?,
            StandingQuery::ProjectedAggregate { output, .. } => {
                self.prepare_registered_aggregate(cx, output.definition().clone(), policy)?
            }
            StandingQuery::Reachability(query) => StandingQuery::Reachability(Box::new(
                self.prepare_standing_reachability(cx, query.relation(), policy)?,
            )),
        };
        let frontier = replacement.status().1;
        // No source mutation, await or fallible work between preparation and swap.
        self.standing_queries[handle.index] = replacement;
        Ok(frontier)
    }

    fn admitted_standing_query(
        &self,
        cx: &QueryCx,
        handle: &StandingQueryHandle,
    ) -> Result<&StandingQuery, StandingQueryError> {
        cx.checkpoint().map_err(StandingQueryError::Interrupted)?;
        if !Arc::ptr_eq(&self.handle_owner, &handle.owner) {
            return Err(StandingQueryError::ForeignHandle);
        }
        self.ensure_readable().map_err(StandingQueryError::Read)?;
        let query = self.standing_queries.get(handle.index)
            .ok_or(StandingQueryError::UnknownHandle)?;
        let (_, frontier, failure) = query.status();
        if let Some(reason) = failure {
            return Err(StandingQueryError::Unavailable { frontier, reason });
        }
        if frontier != self.snapshot.frontier {
            return Err(StandingQueryError::Unavailable {
                frontier, reason: StandingQueryFailure::InvalidDelta,
            });
        }
        Ok(query)
    }

    /// Read a native aggregate result. A reachability handle refuses rather
    /// than masquerading as aggregate cells or panicking on the wrong row kind.
    pub fn standing_query<'a>(
        &'a self,
        cx: &QueryCx,
        handle: &StandingQueryHandle,
    ) -> Result<StandingQueryView<'a>, StandingQueryError> {
        let (query, rows) = match self.admitted_standing_query(cx, handle)? {
            StandingQuery::Aggregate(query) => (query.as_ref(), &query.rows),
            StandingQuery::ProjectedAggregate { source, output } => (source.as_ref(), &output.rows),
            StandingQuery::Reachability(_) => return Err(StandingQueryError::Unsupported),
        };
        Ok(StandingQueryView { rows, frontier: query.frontier, stats: &query.stats })
    }

    /// Borrow the current recursive pair set, with the same owner, health,
    /// cancellation and failure checks as ordinary standing-query reads.
    pub fn standing_reachability<'a>(
        &'a self,
        cx: &QueryCx,
        handle: &StandingQueryHandle,
    ) -> Result<StandingQueryView<'a, (VId, VId)>, StandingQueryError> {
        let StandingQuery::Reachability(query) = self.admitted_standing_query(cx, handle)? else {
            return Err(StandingQueryError::Unsupported);
        };
        Ok(StandingQueryView { rows: &query.rows, frontier: query.frontier, stats: &query.stats })
    }
}

/// One publication lifecycle. A failed derived view never rejects a durable
/// database commit or prevents independently admitted sibling views advancing.
pub(crate) fn publish(queries: &mut [StandingQuery], cx: &CommitCx, batch: &LogicalDeltaBatch) {
    for query in queries {
        let (policy, _, failure) = query.status();
        if failure.is_some() { continue; }
        let mut checkpoint = || cx.checkpoint().map_err(|_| StandingQueryFailure::Interrupted);
        let mut meter = Meter { policy, stats: StandingQueryStats::default(), checkpoint: &mut checkpoint };
        let result = match query {
            StandingQuery::Aggregate(query) => query.maintain(batch, &mut meter),
            StandingQuery::ProjectedAggregate { source, output } => {
                source.maintain_with_output(batch, &mut meter, Some(output.as_mut()))
            }
            StandingQuery::Reachability(query) => query.maintain(cx, batch, &mut meter),
        };
        query.record(batch.commit_seq(), result, meter.stats);
    }
}
