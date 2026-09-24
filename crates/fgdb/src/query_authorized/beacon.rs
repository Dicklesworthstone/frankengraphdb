//! Beacon consumes the capability-visible corpus, not filtered search hits.
//! Historical winners, object predicates and property masking precede all
//! tokenization, BM25 statistics, vector validation and HNSW construction.

use super::authorized_with_errors;
use crate::query::beacon::{self, Meter, Options};
use crate::{Database, QueryError, ReadError};
use asupersync::fs::Vfs;
use fgdb_beacon::BeaconError;
use fgdb_beacon::read::{ReadError as BeaconReadError, Rows, Search};
use fgdb_types::QueryCx;
use fgdb_warden::{Authority, CapabilityToken, Error as WardenError, LimitDimension};
use std::cell::RefCell;

type Error = BeaconReadError<ReadError, QueryError>;

fn control_error(error: QueryError) -> Error {
    match error {
        QueryError::Read(error) => Error::Read(error),
        other => Error::Interrupted(other),
    }
}

impl<V: Vfs + Clone> Database<V> {
    /// Search the current capability-visible corpus at one historical cut.
    ///
    /// The trusted host selects Authority, branch routing and a monotone clock
    /// in the issuer's millisecond epoch. Signature, namespace, read rights and
    /// validity precede source access. Keep the ordinary privileged Database
    /// APIs, Authority and clock out of token-holder reach.
    ///
    /// Resolve historical winners BEFORE applying every original-label clause.
    /// User label selection then sees only permitted labels. Forbidden text or
    /// vector-coordinate properties behave as absent, before inspecting their
    /// values or types. An incomplete vector is omitted, never zero-filled.
    /// Only this restricted corpus supplies BM25 statistics and ANN routing;
    /// hidden objects cannot participate as routing bridges or change scores
    /// through the corpus statistics. This is not a post-filtered index.
    ///
    /// One live permit spans source, projection, construction, search and final
    /// delivery. Signed nodes count capability-admitted vertices before user
    /// selection. Signed work includes native work units and live-checkpoint
    /// overhead. Requested k must fit both signed and native row ceilings;
    /// actual rows are charged once, immediately before release. Native source,
    /// staging and index limits also apply. Empty results still pass the final
    /// live gate. The first authorization/cancellation cause remains typed.
    /// No index, permit, source root or private corpus statistics are returned.
    ///
    /// This is per-execution resident construction, not a maintained index or
    /// GLA operator. Existing mixed-scope history traversal is still inspected
    /// and charged: this does NOT establish descriptor-I/O, timing, error-detail
    /// or resource-failure noninterference. ANN remains approximate; exact
    /// fusion ranks only its explicitly selected candidate population.
    #[allow(clippy::too_many_arguments)]
    pub fn beacon_search_authorized(
        &self,
        cx: &QueryCx,
        authority: &Authority,
        token: &CapabilityToken,
        branch: &str,
        options: &Options,
        query: Search<'_>,
        clock: impl FnMut() -> u64,
    ) -> Result<Rows, BeaconReadError<ReadError, QueryError>> {
        // Unwrap only the selected native lane so the shared delivery gate
        // charges actual hit rows, not one enum wrapper or a duplicated copy.
        match query {
            Search::Text { .. } => search(
                self,
                cx,
                authority,
                token,
                branch,
                options,
                query,
                clock,
                |rows| match rows {
                    Rows::Text(rows) => Ok(rows),
                    _ => Err(BeaconError::Invariant("text search returned another lane")),
                },
            )
            .map(Rows::Text),
            Search::Vector { .. } => search(
                self,
                cx,
                authority,
                token,
                branch,
                options,
                query,
                clock,
                |rows| match rows {
                    Rows::Vector(rows) => Ok(rows),
                    _ => Err(BeaconError::Invariant(
                        "vector search returned another lane",
                    )),
                },
            )
            .map(Rows::Vector),
            Search::Hybrid(_) => search(
                self,
                cx,
                authority,
                token,
                branch,
                options,
                query,
                clock,
                |rows| match rows {
                    Rows::Hybrid(rows) => Ok(rows),
                    _ => Err(BeaconError::Invariant(
                        "hybrid search returned another lane",
                    )),
                },
            )
            .map(Rows::Hybrid),
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn search<V: Vfs + Clone, Row, Clock: FnMut() -> u64>(
    database: &Database<V>,
    cx: &QueryCx,
    authority: &Authority,
    token: &CapabilityToken,
    branch: &str,
    options: &Options,
    query: Search<'_>,
    clock: Clock,
    unpack: fn(Rows) -> Result<Vec<Row>, BeaconError>,
) -> Result<Vec<Row>, Error> {
    authorized_with_errors(
        database,
        cx,
        authority,
        token,
        branch,
        options.as_of,
        clock,
        control_error,
        |snapshot, at, scope, execution| {
            if query.k() as u128 > u128::from(scope.limits().max_rows) {
                return Err(control_error(
                    execution
                        .borrow_mut()
                        .refusal(WardenError::LimitExceeded(LimitDimension::Rows)),
                ));
            }
            let work = RefCell::new(Meter::new(options.policy.max_work_units, |units| {
                let mut live = execution.borrow_mut();
                live.checkpoint()?;
                let units =
                    u64::try_from(units).map_err(|_| live.refusal(WardenError::TooLarge))?;
                let now = (live.clock)();
                let charged = live.permit.charge_work_at(now, units);
                charged.map_err(|error| live.refusal(error))
            }));
            // FG-INV-20: the history walk polls cancellation only; admitted
            // rows (and their visible labels) are the only source charges.
            let mut poll = || {
                let polled = execution.borrow_mut().poll();
                polled.map_err(|error| work.borrow_mut().refuse(error))
            };
            let result = beacon::evaluate(
                snapshot,
                at,
                options,
                query,
                &work,
                beacon::Scan::Unmetered(&mut poll),
                |row| {
                    if !scope.allows_vertex(&row.labels) {
                        return Ok(false);
                    }
                    execution
                        .borrow_mut()
                        .node()
                        .map_err(|error| work.borrow_mut().refuse(error))?;
                    Ok(true)
                },
                |label| scope.allows_label(label),
                |key| scope.allows_property(key),
            );
            let rows = work.into_inner().finish::<ReadError, _>(result)?;
            unpack(rows).map_err(Error::Index)
        },
    )
}
