//! Database-owned session-local maintained queries.
//!
//! Native GQL, recursive topology and analytics share one owner registry,
//! commit hook, admission meter, failure fence and explicit rebuild lifecycle.
//! Registrations are not durable subscriptions and do not survive reopening.

mod aggregate;
mod components;
mod kcore;
mod native;
mod output;
mod recursive;
mod sets;
mod triangles;
// Reuse the concurrently introduced row-output file as one registry sink.
#[path = "standing_query/output/values.rs"]
mod row;
mod sink;

use crate::{Database, ReadError};
use asupersync::fs::Vfs;
use fgdb_delta_types::{LogicalDeltaBatch, RelationId, ZSet, ZSetError, ZSetEvent};
use fgdb_gql::{GqlQueryPolicy, GraphAggregateRow, PreparedGraphAggregate};
use fgdb_gql::algebra::{GraphValueRow, PreparedGraphPattern};
use fgdb_types::{CommitCx, CommitSeq, QueryCx, VId};
use std::sync::Arc;

#[derive(Clone, Debug)]
pub struct StandingQueryHandle {
    owner: Arc<()>,
    index: usize,
    // Presentation only. The bound definition and all maintenance state still
    // have their sole owner in the ordinary registry entry.
    native: Option<Arc<native::Layout>>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StandingQueryStats {
    pub delta_rows: u64,
    /// Component/core-number views report vertices in the rederived region.
    pub affected_vertices: u64,
    /// Distinct retained/new edge identities examined for a one-hop tick.
    /// Parallel edges count separately; a self-loop counts once.
    /// Recursive/triangle views report work/scratch and delta_rows only;
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
    /// An input view did not publish the same complete successor. Never
    /// interpret an unavailable input or a new baseline as an empty delta.
    DependencyUnavailable,
    /// Computed input column and value-independent scalar failure. A source
    /// binding identity or payload is never included in the diagnostic.
    InputExpression {
        column: usize,
        error: fgdb_gql::GraphIntegerError,
    },
    /// Post-HAVING output expression failure; no partial result is published.
    OutputExpression {
        column: usize,
        error: fgdb_gql::GraphIntegerError,
    },
    InvalidDelta,
}

#[derive(Debug)]
pub enum StandingQueryError {
    ForeignHandle,
    UnknownHandle,
    Unsupported,
    SetSchema(fgdb_gql::GraphSetBuildError),
    NativePrepare(Box<crate::QueryError>),
    NativeClassUnsupported { facade: crate::NativeReadClass },
    /// Reading a healthy maintained result exceeded the caller's delivery
    /// allowance. This does not fence the maintained view or change its policy.
    Delivery(StandingQueryFailure),
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
            Self::SetSchema(error) => error.fmt(f),
            Self::NativePrepare(error) => error.fmt(f),
            Self::NativeClassUnsupported { facade } => {
                write!(f, "native {facade:?} has no supported standing-query registration")
            }
            Self::Delivery(reason) => write!(f, "standing result delivery refused: {reason:?}"),
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
impl core::error::Error for StandingQueryError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::NativePrepare(error) => Some(error.as_ref()),
            Self::SetSchema(error) => Some(error),
            Self::Read(error) => Some(error),
            Self::Interrupted(error) => Some(error.as_ref()),
            _ => None,
        }
    }
}

/// Borrowed rows from one healthy, current maintained result. Existing GQL
/// callers retain GraphAggregateRow as the default. Recursive topology views
/// carry native (VId, VId) pairs; identities are never narrowed to scalars.
#[derive(Debug)]
pub struct StandingQueryView<'a, Row: Ord = GraphAggregateRow> {
    rows: &'a ZSet<Row>,
    ordered: Option<&'a [Arc<Row>]>,
    frontier: CommitSeq,
    stats: &'a StandingQueryStats,
}
impl<Row: Ord> StandingQueryView<'_, Row> {
    pub fn frontier(&self) -> CommitSeq {
        self.frontier
    }
    /// The final selected result bag, including the output window when present.
    /// ALL collisions have positive multiplicities; DISTINCT has weight one
    /// per selected class. Z-set key order is NOT the query's ORDER BY.
    pub fn rows(&self) -> &ZSet<Row> {
        self.rows
    }
    /// Query-order occurrences for ordinary row queries, and for aggregate
    /// definitions with ORDER BY, OFFSET or LIMIT.
    /// Includes duplicate ALL occurrences. Some(empty) is a valid empty page;
    /// None means this view has no ranked result stage (including reachability).
    /// Iteration borrows the same published generation as rows() and frontier().
    pub fn ordered_rows(
        &self,
    ) -> Option<impl DoubleEndedIterator<Item = &Row> + ExactSizeIterator + '_> {
        self.ordered.map(|rows| rows.iter().map(Arc::as_ref))
    }
    pub fn last_maintenance(&self) -> &StandingQueryStats {
        self.stats
    }
}

