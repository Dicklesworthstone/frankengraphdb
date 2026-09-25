//! Beacon retrieval through the same pinned, live-authorized session as GQL.
//! The query never receives the privileged Database, issuer, clock or index.

use super::{AuthorizedReadSession, Live};
use crate::query::beacon::{self, Meter, Options, Scan, SharedWork};
use crate::{EmbeddedReadView, QueryError, ReadError};
use fgdb_beacon::read::{Projection, ReadError as SearchError, ReadPolicy, Rows, Search};
use fgdb_beacon::{BeaconError, BeaconIndex, IndexConfig, WorkControl};
use fgdb_gql::{GqlBudgetDimension, GqlQueryError, GqlQueryPolicy, GraphSymbolResolver};
use fgdb_types::{CommitSeq, QueryCx};
use fgdb_warden::{Error as WardenError, LimitDimension, PlannerPredicates};
use std::cell::RefCell;

#[path = "beacon/prepared.rs"]
mod prepared;
pub use prepared::AuthorizedBeaconIndex;

type Error = SearchError<ReadError, QueryError>;

#[cfg(test)]
#[path = "beacon_tests.rs"]
mod tests;

// Preserve the native Beacon error as a VALUE until session.run has performed
// its final live checks. Authorization failures remain outer QueryErrors, so
// expiry/retirement close the session through the existing terminal rule.
fn finish<T>(result: Result<Result<T, BeaconError>, QueryError>) -> Result<T, Error> {
    match result {
        Ok(result) => result.map_err(SearchError::Index),
        Err(QueryError::Read(error)) => Err(SearchError::Read(error)),
        Err(error) => Err(SearchError::Interrupted(error)),
    }
}

fn settle<T, F>(
    work: RefCell<Meter<QueryError, F>>,
    result: Result<T, BeaconError>,
) -> Result<Result<T, BeaconError>, QueryError>
where
    F: FnMut(usize) -> Result<(), QueryError>,
{
    match work.into_inner().into_failure() {
        Some(error) => Err(error),
        None => Ok(result),
    }
}

fn charge(execution: &Live<'_, '_, '_>, units: usize) -> Result<(), QueryError> {
    let mut live = execution.borrow_mut();
    live.checkpoint()?;
    let units = u64::try_from(units).map_err(|_| live.refusal(WardenError::TooLarge))?;
    let now = (live.clock)();
    let charged = live.permit.charge_work_at(now, units);
    charged.map_err(|error| live.refusal(error))
}

// A request may narrow but never widen the host's session policy. Converting
// an unrepresentable u64 ceiling to usize::MAX preserves the platform domain;
// no observed count or semantic query value is narrowed.
fn policy(request: ReadPolicy, host: GqlQueryPolicy) -> ReadPolicy {
    let cap = |requested: usize, allowed: u64| {
        usize::try_from(allowed).map_or(requested, |allowed| requested.min(allowed))
    };
    ReadPolicy {
        max_work_units: cap(request.max_work_units, host.evaluator.max_work_units),
        max_source_scratch: cap(
            request.max_source_scratch,
            host.evaluator.max_scratch_entries,
        ),
        max_staging_rows: host
            .rows
            .max_snapshot_records()
            .map_or(request.max_staging_rows, |n| {
                cap(request.max_staging_rows, n)
            }),
        max_result_rows: host
            .rows
            .max_result_rows()
            .map_or(request.max_result_rows, |n| cap(request.max_result_rows, n)),
    }
}

fn admit_query(
    query: Search<'_>,
    scope: &PlannerPredicates,
    host: GqlQueryPolicy,
    execution: &Live<'_, '_, '_>,
) -> Result<(), QueryError> {
    let count = u64::try_from(query.k())
        .map_err(|_| execution.borrow_mut().refusal(WardenError::TooLarge))?;
    if count > scope.limits().max_rows {
        return Err(execution
            .borrow_mut()
            .refusal(WardenError::LimitExceeded(LimitDimension::Rows)));
    }
    host.rows
        .check(GqlBudgetDimension::ResultRows, count)
        .map_err(|error| QueryError::Pattern(GqlQueryError::Rows(error)))
}

// Copy only enabled projection metadata, after dimension and work admission.
// In particular, a text-only request cannot inspect or clone vector keys.
fn definition(
    options: &Options,
    config: IndexConfig,
    host: GqlQueryPolicy,
    work: &mut impl WorkControl,
) -> Result<Options, BeaconError> {
    work.charge(1)?;
    config.validate()?;
    if config.text.is_some() && options.projection.text.is_none() {
        return Err(BeaconError::InvalidConfig(
            "text index needs a text property",
        ));
    }
    let mut vector_keys = Vec::new();
    if let Some(vector) = &config.vector {
        if options.projection.vector.len() != vector.dimensions {
            return Err(BeaconError::InvalidConfig(
                "vector properties must match dimensions",
            ));
        }
        if vector.dimensions > config.max_vector_values {
            return Err(BeaconError::ResourceLimit {
                resource: "vector projection",
                limit: config.max_vector_values,
            });
        }
        work.charge(vector.dimensions)?;
        vector_keys
            .try_reserve_exact(vector.dimensions)
            .map_err(|_| BeaconError::ResourceLimit {
                resource: "vector projection allocation",
                limit: vector.dimensions,
            })?;
        vector_keys.extend_from_slice(&options.projection.vector);
    }
    Ok(Options {
        as_of: options.as_of,
        vertex_label: options.vertex_label,
        projection: Projection {
            text: options.projection.text.filter(|_| config.text.is_some()),
            vector: vector_keys,
        },
        index: config,
        policy: policy(options.policy, host),
    })
}

