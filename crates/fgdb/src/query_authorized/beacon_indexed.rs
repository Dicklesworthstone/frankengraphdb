//! Capability-scoped indexed graph retrieval. Authorization still owns corpus
//! construction, traversal admission, the live allowance and final delivery.

use super::{authorized_with_errors, control_error};
use crate::query::beacon::{self, Meter, Options};
use crate::{Database, QueryError, ReadError};
use asupersync::fs::Vfs;
use fgdb_beacon::expansion::ExpansionSpec;
use fgdb_beacon::read::ReadError as BeaconReadError;
use fgdb_beacon::{GraphHybridHit, GraphHybridQuery};
use fgdb_delta_types::RelationId;
use fgdb_types::QueryCx;
use fgdb_warden::{Authority, CapabilityToken, Error as WardenError, LimitDimension};
use std::cell::RefCell;

impl<V: Vfs + Clone> Database<V> {
    /// Capability-visible graph/text/vector retrieval with indexed expansion.
    ///
    /// Shares the ordinary authorized execution owner: signature, namespace,
    /// branch, rights, expiry and retirement checks precede source admission;
    /// one live Warden permit spans all modalities and final row release.
    /// Historical vertex winners are selected before original-label tests;
    /// property masks precede tokenization, vector validation, BM25 and ANN.
    /// The graph source resolves each incident edge's exact-cut winner, then
    /// admits its relation and BOTH selected endpoints before the BFS sees it.
    /// Hidden transit vertices, hidden seeds and forbidden shortcut relations
    /// cannot alter the visible candidate population, scores or hop distances.
    ///
    /// Only reached neighborhoods below the hop bound request edge histories.
    /// Hidden history and private index traversal poll cancellation without
    /// charging signed/native work or incidence/scratch limits. Visible vertex
    /// admissions, selected corpus construction, admitted incidence visits,
    /// BFS, searches and fusion share one cumulative native/signed allowance.
    /// Signed nodes still count vertex admissions, not edges. Requested k must
    /// fit both row ceilings; actual rows are charged once at final delivery.
    /// Even an empty result rechecks the live permit before it can escape.
    /// No raw source, selected graph, index, permit or private statistics escape.
    ///
    /// The explicit indexed profile's max_input_edges and max_source_scratch
    /// bound ADMITTED INCIDENCE VISITS, not the entire graph. Parallel EIds
    /// count separately, a self-loop once per anchor, an undirected edge up to
    /// once per expanded endpoint. A failed bound returns no partial result.
    /// Text/vector corpus construction, decoded storage and visited state are
    /// still resident; no spill or timing/I/O-isolation guarantee is made.
    /// ANN and candidate-limited fusion retain their approximation boundaries.
    /// The trusted host owns Authority, database/branch routing and the clock;
    /// keep the raw Database APIs and Authority out of bearer-holder reach.
    #[allow(clippy::too_many_arguments)]
    pub fn beacon_search_graph_indexed_authorized(
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
                let mut poll = || {
                    let polled = execution.borrow_mut().poll();
                    polled.map_err(|error| work.borrow_mut().refuse(error))
                };
                let result = beacon::indexed::evaluate(
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
                    || Ok(()),
                );
                work.into_inner().finish::<ReadError, _>(result)
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DatabaseKeys, MemVfs, WriteBatch};
    use asupersync::lab::run_async_under_lab;
    use asupersync::security::key::AuthKey;
    use fgdb_beacon::expansion::{ExpansionDirection, ExpansionLimits};
    use fgdb_beacon::{
        BeaconError, DistanceMetric, ExactHybridQuery, ExactRrfProfile, HnswConfig, TextMatch,
        VectorSearch,
    };
    use fgdb_delta_types::{LabelId, PropertyKeyId, SchemaEpoch};
    use fgdb_types::{
        CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId,
    };
    use fgdb_warden::{Grant, QueryLimits, Scope};
    use std::cell::Cell;