pub(crate) enum StandingQuery {
    Aggregate(Box<aggregate::StandingQuery>),
    /// The same complete-group producer with a prepared downstream result stage.
    ProjectedAggregate {
        source: Box<aggregate::StandingQuery>,
        output: Box<output::State>,
    },
    /// Complete projected tuples with their counts, not user-visible aggregates.
    Rows {
        source: Box<aggregate::StandingQuery>,
        output: Box<row::State>,
    },
    Reachability(Box<recursive::State>),
    Triangles(Box<triangles::State>),
    Components(Box<components::State>),
    CoreNumbers(Box<kcore::State>),
    /// Dependencies name only earlier registry entries, so the append order
    /// is a topological order without a second scheduler or recursive walk.
    Set(Box<sets::State>),
}

impl StandingQuery {
    fn status(&self) -> (GqlQueryPolicy, CommitSeq, Option<StandingQueryFailure>) {
        match self {
            Self::Aggregate(query) | Self::ProjectedAggregate { source: query, .. }
            | Self::Rows { source: query, .. } => {
                (query.policy, query.frontier, query.failure)
            }
            Self::Reachability(query) => (query.policy, query.frontier, query.failure),
            Self::Triangles(query) => (query.policy, query.frontier, query.failure),
            Self::Components(query) => (query.policy, query.frontier, query.failure),
            Self::CoreNumbers(query) => (query.policy, query.frontier, query.failure),
            Self::Set(query) => (query.policy, query.frontier, query.failure),
        }
    }

    fn record(
        &mut self,
        at: CommitSeq,
        result: Result<(), StandingQueryFailure>,
        stats: StandingQueryStats,
    ) {
        let (frontier, failure, observed) = match self {
            Self::Aggregate(query) | Self::ProjectedAggregate { source: query, .. }
            | Self::Rows { source: query, .. } => {
                (&mut query.frontier, &mut query.failure, &mut query.stats)
            }
            Self::Reachability(query) => {
                (&mut query.frontier, &mut query.failure, &mut query.stats)
            }
            Self::Triangles(query) => {
                (&mut query.frontier, &mut query.failure, &mut query.stats)
            }
            Self::Components(query) => {
                (&mut query.frontier, &mut query.failure, &mut query.stats)
            }
            Self::CoreNumbers(query) => {
                (&mut query.frontier, &mut query.failure, &mut query.stats)
            }
            Self::Set(query) => {
                (&mut query.frontier, &mut query.failure, &mut query.stats)
            }
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
    /// aggregate projection, scalar output expressions, DISTINCT and ranking.
    /// ORDER BY uses exact cells, independent NULL placement and complete-key
    /// tiebreaks. OFFSET/LIMIT apply to final occurrences AFTER DISTINCT.
    ///
    /// Candidates outside a page remain available for deletion/refill. Result
    /// limits count selected occurrences; work/scratch govern initialization
    /// and maintenance of candidates, including output errors outside the page.
    /// A finite page walks its ranked prefix, not the graph. Large offsets cost
    /// that prefix; unbounded ranking materializes the complete ordered result.
    /// Use ordered_rows() for query order and rows() for its selected Z-set bag.
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
            let producer = definition
                .incremental_source_definition()
                .ok_or(StandingQueryError::Unsupported)?;
            let mut output = output::State::new(definition);
            let source =
                self.prepare_standing_query_with_output(cx, producer, policy, Some(&mut output))?;
            Ok(StandingQuery::ProjectedAggregate {
                source: Box::new(source),
                output: Box::new(output),
            })
        } else {
            let query = self.prepare_standing_query(cx, definition, policy)?;
            Ok(StandingQuery::Aggregate(Box::new(query)))
        }
    }