#[allow(clippy::too_many_arguments)]
fn build<F>(
    view: &EmbeddedReadView,
    at: CommitSeq,
    scope: &PlannerPredicates,
    host: GqlQueryPolicy,
    execution: &Live<'_, '_, '_>,
    options: &Options,
    work: &RefCell<Meter<QueryError, F>>,
) -> Result<(BeaconIndex, u64), BeaconError>
where
    F: FnMut(usize) -> Result<(), QueryError>,
{
    let mut admitted = 0_u64;
    // Hidden histories must not affect caller-controllable refusal thresholds.
    // Reuse the production authorization-before-projection builder, including
    // its poll-only history walk and masking BEFORE value/type inspection.
    let mut poll = || {
        execution
            .borrow_mut()
            .poll()
            .map_err(|error| work.borrow_mut().refuse(error))
    };
    let index = beacon::build(
        &view.snapshot,
        at,
        options,
        options.index.clone(),
        work,
        Scan::Unmetered(&mut poll),
        |row| {
            if !scope.allows_vertex(&row.labels) {
                return Ok(false);
            }
            execution
                .borrow_mut()
                .node()
                .map_err(|error| work.borrow_mut().refuse(error))?;
            let next = admitted.checked_add(1).ok_or(BeaconError::ResourceLimit {
                resource: "admitted vertices",
                limit: usize::MAX,
            })?;
            host.rows
                .check(GqlBudgetDimension::SnapshotRecords, next)
                .map_err(|error| {
                    work.borrow_mut()
                        .refuse(QueryError::Pattern(GqlQueryError::Rows(error)))
                })?;
            // One visible-source metadata admission, independent of hidden
            // versions and before user label selection, like signed nodes.
            if u128::from(admitted) >= options.policy.max_source_scratch as u128 {
                return Err(BeaconError::ResourceLimit {
                    resource: "admitted vertex scratch",
                    limit: options.policy.max_source_scratch,
                });
            }
            admitted = next;
            Ok(true)
        },
        |label| scope.allows_label(label),
        |key| scope.allows_property(key),
    )?;
    Ok((index, admitted))
}

impl<R: GraphSymbolResolver, C: FnMut() -> u64> AuthorizedReadSession<'_, R, C> {
    /// Search this session's fixed, capability-visible generation. None selects
    /// the session frontier, not the current writer. An explicit historical cut
    /// must belong to this pin. No query catalog callback or raw view escapes.
    ///
    /// Text, exact/approximate vector and rational candidate fusion share the
    /// existing Beacon engines. Historical visibility and original-label scope
    /// precede label selection, property projection, statistics and ANN routing.
    /// Request limits can only narrow the host policy. Signed/native records
    /// count admitted vertices before user selection; returned hits are charged
    /// exactly once by the same session delivery gate used by native queries.
    ///
    /// One live permit covers the entire execution, including empty answers and
    /// late errors. Expiry/retirement and clock rollback close the session; normal
    /// query/resource errors retain its pin. Host unwinding follows run's guard.
    /// This builds per call; it is not a durable index, spill or timing isolation.
    pub fn beacon_search(
        &mut self,
        cx: &QueryCx,
        options: &Options,
        query: Search<'_>,
    ) -> Result<Rows, Error> {
        finish(self.run(cx, |view, _, scope, _, host, execution| {
            admit_query(query, scope, host, execution)?;
            let at = options.as_of.unwrap_or(view.frontier());
            view.snapshot.check_frontier(at).map_err(QueryError::Read)?;
            let work = RefCell::new(Meter::new(
                policy(options.policy, host).max_work_units,
                |units| charge(execution, units),
            ));
            let result = (|| {
                work.borrow_mut().charge(1)?;
                let config = options.config_for(query)?;
                query.validate(&config, &mut SharedWork(&work))?;
                let options = definition(options, config, host, &mut SharedWork(&work))?;
                let (index, _) = build(view, at, scope, host, execution, &options, &work)?;
                let rows = query.execute(&index.snapshot(), &mut SharedWork(&work))?;
                work.borrow_mut().charge(1)?;
                Ok(rows)
            })();
            let result = settle(work, result)?;
            let rows = result.as_ref().map_or(0, Rows::len);
            Ok((result, rows))
        }))
    }
}
