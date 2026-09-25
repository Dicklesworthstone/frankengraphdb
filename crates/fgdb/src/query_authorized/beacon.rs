//! Beacon consumes the capability-visible corpus, not filtered search hits.
//! Historical winners, object predicates and property masking precede all
//! tokenization, BM25 statistics, vector validation and HNSW construction.

use super::authorized_with_errors;
use crate::query::beacon::{self, Meter, Options};
use crate::{Database, QueryError, ReadError};
use asupersync::fs::Vfs;
use fgdb_beacon::expansion::ExpansionSpec;
use fgdb_beacon::read::{ReadError as BeaconReadError, Rows, Search};
use fgdb_beacon::{BeaconError, GraphHybridHit, GraphHybridQuery};
use fgdb_delta_types::RelationId;
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

    /// Fuse graph expansion with text/vector retrieval over one capability-
    /// visible historical cut. This is the governed counterpart of
    /// [`Database::beacon_search_graph`], not filtering of privileged hits.
    ///
    /// Original-label predicates admit vertices before projection or routing.
    /// Only allowed relations with BOTH endpoints in that selected corpus may
    /// contribute arcs. Hidden/missing/out-of-selection seeds are absent;
    /// hidden transit nodes and forbidden relations cannot create shortcuts.
    /// Property masks apply before vector validation and BM25 statistics.
    ///
    /// One live Warden permit and one native work allowance cover all three
    /// lanes, expansion and fusion. Requested k must fit both row ceilings;
    /// actual fused rows are charged once at final delivery, including a live
    /// check for empty outputs. Hidden history is polled without signed work
    /// charges. No source, graph, index or private statistics escape.
    ///
    /// The host owns authority, branch routing and the monotone issuer clock.
    /// This remains a bounded resident execution, not a durable index, spill
    /// implementation, mandatory secure-view facade or timing-isolation proof.
    /// Candidate-limited fusion and ANN retain their existing approximation
    /// boundaries; adding the graph lane does not certify an exhaustive answer.
    #[allow(clippy::too_many_arguments)]
    pub fn beacon_search_graph_authorized(
        &self,
        cx: &QueryCx,
        authority: &Authority,
        token: &CapabilityToken,
        branch: &str,
        options: &Options,
        query: GraphHybridQuery<'_>,
        expansion: ExpansionSpec<'_, RelationId>,
        clock: impl FnMut() -> u64,
    ) -> Result<Vec<GraphHybridHit>, BeaconReadError<ReadError, QueryError>> {
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
                if query.retrieval.k as u128 > u128::from(scope.limits().max_rows) {
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
                // The SAME poll-only control covers vertex and edge history.
                // Charging a rejected edge would disclose hidden incidence via
                // signed MaxWork or the native work budget (FG-INV-20).
                let mut poll = || {
                    let polled = execution.borrow_mut().poll();
                    polled.map_err(|error| work.borrow_mut().refuse(error))
                };
                let result = beacon::graph::evaluate(
                    snapshot,
                    at,
                    options,
                    query,
                    expansion,
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
                    |relation| scope.allows_relation(relation),
                );
                work.into_inner().finish::<ReadError, _>(result)
            },
        )
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

#[cfg(test)]
mod graph_tests {
    use super::*;
    use crate::{DatabaseKeys, MemVfs, WriteBatch};
    use asupersync::lab::run_async_under_lab;
    use asupersync::security::key::AuthKey;
    use fgdb_beacon::expansion::{ExpansionDirection, ExpansionLimits};
    use fgdb_beacon::{
        DistanceMetric, ExactHybridQuery, ExactRrfProfile, HnswConfig, TextMatch, VectorSearch,
    };
    use fgdb_delta_types::{LabelId, PropertyKeyId, SchemaEpoch};
    use fgdb_types::{
        CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId,
    };
    use fgdb_warden::{Grant, QueryLimits, Scope};