    /// Maintain an ordinary scalar/vertex MATCH result, without requiring the
    /// application to write an aggregate query. Reuses the existing vertex,
    /// fixed-hop and single correlated OPTIONAL/EXISTS/NOT EXISTS maintainers.
    /// ALL retains duplicate occurrences; DISTINCT remains present until the
    /// last supporting occurrence disappears. Original row ordering, NULL
    /// placement and OFFSET/LIMIT apply after this multiplicity decision.
    ///
    /// The counted source and selected bag/sequence publish atomically on each
    /// durable commit. Only initialization/rebuild scans the graph. Retained
    /// tuple counts let OFFSET skip duplicates arithmetically; finite pages
    /// visit their tuple prefix plus changed tuples, then expand selected rows.
    /// Result quotas count selected occurrences, not private support. Work and
    /// scratch are cumulative across source and output, not byte-memory limits.
    ///
    /// The current carrier supports up to 64 scalar/vertex columns and checked
    /// u64 multiplicities per tuple. Unsupported source operators, hidden output
    /// carriers, paths/collections and overflow refuse explicitly. Registration
    /// remains session-local; this is not a durable subscription or delivery API.
    pub fn register_standing_rows(
        &mut self,
        cx: &QueryCx,
        definition: PreparedGraphPattern<GraphValueRow>,
        policy: GqlQueryPolicy,
    ) -> Result<StandingQueryHandle, StandingQueryError> {
        let query = self.prepare_registered_rows(cx, definition, policy)?;
        Ok(self.store_standing_query(query))
    }

    fn prepare_registered_rows(
        &self,
        cx: &QueryCx,
        definition: PreparedGraphPattern<GraphValueRow>,
        policy: GqlQueryPolicy,
    ) -> Result<StandingQuery, StandingQueryError> {
        cx.checkpoint().map_err(StandingQueryError::Interrupted)?;
        self.ensure_readable().map_err(StandingQueryError::Read)?;
        let producer = definition.incremental_row_source_definition().ok_or(StandingQueryError::Unsupported)?;
        let mut output = row::State::new(definition).ok_or(StandingQueryError::Unsupported)?;
        let source = self.prepare_standing_query_with_output(cx, producer, policy, Some(&mut output))?;
        Ok(StandingQuery::Rows { source: Box::new(source), output: Box::new(output) })
    }

    /// Register directed, one-or-more-hop reachability for one relation.
    /// Each native (source, destination) pair has weight one. Parallel edges
    /// retain independent lifetimes; self pairs require a nonempty cycle.
    /// Labels, properties, valid time and path length do not filter this view.
    /// This is an explicit topology API, not a new GQL grammar or a substitute
    /// for bounded WALK/path multiplicity semantics.
    ///
    /// Initialization derives topology from the current authenticated snapshot,
    /// not by replaying historical deltas or historical closures. Physical edge
    /// records (including versions/tombstones) count toward max_snapshot_records;
    /// borrowed source reads, bootstrap and result materialization share one
    /// work/scratch allowance. The result limit bounds the CURRENT closure.
    /// A retired window remains usable when it preserves the exact boundary
    /// identity; a missing/foreign anchor refuses. No historical delta rows are
    /// consumed at initialization (delta_rows is zero). Ordinary maintenance
    /// consumes only the newly committed batch.
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
        StandingQueryHandle {
            owner: Arc::clone(&self.handle_owner),
            index,
            native: None,
        }
    }

    /// Repair any kind of maintained result without changing its handle or
    /// definition. Both aggregate and recursive views rebuild from the same
    /// authoritative current snapshot, including after delta retirement. Preparation
    /// is private: any read, cancellation, budget or arithmetic refusal leaves
    /// the old rows, frontier, policy and failure untouched. Successful repair
    /// replaces all state together and resumes ordinary commit maintenance.
    /// An already durable write is never rolled back by a maintenance failure.
    /// Set compositions rebuild from their current healthy operand views;
    /// repair unavailable dependencies first, then rebuild their dependents.
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
        let current = self
            .standing_queries
            .get(handle.index)
            .ok_or(StandingQueryError::UnknownHandle)?;
        let replacement = match current {
            StandingQuery::Aggregate(query) => {
                self.prepare_registered_aggregate(cx, query.definition.clone(), policy)?
            }
            StandingQuery::ProjectedAggregate { output, .. } => {
                self.prepare_registered_aggregate(cx, output.definition().clone(), policy)?
            }
            StandingQuery::Rows { output, .. } => {
                self.prepare_registered_rows(cx, output.definition().clone(), policy)?
            }
            StandingQuery::Reachability(query) => StandingQuery::Reachability(Box::new(
                self.prepare_standing_reachability(cx, query.relation(), policy)?,
            )),
            StandingQuery::Triangles(query) => StandingQuery::Triangles(Box::new(
                self.prepare_standing_triangles(cx, query.relation(), query.quantifier(), policy)?,
            )),
            StandingQuery::Components(query) => StandingQuery::Components(Box::new(
                self.prepare_standing_components(cx, query.relation, policy)?,
            )),
            StandingQuery::CoreNumbers(query) => StandingQuery::CoreNumbers(Box::new(
                self.prepare_standing_core_numbers(cx, query.relation, policy)?,
            )),
            StandingQuery::Set(query) => StandingQuery::Set(Box::new(
                self.prepare_standing_set(cx, query.inputs, query.operation(), policy, handle.index)?,
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
        let query = self
            .standing_queries
            .get(handle.index)
            .ok_or(StandingQueryError::UnknownHandle)?;
        let (_, frontier, failure) = query.status();
        if let Some(reason) = failure {
            return Err(StandingQueryError::Unavailable { frontier, reason });
        }
        if frontier != self.snapshot.frontier {
            return Err(StandingQueryError::Unavailable {
                frontier,
                reason: StandingQueryFailure::InvalidDelta,
            });
        }
        Ok(query)
    }

    /// Read a native aggregate result. Its bag and optional ordered page share
    /// one publication frontier. A reachability handle refuses rather than
    /// masquerading as aggregate cells or panicking on the wrong row kind.
    pub fn standing_query<'a>(
        &'a self,
        cx: &QueryCx,
        handle: &StandingQueryHandle,
    ) -> Result<StandingQueryView<'a>, StandingQueryError> {
        let (query, rows, ordered) = match self.admitted_standing_query(cx, handle)? {
            StandingQuery::Aggregate(query) => (query.as_ref(), &query.rows, None),
            StandingQuery::ProjectedAggregate { source, output } => {
                (source.as_ref(), &output.rows, output.ordered_rows())
            }
            StandingQuery::Reachability(_) | StandingQuery::Rows { .. }
            | StandingQuery::Triangles(_) | StandingQuery::Components(_)
            | StandingQuery::CoreNumbers(_) | StandingQuery::Set(_) => return Err(StandingQueryError::Unsupported),
        };
        Ok(StandingQueryView {
            rows,
            ordered,
            frontier: query.frontier,
            stats: &query.stats,
        })
    }

    /// Borrow the ordinary MATCH value rows at the current published frontier.
    /// ordered_rows() is always Some, including empty and canonical-order
    /// results. Wrong-kind and foreign handles never expose private carriers.
    pub fn standing_rows<'a>(
        &'a self,
        cx: &QueryCx,
        handle: &StandingQueryHandle,
    ) -> Result<StandingQueryView<'a, GraphValueRow>, StandingQueryError> {
        let StandingQuery::Rows { source, output } = self.admitted_standing_query(cx, handle)? else {
            return Err(StandingQueryError::Unsupported);
        };
        Ok(StandingQueryView { rows: &output.rows, ordered: Some(&output.ordered),
            frontier: source.frontier, stats: &source.stats })
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
        Ok(StandingQueryView {
            rows: &query.rows,
            ordered: None,
            frontier: query.frontier,
            stats: &query.stats,
        })
    }
}