    const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0xc2; 32]);
    const R: RelationId = RelationId(1);

    fn authority() -> Authority {
        Authority::new(
            AuthKey::from_seed(3201),
            NS,
            "host-graph",
            SchemaEpoch(0),
            1,
        )
        .unwrap()
    }
    fn grant() -> Grant {
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
        grant.relations = Scope::only([R]);
        grant.properties = Scope::only([PropertyKeyId(1), PropertyKeyId(2)]);
        grant
    }
    fn options() -> Options {
        let mut options = Options::text(PropertyKeyId(1));
        options.projection.vector = vec![PropertyKeyId(2)];
        options.index.vector = Some(HnswConfig::new(1, DistanceMetric::SquaredEuclidean));
        options
    }
    fn query() -> GraphHybridQuery<'static> {
        GraphHybridQuery {
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
            graph_candidates: 3,
            graph_weight: 100,
        }
    }
    fn expansion() -> ExpansionSpec<'static, RelationId> {
        ExpansionSpec {
            seeds: &[VId(1), VId(90), VId(999)],
            relation: None,
            direction: ExpansionDirection::Outgoing,
            max_hops: 2,
            include_seeds: false,
            limits: ExpansionLimits::default(),
        }
    }
    async fn graph(cx: &CommitCx, hidden: bool) -> Database<MemVfs> {
        let mut db = Database::open_memory(cx, DatabaseKeys::new([0xc1; 32], NS, [0xc3; 32]))
            .await
            .unwrap();
        let mut batch = WriteBatch::new(R);
        for id in 1..=3 {
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
        batch.add_edge(EId(2), VId(2), VId(3), vec![]);
        if hidden {
            batch.create_vertex(
                VId(90),
                vec![LabelId(99)],
                vec![
                    (
                        PropertyKeyId(1),
                        CanonicalScalar::ucs_basic_text("graph graph secret").unwrap(),
                    ),
                    (PropertyKeyId(2), CanonicalScalar::Int(0)),
                ],
            );
            batch.add_edge(EId(3), VId(1), VId(90), vec![]);
            batch.add_edge(EId(4), VId(90), VId(3), vec![]);
            for id in 100..132 {
                batch.add_edge(EId(id), VId(1), VId(90), vec![]);
            }
        }
        db.write(cx, batch).await.unwrap();
        if hidden {
            let mut shortcut = WriteBatch::new(RelationId(99));
            shortcut.add_edge(EId(5), VId(1), VId(3), vec![]);
            db.write(cx, shortcut).await.unwrap();
        }
        db
    }
    fn assert_authorization<T>(
        result: Result<T, BeaconReadError<ReadError, QueryError>>,
        expected: WardenError,
    ) {
        assert!(matches!(result,
            Err(BeaconReadError::Interrupted(QueryError::Authorization(error))) if error == expected)); // ubs:ignore -- a public WardenError authorization verdict in a test helper; no secret, token or MAC is compared here.
    }

    #[test]
    fn hidden_neighbors_and_shortcuts_match_physically_removed_corpus_in_every_direction() {
        let ((), report) = run_async_under_lab(0xbeac_3201, |root| async move {
            let c = PurposeContexts::narrow_runtime_root(&root);
            let full = graph(&c.commit(), true).await;
            let clean = graph(&c.commit(), false).await;
            let authority = authority();
            let token = authority.issue_at(&grant(), 100).unwrap();
            for direction in [
                ExpansionDirection::Outgoing,
                ExpansionDirection::Incoming,
                ExpansionDirection::Undirected,
            ] {
                for include in [false, true] {
                    let mut expansion = expansion();
                    expansion.direction = direction;
                    expansion.include_seeds = include;
                    let actual = full
                        .beacon_search_graph_indexed_authorized(
                            &c.query(),
                            &authority,
                            &token,
                            "main",
                            &options(),
                            query(),
                            expansion,
                            || 100,
                        )
                        .unwrap();
                    assert_eq!(
                        actual,
                        clean
                            .beacon_search_graph(&c.query(), &options(), query(), expansion,)
                            .unwrap()
                    );
                    assert_eq!(
                        actual,
                        full.beacon_search_graph_authorized(
                            &c.query(),
                            &authority,
                            &token,
                            "main",
                            &options(),
                            query(),
                            expansion,
                            || 100,
                        )
                        .unwrap()
                    );
                    assert!(actual.iter().all(|hit| hit.id != VId(90)));
                }
            }
            let actual = full
                .beacon_search_graph_indexed_authorized(
                    &c.query(),
                    &authority,
                    &token,
                    "main",
                    &options(),
                    query(),
                    expansion(),
                    || 100,
                )
                .unwrap();
            assert_eq!(
                actual
                    .iter()
                    .find(|hit| hit.id == VId(3))
                    .unwrap()
                    .graph_hops,
                Some(2)
            );
            assert_ne!(
                actual,
                full.beacon_search_graph_indexed(&c.query(), &options(), query(), expansion(),)
                    .unwrap(),
                "the unscoped hidden shortcuts must affect the fixture"
            );
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn hidden_index_history_changes_neither_observable_work_nor_incidence_admission() {
        let ((), report) = run_async_under_lab(0xbeac_3202, |root| async move {
            let c = PurposeContexts::narrow_runtime_root(&root);
            let full = graph(&c.commit(), true).await;
            let clean = graph(&c.commit(), false).await;
            let authority = authority();
            let token = authority.issue_at(&grant(), 100).unwrap();
            let mut expansion = expansion();
            expansion.limits.max_input_edges = 2;
            expansion.limits.max_source_scratch = 2;
            let calls = Cell::new(0usize);
            let run = |db: &Database<MemVfs>, limit: usize| {
                let mut options = options();
                options.policy.max_work_units = limit;
                db.beacon_search_graph_indexed_authorized(
                    &c.query(),
                    &authority,
                    &token,
                    "main",
                    &options,
                    query(),
                    expansion,
                    || {
                        calls.set(calls.get() + 1);
                        100
                    },
                )
            };
            let expected = run(&clean, 100_000).unwrap();
            let clean_calls = calls.replace(0);
            assert_eq!(run(&full, 100_000).unwrap(), expected);
            assert_eq!(calls.replace(0), clean_calls);
            // Measure the native minimum independently for each physical graph.
            // The hidden histories must not change where the search refuses.
            let minimum = |db: &Database<MemVfs>| {
                let (mut low, mut high) = (0, 100_000);
                while low < high {
                    let mid = low + (high - low) / 2;
                    if run(db, mid).is_ok() {
                        high = mid;
                    } else {
                        low = mid + 1;
                    }
                }
                low
            };
            let needed = minimum(&clean);
            assert!(needed > 0);
            assert_eq!(minimum(&full), needed);
            for db in [&clean, &full] {
                assert_eq!(run(db, needed).unwrap(), expected);
                assert!(matches!(
                    run(db, needed - 1),
                    Err(BeaconReadError::Index(_))
                ));
            }
            expansion.limits.max_input_edges = 1;
            for db in [&clean, &full] {
                assert!(matches!(
                    db.beacon_search_graph_indexed_authorized(
                        &c.query(),
                        &authority,
                        &token,
                        "main",
                        &options(),
                        query(),
                        expansion,
                        || 100,
                    ),
                    Err(BeaconReadError::Index(BeaconError::ResourceLimit {
                        resource: "indexed expansion incidence visits",
                        limit: 1,
                    }))
                ));
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn hidden_successor_vertex_blocks_transit_but_exact_history_still_replays() {
        let ((), report) = run_async_under_lab(0xbeac_3203, |root| async move {
            let c = PurposeContexts::narrow_runtime_root(&root);
            let mut db = graph(&c.commit(), false).await;
            let authority = authority();
            let token = authority.issue_at(&grant(), 100).unwrap();
            let before = db.frontier().unwrap();
            let original = db
                .beacon_search_graph_indexed_authorized(
                    &c.query(),
                    &authority,
                    &token,
                    "main",
                    &options(),
                    query(),
                    expansion(),
                    || 100,
                )
                .unwrap();
            let mut hide = WriteBatch::new(R);
            hide.set_vertex_label(VId(2), LabelId(1), false);
            hide.set_vertex_label(VId(2), LabelId(99), true);
            db.write(&c.commit(), hide).await.unwrap();
            let current = db
                .beacon_search_graph_indexed_authorized(
                    &c.query(),
                    &authority,
                    &token,
                    "main",
                    &options(),
                    query(),
                    expansion(),
                    || 100,
                )
                .unwrap();
            assert!(current.iter().all(|hit| hit.id != VId(2)));
            assert_eq!(
                current
                    .iter()
                    .find(|hit| hit.id == VId(3))
                    .unwrap()
                    .graph_hops,
                None
            );
            assert_ne!(current, original);
            let mut history = options();
            history.as_of = Some(before);
            assert_eq!(
                db.beacon_search_graph_indexed_authorized(
                    &c.query(),
                    &authority,
                    &token,
                    "main",
                    &history,
                    query(),
                    expansion(),
                    || 100,
                )
                .unwrap(),
                original
            );
            assert_eq!(
                current,
                db.beacon_search_graph_authorized(
                    &c.query(),
                    &authority,
                    &token,
                    "main",
                    &options(),
                    query(),
                    expansion(),
                    || 100,
                )
                .unwrap()
            );
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn forbidden_vector_values_are_absent_before_type_validation_or_scoring() {
        let ((), report) = run_async_under_lab(0xbeac_3204, |root| async move {
            let c = PurposeContexts::narrow_runtime_root(&root);
            let mut db = graph(&c.commit(), false).await;
            let mut clean = graph(&c.commit(), false).await;
            let mut bad = WriteBatch::new(R);
            let mut removed = WriteBatch::new(R);
            for id in 1..=3 {
                bad.set_vertex_property(
                    VId(id),
                    PropertyKeyId(2),
                    Some(CanonicalScalar::ucs_basic_text("not-a-vector-coordinate").unwrap()),
                );
                removed.set_vertex_property(VId(id), PropertyKeyId(2), None);
            }
            db.write(&c.commit(), bad).await.unwrap();
            clean.write(&c.commit(), removed).await.unwrap();
            let authority = authority();
            let mut grant = grant();
            grant.properties = Scope::only([PropertyKeyId(1)]);
            let token = authority.issue_at(&grant, 100).unwrap();
            let actual = db
                .beacon_search_graph_indexed_authorized(
                    &c.query(),
                    &authority,
                    &token,
                    "main",
                    &options(),
                    query(),
                    expansion(),
                    || 100,
                )
                .unwrap();
            assert_eq!(
                actual,
                clean
                    .beacon_search_graph_indexed(&c.query(), &options(), query(), expansion(),)
                    .unwrap()
            );
            assert!(
                db.beacon_search_graph_indexed(&c.query(), &options(), query(), expansion(),)
                    .is_err()
            );
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn expiry_and_retirement_at_every_live_boundary_release_no_result() {
        let ((), report) = run_async_under_lab(0xbeac_3205, |root| async move {
            let c = PurposeContexts::narrow_runtime_root(&root);
            let db = graph(&c.commit(), true).await;
            let issuer = authority();
            let token = issuer.issue_at(&grant(), 100).unwrap();
            let mut total = 0;
            db.beacon_search_graph_indexed_authorized(
                &c.query(),
                &issuer,
                &token,
                "main",
                &options(),
                query(),
                expansion(),
                || {
                    total += 1;
                    100
                },
            )
            .unwrap();
            assert!(total > 10);
            for retire in [false, true] {
                for stop in 1..=total {
                    let issuer = authority();
                    let token = issuer.issue_at(&grant(), 100).unwrap();
                    let mut calls = 0;
                    let result = db.beacon_search_graph_indexed_authorized(
                        &c.query(),
                        &issuer,
                        &token,
                        "main",
                        &options(),
                        query(),
                        expansion(),
                        || {
                            calls += 1;
                            if calls >= stop {
                                if retire {
                                    issuer.retire();
                                    100
                                } else {
                                    1001
                                }
                            } else {
                                100
                            }
                        },
                    );
                    assert_authorization(
                        result,
                        if retire {
                            WardenError::AuthorityRetired
                        } else {
                            WardenError::Expired
                        },
                    );
                    assert!(calls >= stop);
                }
            }
            let mut calls = 0;
            assert_authorization(
                db.beacon_search_graph_indexed_authorized(
                    &c.query(),
                    &issuer,
                    &token,
                    "main",
                    &options(),
                    query(),
                    expansion(),
                    || {
                        calls += 1;
                        if calls == total { 99 } else { 100 }
                    },
                ),
                WardenError::ClockWentBackwards,
            );
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn signed_limits_and_empty_results_still_cross_the_live_authority_boundary() {
        let ((), report) = run_async_under_lab(0xbeac_3206, |root| async move {
            let c = PurposeContexts::narrow_runtime_root(&root);
            let db = graph(&c.commit(), true).await;
            let issuer = authority();
            for (dimension, limit) in [
                (LimitDimension::Nodes, 2),
                (LimitDimension::Rows, 2),
                (LimitDimension::Work, 0),
            ] {
                let mut grant = grant();
                match dimension {
                    LimitDimension::Nodes => grant.limits.max_nodes = limit,
                    LimitDimension::Rows => grant.limits.max_rows = limit,
                    LimitDimension::Work => grant.limits.max_work = limit,
                }
                let token = issuer.issue_at(&grant, 100).unwrap();
                assert_authorization(
                    db.beacon_search_graph_indexed_authorized(
                        &c.query(),
                        &issuer,
                        &token,
                        "main",
                        &options(),
                        query(),
                        expansion(),
                        || 100,
                    ),
                    WardenError::LimitExceeded(dimension),
                );
            }
            let token = issuer.issue_at(&grant(), 100).unwrap();
            let mut query = query();
            query.retrieval.k = 0;
            assert_authorization(
                db.beacon_search_graph_indexed_authorized(
                    &c.query(),
                    &issuer,
                    &token,
                    "main",
                    &options(),
                    query,
                    expansion(),
                    || 1001,
                ),
                WardenError::Expired,
            );
            query.graph_weight = 0;
            query.graph_candidates = 0;
            let mut expansion = expansion();
            expansion.limits = ExpansionLimits {
                max_vertices: 0,
                max_input_edges: 0,
                max_visited_vertices: 0,
                max_seed_ids: 0,
                max_source_scratch: 0,
            };
            assert!(
                db.beacon_search_graph_indexed_authorized(
                    &c.query(),
                    &issuer,
                    &token,
                    "main",
                    &options(),
                    query,
                    expansion,
                    || 100,
                )
                .unwrap()
                .is_empty()
            );
            issuer.retire();
            assert_authorization(
                db.beacon_search_graph_indexed_authorized(
                    &c.query(),
                    &issuer,
                    &token,
                    "main",
                    &options(),
                    query,
                    expansion,
                    || 100,
                ),
                WardenError::AuthorityRetired,
            );
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn each_native_work_control_refusal_remains_typed_without_partial_rows() {
        let ((), report) = run_async_under_lab(0xbeac_3207, |root| async move {
            let c = PurposeContexts::narrow_runtime_root(&root);
            let db = graph(&c.commit(), false).await;
            let before = db.frontier().unwrap();
            let run = |stop: usize| {
                let mut calls = 0;
                let work = RefCell::new(Meter::new(1_000_000, |_| {
                    calls += 1;
                    if calls == stop { Err(stop) } else { Ok(()) }
                }));
                let result = beacon::indexed::evaluate(
                    &db.snapshot,
                    before,
                    &options(),
                    query(),
                    expansion(),
                    &work,
                    beacon::Scan::Metered,
                    |_| Ok(true),
                    |_| true,
                    |_| true,
                    |_| true,
                    || Ok(()),
                );
                let result = work.into_inner().finish::<ReadError, _>(result);
                (result, calls)
            };
            let (success, total) = run(usize::MAX);
            assert_eq!(success.unwrap().len(), 3);
            assert!(total > 10);
            for stop in 1..=total {
                let (result, calls) = run(stop);
                assert!(matches!(result, Err(BeaconReadError::Interrupted(at)) if at == stop));
                assert_eq!(calls, stop);
                assert_eq!(db.frontier().unwrap(), before);
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }
}