    const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x83; 32]);

    async fn graph(cx: &CommitCx, hidden: bool) -> Database<MemVfs> {
        let mut db = Database::open_memory(cx, DatabaseKeys::new([0x81; 32], NS, [0x82; 32]))
            .await
            .unwrap();
        let mut batch = WriteBatch::new(RelationId(1));
        for id in [1, 2, 3] {
            batch.create_vertex(
                VId(id),
                vec![LabelId(1)],
                vec![
                    (
                        PropertyKeyId(1),
                        CanonicalScalar::ucs_basic_text("graph").unwrap(),
                    ),
                    (PropertyKeyId(2), CanonicalScalar::Int(id as i64)),
                ],
            );
        }
        batch.add_edge(EId(1), VId(1), VId(2), vec![]);
        if hidden {
            batch.create_vertex(
                VId(90),
                vec![LabelId(99)],
                vec![
                    (
                        PropertyKeyId(1),
                        CanonicalScalar::ucs_basic_text("graph graph").unwrap(),
                    ),
                    (PropertyKeyId(2), CanonicalScalar::Int(0)),
                ],
            );
            batch.add_edge(EId(2), VId(1), VId(90), vec![]);
            batch.add_edge(EId(3), VId(90), VId(3), vec![]);
        }
        db.write(cx, batch).await.unwrap();
        if hidden {
            let mut shortcut = WriteBatch::new(RelationId(99));
            shortcut.add_edge(EId(4), VId(1), VId(3), vec![]);
            db.write(cx, shortcut).await.unwrap();
        }
        db
    }

    #[test]
    fn hidden_transit_and_forbidden_shortcuts_match_physically_removed_graph() {
        let ((), report) = run_async_under_lab(0xbeac_2201, |root| async move {
            let c = PurposeContexts::narrow_runtime_root(&root);
            let full = graph(&c.commit(), true).await;
            let clean = graph(&c.commit(), false).await;
            let authority = Authority::new(
                AuthKey::from_seed(2201),
                NS,
                "host-graph",
                SchemaEpoch(0),
                1,
            )
            .unwrap();
            let mut grant = Grant::read_only(
                "main",
                1000,
                QueryLimits {
                    max_nodes: 3,
                    max_work: 1_000_000,
                    max_rows: 3,
                },
            );
            grant.labels = Scope::only([LabelId(1)]);
            grant.relations = Scope::only([RelationId(1)]);
            grant.properties = Scope::only([PropertyKeyId(1), PropertyKeyId(2)]);
            let token = authority.issue_at(&grant, 100).unwrap();
            let mut options = Options::text(PropertyKeyId(1));
            options.projection.vector = vec![PropertyKeyId(2)];
            options.index.vector = Some(HnswConfig::new(1, DistanceMetric::SquaredEuclidean));
            let query = GraphHybridQuery {
                retrieval: ExactHybridQuery {
                    vector: &[0.0],
                    text: "graph",
                    k: 3,
                    vector_candidates: 3,
                    text_candidates: 3,
                    vector_mode: VectorSearch::Exact,
                    text_mode: TextMatch::Any,
                    profile: ExactRrfProfile::default(),
                },
                graph_candidates: 8,
                graph_weight: 100,
            };
            let expansion = ExpansionSpec {
                seeds: &[VId(1)],
                relation: None,
                direction: ExpansionDirection::Outgoing,
                max_hops: 2,
                include_seeds: false,
                limits: ExpansionLimits::default(),
            };
            let actual = full
                .beacon_search_graph_authorized(
                    &c.query(),
                    &authority,
                    &token,
                    "main",
                    &options,
                    query,
                    expansion,
                    || 100,
                )
                .unwrap();
            let expected = clean
                .beacon_search_graph(&c.query(), &options, query, expansion)
                .unwrap();
            assert_eq!(actual, expected); // Includes all scores, ranks and hop metadata.
            assert_eq!(
                actual
                    .iter()
                    .find(|hit| hit.id == VId(2))
                    .unwrap()
                    .graph_hops,
                Some(1)
            );
            assert_eq!(
                actual
                    .iter()
                    .find(|hit| hit.id == VId(3))
                    .unwrap()
                    .graph_hops,
                None
            );
            assert!(actual.iter().all(|hit| hit.id != VId(90)));
            assert_ne!(
                actual,
                full.beacon_search_graph(&c.query(), &options, query, expansion)
                    .unwrap(),
                "the hidden graph must change an unscoped query, not be an inert fixture",
            );
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }
}