/// One publication lifecycle. A failed derived view never rejects a durable
/// database commit or prevents independently admitted sibling views advancing.
pub(crate) fn publish(queries: &mut [StandingQuery], cx: &CommitCx, batch: &LogicalDeltaBatch) {
    for index in 0..queries.len() {
        // Every dependency precedes its consumer. Borrow already published
        // operands and one exclusive consumer; no cursor can skip a tick.
        let (prior, remaining) = queries.split_at_mut(index);
        let query = &mut remaining[0];
        let (policy, _, failure) = query.status();
        if failure.is_some() {
            continue;
        }
        let mut checkpoint = || {
            cx.checkpoint()
                .map_err(|_| StandingQueryFailure::Interrupted)
        };
        let mut meter = Meter {
            policy,
            stats: StandingQueryStats::default(),
            checkpoint: &mut checkpoint,
        };
        let result = match query {
            StandingQuery::Aggregate(query) => query.maintain(batch, &mut meter),
            StandingQuery::ProjectedAggregate { source, output } => {
                source.maintain_with_output(batch, &mut meter, Some(output.as_mut()))
            }
            StandingQuery::Rows { source, output } => {
                source.maintain_with_output(batch, &mut meter, Some(output.as_mut()))
            }
            StandingQuery::Reachability(query) => query.maintain(cx, batch, &mut meter),
            StandingQuery::Triangles(query) => query.maintain(cx, batch, &mut meter),
            StandingQuery::Components(query) => query.maintain(cx, batch, &mut meter),
            StandingQuery::CoreNumbers(query) => query.maintain(cx, batch, &mut meter),
            StandingQuery::Set(query) => query.maintain(batch, prior, &mut meter),
        };
        query.record(batch.commit_seq(), result, meter.stats);
    }
}
