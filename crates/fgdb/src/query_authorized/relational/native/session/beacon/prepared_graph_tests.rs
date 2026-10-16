use super::*;
use crate::{Database, DatabaseKeys, MemVfs, WriteBatch};
use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb_beacon::expansion::ExpansionLimits;
use fgdb_beacon::{
    DistanceMetric, ExactHybridQuery, ExactRrfProfile, HnswConfig, TextMatch, VectorSearch,
};
use fgdb_delta_types::{PropertyKeyId, SchemaEpoch};
use fgdb_gql::{GraphSymbol, GraphSymbolKind};
use fgdb_types::context::SimulationCheckpointProbe;
use fgdb_types::{CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, EId, PurposeContexts};
use fgdb_warden::{Authority, Grant, QueryLimits, Scope};
use std::cell::Cell;

const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x72; 32]);
const L: LabelId = LabelId(1);
const HIDDEN: LabelId = LabelId(99);
const R: RelationId = RelationId(1);
const T: PropertyKeyId = PropertyKeyId(1);
const X: PropertyKeyId = PropertyKeyId(2);
const HIGH: VId = VId(u128::MAX);
const NOW: u64 = 100;
const EXPIRES: u64 = 10_000;

fn authority() -> Authority {
    Authority::new(
        AuthKey::from_seed(0x7251),
        NS,
        "graph-reuse",
        SchemaEpoch(0),
        1,
    )
    .unwrap()
}
fn grant() -> Grant {
    let mut grant = Grant::read_only(
        "main",
        EXPIRES,
        QueryLimits {
            max_nodes: 4,
            max_work: 100_000_000,
            max_rows: 8,
        },
    );
    grant.labels = Scope::only([L]);
    grant.properties = Scope::only([T, X]);
    grant.relations = Scope::only([R]);
    grant
}
fn host() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 8, 10_000_000, 10_000)
}
fn catalog(_: GraphSymbolKind, _: &str) -> Option<GraphSymbol> {
    None
}
fn options() -> Options {
    let mut options = Options::text(T);
    options.vertex_label = Some(L);
    options.projection.vector = vec![X];
    options.index.vector = Some(HnswConfig::new(1, DistanceMetric::SquaredEuclidean));
    options
}
fn query() -> GraphHybridQuery<'static> {
    GraphHybridQuery {
        retrieval: ExactHybridQuery {
            vector: &[0.0],
            text: "graph",
            k: 1,
            vector_candidates: 1,
            text_candidates: 1,
            vector_mode: VectorSearch::Exact,
            text_mode: TextMatch::Any,
            profile: ExactRrfProfile::default(),
        },
        graph_candidates: 1,
        graph_weight: 100,
    }
}
fn expansion() -> ExpansionSpec<'static, RelationId> {
    ExpansionSpec {
        seeds: &[VId(0)],
        relation: Some(R),
        direction: ExpansionDirection::Outgoing,
        max_hops: 2,
        include_seeds: false,
        limits: ExpansionLimits::default(),
    }
}
fn no_staging() -> ReadPolicy {
    ReadPolicy {
        max_staging_rows: 0,
        ..ReadPolicy::default()
    }
}
async fn fixture(cx: &CommitCx, hidden: bool, disconnected: usize) -> Database<MemVfs> {
    let mut db = Database::open_memory(cx, DatabaseKeys::new([0x71; 32], NS, [0x73; 32]))
        .await
        .unwrap();
    let mut batch = WriteBatch::new(R);
    for (id, x) in [(VId(0), 0), (HIGH, 9), (VId(9), 1)] {
        batch.create_vertex(
            id,
            vec![L],
            vec![
                (T, CanonicalScalar::ucs_basic_text("graph").unwrap()),
                (X, CanonicalScalar::Int(x)),
            ],
        );
    }
    // Graph membership cannot be inferred from the set of indexed documents.
    batch.create_vertex(VId(1), vec![L], vec![]);
    for (eid, src, dst) in [
        (0, VId(0), VId(1)),
        (1, VId(0), VId(1)),
        (2, VId(1), HIGH),
        (3, HIGH, VId(0)),
        (4, VId(1), VId(1)),
    ] {
        batch.add_edge(EId(eid), src, dst, vec![]);
    }
    for eid in 0..disconnected {
        batch.add_edge(EId(1000 + eid as u128), VId(9), VId(9), vec![]);
    }
    if hidden {
        batch.create_vertex(
            VId(90),
            vec![HIDDEN],
            vec![
                (T, CanonicalScalar::Int(123)),
                (
                    X,
                    CanonicalScalar::ucs_basic_text("invalid hidden vector").unwrap(),
                ),
            ],
        );
        // Both edge orientations exercise masking before charging endpoints.
        for (eid, src, dst) in [
            (10, VId(0), VId(90)),
            (11, VId(90), HIGH),
            (12, VId(90), VId(0)),
            (13, HIGH, VId(90)),
        ] {
            batch.add_edge(EId(eid), src, dst, vec![]);
        }
    }
    db.write(cx, batch).await.unwrap();
    if hidden {
        let mut forbidden = WriteBatch::new(RelationId(99));
        forbidden.add_edge(EId(u128::MAX), VId(0), HIGH, vec![]);
        db.write(cx, forbidden).await.unwrap();
    }
    db
}
fn denied(result: Result<Vec<GraphHybridHit>, Error>, expected: WardenError) {
    assert!(
        matches!(&result,
        Err(Error::Interrupted(QueryError::Authorization(error))) if *error == expected),
        "expected {expected:?}, got {result:?}"
    );
}

#[test]
fn prepared_graph_fusion_reuses_all_lanes_and_keeps_propertyless_transit() {
    let ((), report) = run_async_under_lab(0xbeac_5701, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let db = fixture(&c.commit(), true, 128).await;
        let clean = fixture(&c.commit(), false, 128).await;
        let issuer = authority();
        let token = issuer.issue_at(&grant(), NOW).unwrap();
        let mut session = db
            .authorized_read_session(&c.query(), &issuer, &token, "main", catalog, host(), || NOW)
            .unwrap();
        let prepared = session
            .prepare_beacon_index(&c.query(), &options())
            .unwrap();
        let copy = prepared.clone();
        for (vector, text) in [(1, 1), (1, 0), (0, 1), (0, 0)] {
            let mut q = query();
            if vector == 0 && text == 0 {
                q = GraphHybridQuery::graph_only(1, 1, 100, 60).unwrap();
            } else {
                q.retrieval.profile = ExactRrfProfile::new(60, vector, text).unwrap();
            }
            for direction in [
                ExpansionDirection::Outgoing,
                ExpansionDirection::Incoming,
                ExpansionDirection::Undirected,
            ] {
                let mut e = expansion();
                e.direction = direction;
                e.relation = None; // the forbidden shortcut is still excluded
                let expected = clean
                    .beacon_search_graph_filtered(&c.query(), &options(), q, e, |id| id == HIGH)
                    .unwrap();
                let actual = session
                    .search_beacon_graph_index_filtered(
                        &c.query(),
                        &copy,
                        q,
                        e,
                        no_staging(),
                        &[VId(90), HIGH],
                    )
                    .unwrap();
                assert_eq!(actual, expected);
                assert_eq!(actual.len(), 1);
                assert_eq!(actual[0].id, HIGH);
                assert_eq!(actual[0].graph_rank.unwrap().get(), 1);
                assert_eq!(
                    actual[0].graph_hops,
                    Some(if direction == ExpansionDirection::Outgoing {
                        2
                    } else {
                        1
                    })
                );
                assert_eq!(
                    actual[0].vector_rank.map(|rank| rank.get()),
                    (vector != 0).then_some(1)
                );
                assert_eq!(
                    actual[0].text_rank.map(|rank| rank.get()),
                    (text != 0).then_some(1)
                );
            }
        }
        let actual = session
            .search_beacon_graph_index_filtered(
                &c.query(),
                &prepared,
                query(),
                expansion(),
                no_staging(),
                &[HIGH],
            )
            .unwrap();
        assert_eq!(
            (actual[0].score.numerator(), actual[0].score.denominator()),
            (102, 61)
        );
        let graph = GraphHybridQuery::graph_only(1, 1, 100, 60).unwrap();
        let transit = session
            .search_beacon_graph_index_filtered(
                &c.query(),
                &prepared,
                graph,
                expansion(),
                no_staging(),
                &[VId(1)],
            )
            .unwrap();
        assert_eq!(transit[0].id, VId(1));
        assert_eq!(transit[0].graph_hops, Some(1));
        // An ANN request reuses exactly the prepared topology too.
        let mut ann = query();
        ann.retrieval.vector_mode = VectorSearch::Approximate { ef_search: 32 };
        assert_eq!(
            session
                .search_beacon_graph_index_filtered(
                    &c.query(),
                    &prepared,
                    ann,
                    expansion(),
                    no_staging(),
                    &[HIGH],
                )
                .unwrap(),
            actual
        );
        assert!(
            session
                .search_beacon_graph_index_filtered(
                    &c.query(),
                    &prepared,
                    query(),
                    expansion(),
                    no_staging(),
                    &[],
                )
                .unwrap()
                .is_empty()
        );
        assert_eq!(format!("{prepared:?}"), "AuthorizedBeaconIndex([REDACTED])");
        session.close();
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn reused_graph_reads_the_prepared_history_not_the_session_or_writer_head() {
    let ((), report) = run_async_under_lab(0xbeac_5702, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let mut db = fixture(&c.commit(), false, 0).await;
        let first = db.frontier().unwrap();
        let before = db
            .beacon_search_graph_filtered(&c.query(), &options(), query(), expansion(), |id| {
                id == HIGH
            })
            .unwrap();
        let mut change = WriteBatch::new(R);
        change.delete_vertex(VId(1));
        change.set_vertex_property(HIGH, X, Some(CanonicalScalar::Int(20)));
        db.write(&c.commit(), change).await.unwrap();
        let second = db.frontier().unwrap();
        let issuer = authority();
        let token = issuer.issue_at(&grant(), NOW).unwrap();
        let mut session = db
            .authorized_read_session(&c.query(), &issuer, &token, "main", catalog, host(), || NOW)
            .unwrap();
        let mut historic = options();
        historic.as_of = Some(first);
        let old = session.prepare_beacon_index(&c.query(), &historic).unwrap();
        let new = session
            .prepare_beacon_index(&c.query(), &options())
            .unwrap();
        assert_eq!(old.source_sequence(), first);
        assert_eq!(new.source_sequence(), second);
        let after = session
            .search_beacon_graph_index_filtered(
                &c.query(),
                &new,
                query(),
                expansion(),
                no_staging(),
                &[HIGH],
            )
            .unwrap();
        assert_eq!(after[0].graph_hops, None);
        assert_ne!(before, after);
        historic.vertex_label = Some(HIDDEN);
        historic.projection.vector.clear();
        db.compact(&c.commit()).await.unwrap();
        drop(db);
        assert_eq!(
            session
                .search_beacon_graph_index_filtered(
                    &c.query(),
                    &old,
                    query(),
                    expansion(),
                    no_staging(),
                    &[HIGH],
                )
                .unwrap(),
            before
        );
        assert_eq!(
            session
                .search_beacon_graph_index_filtered(
                    &c.query(),
                    &new,
                    query(),
                    expansion(),
                    no_staging(),
                    &[HIGH],
                )
                .unwrap(),
            after
        );
        session.close();
        denied(
            session.search_beacon_graph_index(&c.query(), &old, query(), expansion(), no_staging()),
            WardenError::ExecutionStopped,
        );
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn prepared_graph_limits_count_reached_incidence_and_cannot_be_evaded_by_filtering() {
    let ((), report) = run_async_under_lab(0xbeac_5703, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let db = fixture(&c.commit(), true, 128).await;
        let issuer = authority();
        let token = issuer.issue_at(&grant(), NOW).unwrap();
        let mut session = db
            .authorized_read_session(&c.query(), &issuer, &token, "main", catalog, host(), || NOW)
            .unwrap();
        let prepared = session
            .prepare_beacon_index(&c.query(), &options())
            .unwrap();
        let mut e = expansion();
        e.limits.max_input_edges = 4; // parallel 0->1 twice, 1->HIGH, 1->1
        e.limits.max_source_scratch = 64;
        let expected = session
            .search_beacon_graph_index_filtered(
                &c.query(),
                &prepared,
                query(),
                e,
                no_staging(),
                &[HIGH],
            )
            .unwrap();
        assert_eq!(expected[0].graph_hops, Some(2));
        for allowed in [&[HIGH][..], &[][..]] {
            let mut low = e;
            low.limits.max_input_edges = 3;
            assert!(matches!(
                session.search_beacon_graph_index_filtered(
                    &c.query(),
                    &prepared,
                    query(),
                    low,
                    no_staging(),
                    allowed,
                ),
                Err(Error::Index(BeaconError::ResourceLimit {
                    resource: "prepared graph incidence visits",
                    limit: 3
                }))
            ));
            low = e;
            low.limits.max_source_scratch = 3;
            // Three entries cannot hold the seeds' and kernel's state, which
            // the shared allowance (5c4b4bac) reserves before the reached
            // incidences, whatever the filter.
            assert!(matches!(
                session.search_beacon_graph_index_filtered(
                    &c.query(),
                    &prepared,
                    query(),
                    low,
                    no_staging(),
                    allowed,
                ),
                Err(Error::Index(BeaconError::ResourceLimit {
                    resource: "prepared graph scratch entries",
                    limit: 3
                }))
            ));
            low = e;
            low.limits.max_visited_vertices = 2;
            assert!(
                session
                    .search_beacon_graph_index_filtered(
                        &c.query(),
                        &prepared,
                        query(),
                        low,
                        no_staging(),
                        allowed,
                    )
                    .is_err()
            );
        }
        for allowed in [&[HIGH, VId(0)][..], &[HIGH, HIGH][..]] {
            assert!(matches!(
                session.search_beacon_graph_index_filtered(
                    &c.query(),
                    &prepared,
                    query(),
                    e,
                    no_staging(),
                    allowed,
                ),
                Err(Error::Index(BeaconError::InvalidQuery(_)))
            ));
        }
        let mut bad = query();
        bad.retrieval.vector = &[0.0, 1.0];
        assert!(matches!(
            session.search_beacon_graph_index_filtered(
                &c.query(),
                &prepared,
                bad,
                e,
                no_staging(),
                &[],
            ),
            Err(Error::Index(BeaconError::Dimension { .. }))
        ));
        assert!(matches!(
            session.search_beacon_graph_index_filtered(
                &c.query(),
                &prepared,
                query(),
                e,
                ReadPolicy {
                    max_work_units: 0,
                    ..no_staging()
                },
                &[],
            ),
            Err(Error::Index(BeaconError::WorkBudgetExceeded))
        ));
        let mut frozen = options();
        frozen.policy.max_result_rows = 0;
        let frozen = session.prepare_beacon_index(&c.query(), &frozen).unwrap();
        assert!(matches!(
            session.search_beacon_graph_index_filtered(
                &c.query(),
                &frozen,
                query(),
                e,
                no_staging(),
                &[],
            ),
            Err(Error::Index(BeaconError::ResourceLimit {
                resource: "result rows",
                limit: 0
            }))
        ));
        let mut zero = expansion();
        zero.seeds = &[VId(9), HIGH, HIGH, VId(999)];
        zero.include_seeds = true;
        zero.max_hops = 0;
        zero.limits.max_input_edges = 0;
        zero.limits.max_source_scratch = 0;
        let only = GraphHybridQuery::graph_only(2, 2, 1, 60).unwrap();
        assert!(matches!(
            session.search_beacon_graph_index(&c.query(), &prepared, only, zero, no_staging()),
            Err(Error::Index(BeaconError::ResourceLimit {
                resource: "prepared graph scratch entries",
                limit: 0
            }))
        ));
        zero.limits.max_source_scratch = 64;
        let rows = session
            .search_beacon_graph_index(&c.query(), &prepared, only, zero, no_staging())
            .unwrap();
        assert_eq!(
            rows.iter()
                .map(|row| (row.id, row.graph_hops))
                .collect::<Vec<_>>(),
            [(VId(9), Some(0)), (HIGH, Some(0))]
        );
        let mut disabled = query();
        disabled.graph_weight = 0;
        disabled.graph_candidates = 0;
        zero.limits.max_vertices = 0;
        zero.limits.max_seed_ids = 0;
        assert!(
            session
                .search_beacon_graph_index(
                    &c.query(),
                    &prepared,
                    disabled,
                    zero,
                    ReadPolicy {
                        max_source_scratch: 0,
                        ..no_staging()
                    },
                )
                .is_ok()
        );
        assert!(!session.is_closed());
        session.close();
        // Four saved vertex admissions plus four reached incidence visits.
        let mut limited = db
            .authorized_read_session(
                &c.query(),
                &issuer,
                &token,
                "main",
                catalog,
                GqlQueryPolicy::new(7, 8, 10_000_000, 10_000),
                || NOW,
            )
            .unwrap();
        let prepared = limited
            .prepare_beacon_index(&c.query(), &options())
            .unwrap();
        assert!(matches!(limited.search_beacon_graph_index_filtered(
            &c.query(), &prepared, query(), expansion(), no_staging(), &[],
        ), Err(Error::Interrupted(QueryError::Pattern(GqlQueryError::Rows(error))))
            if error.dimension == GqlBudgetDimension::SnapshotRecords
                && error.limit == 7 && error.observed == 8));
        assert!(!limited.is_closed());
        limited.close();
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn graph_reuse_is_exact_session_bound_and_reauthorizes_before_handle_diagnostics() {
    let ((), report) = run_async_under_lab(0xbeac_5704, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let db = fixture(&c.commit(), true, 0).await;
        let issuer = authority();
        let token = issuer.issue_at(&grant(), NOW).unwrap();
        let mut first = db
            .authorized_read_session(&c.query(), &issuer, &token, "main", catalog, host(), || NOW)
            .unwrap();
        let prepared = first.prepare_beacon_index(&c.query(), &options()).unwrap();
        let mut second = db
            .authorized_read_session(&c.query(), &issuer, &token, "main", catalog, host(), || NOW)
            .unwrap();
        denied(
            second.search_beacon_graph_index(
                &c.query(),
                &prepared,
                query(),
                expansion(),
                no_staging(),
            ),
            WardenError::ScopeDenied,
        );
        assert!(!second.is_closed());
        let mut text_only = options();
        text_only.index.vector = None;
        let own = second.prepare_beacon_index(&c.query(), &text_only).unwrap();
        assert!(matches!(
            second.search_beacon_graph_index(&c.query(), &own, query(), expansion(), no_staging(),),
            Err(Error::Index(BeaconError::Disabled("vector")))
        ));
        let only = GraphHybridQuery::graph_only(1, 1, 1, 60).unwrap();
        assert!(
            second
                .search_beacon_graph_index(&c.query(), &own, only, expansion(), no_staging(),)
                .is_ok()
        );
        issuer.retire();
        denied(
            second.search_beacon_graph_index_filtered(
                &c.query(),
                &prepared,
                query(),
                expansion(),
                no_staging(),
                &[],
            ),
            WardenError::AuthorityRetired,
        );
        assert!(second.is_closed());
        first.close();
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn every_graph_reuse_checkpoint_can_cancel_without_consuming_the_handle() {
    let ((), report) = run_async_under_lab(0xbeac_5705, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let db = fixture(&c.commit(), true, 0).await;
        let issuer = authority();
        let token = issuer.issue_at(&grant(), NOW).unwrap();
        let mut session = db
            .authorized_read_session(&c.query(), &issuer, &token, "main", catalog, host(), || NOW)
            .unwrap();
        let prepared = session
            .prepare_beacon_index(&c.query(), &options())
            .unwrap();
        for (q, ids) in [
            (query(), &[HIGH][..]),
            (query(), &[][..]),
            (
                GraphHybridQuery::graph_only(0, 1, 1, 60).unwrap(),
                &[HIGH][..],
            ),
        ] {
            let probe = Arc::new(SimulationCheckpointProbe::new(None));
            let expected = session
                .search_beacon_graph_index_filtered(
                    &c.query().with_checkpoint_probe(probe.clone()),
                    &prepared,
                    q,
                    expansion(),
                    no_staging(),
                    ids,
                )
                .unwrap();
            let calls = probe.calls();
            assert!(calls > 5);
            for cut in 1..=calls {
                let probe = Arc::new(SimulationCheckpointProbe::new(Some(cut)));
                let result = session.search_beacon_graph_index_filtered(
                    &c.query().with_checkpoint_probe(probe.clone()),
                    &prepared,
                    q,
                    expansion(),
                    no_staging(),
                    ids,
                );
                assert!(
                    matches!(result, Err(Error::Interrupted(_))),
                    "cut={cut}: {result:?}"
                );
                assert!(probe.calls() >= cut);
                assert!(!session.is_closed());
                assert_eq!(
                    session
                        .search_beacon_graph_index_filtered(
                            &c.query(),
                            &prepared,
                            q,
                            expansion(),
                            no_staging(),
                            ids,
                        )
                        .unwrap(),
                    expected
                );
            }
        }
        session.close();
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn live_expiry_during_graph_reuse_closes_even_an_empty_result() {
    let ((), report) = run_async_under_lab(0xbeac_5706, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let db = fixture(&c.commit(), true, 0).await;
        let issuer = authority();
        let token = issuer.issue_at(&grant(), NOW).unwrap();
        for ids in [&[HIGH][..], &[][..]] {
            let seen = Cell::new(0);
            let mut session = db
                .authorized_read_session(
                    &c.query(),
                    &issuer,
                    &token,
                    "main",
                    catalog,
                    host(),
                    || {
                        seen.set(seen.get() + 1);
                        NOW
                    },
                )
                .unwrap();
            let prepared = session
                .prepare_beacon_index(&c.query(), &options())
                .unwrap();
            seen.set(0);
            session
                .search_beacon_graph_index_filtered(
                    &c.query(),
                    &prepared,
                    query(),
                    expansion(),
                    no_staging(),
                    ids,
                )
                .unwrap();
            let calls = seen.get();
            session.close();
            for cut in 0..calls {
                let seen = Cell::new(0);
                let stop = Cell::new(None);
                let mut session = db
                    .authorized_read_session(
                        &c.query(),
                        &issuer,
                        &token,
                        "main",
                        catalog,
                        host(),
                        || {
                            let at = seen.get();
                            seen.set(at + 1);
                            if stop.get().is_some_and(|cut| at >= cut) {
                                EXPIRES
                            } else {
                                NOW
                            }
                        },
                    )
                    .unwrap();
                let prepared = session
                    .prepare_beacon_index(&c.query(), &options())
                    .unwrap();
                seen.set(0);
                stop.set(Some(cut));
                denied(
                    session.search_beacon_graph_index_filtered(
                        &c.query(),
                        &prepared,
                        query(),
                        expansion(),
                        no_staging(),
                        ids,
                    ),
                    WardenError::Expired,
                );
                assert!(session.is_closed(), "cut={cut}");
            }
        }
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

// Determine the native admission threshold through the PUBLIC execution path,
// not a model of the traversal. Every unsuccessful trial must be a work refusal.
fn minimum_work<R: GraphSymbolResolver, C: FnMut() -> u64>(
    session: &mut AuthorizedReadSession<'_, R, C>,
    prepared: &AuthorizedBeaconIndex,
    cx: &QueryCx,
    expansion: ExpansionSpec<'_, RelationId>,
) -> usize {
    let (mut low, mut high) = (0, 1_000_000);
    session
        .search_beacon_graph_index_filtered(
            cx,
            prepared,
            query(),
            expansion,
            ReadPolicy {
                max_work_units: high,
                ..no_staging()
            },
            &[HIGH],
        )
        .unwrap();
    while low < high {
        let middle = low + (high - low) / 2;
        let result = session.search_beacon_graph_index_filtered(
            cx,
            prepared,
            query(),
            expansion,
            ReadPolicy {
                max_work_units: middle,
                ..no_staging()
            },
            &[HIGH],
        );
        match result {
            Ok(_) => high = middle,
            Err(Error::Index(BeaconError::WorkBudgetExceeded)) => low = middle + 1,
            other => panic!("unexpected threshold result: {other:?}"),
        }
        assert!(!session.is_closed());
    }
    low
}

#[test]
fn hidden_and_disconnected_edges_do_not_change_reused_native_work_allowance() {
    let ((), report) = run_async_under_lab(0xbeac_5707, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let clean = fixture(&c.commit(), false, 0).await;
        let expanded = fixture(&c.commit(), true, 128).await;
        let issuer = authority();
        let token = issuer.issue_at(&grant(), NOW).unwrap();
        let mut a = clean
            .authorized_read_session(&c.query(), &issuer, &token, "main", catalog, host(), || NOW)
            .unwrap();
        let mut b = expanded
            .authorized_read_session(&c.query(), &issuer, &token, "main", catalog, host(), || NOW)
            .unwrap();
        let pa = a.prepare_beacon_index(&c.query(), &options()).unwrap();
        let pb = b.prepare_beacon_index(&c.query(), &options()).unwrap();
        for direction in [
            ExpansionDirection::Outgoing,
            ExpansionDirection::Incoming,
            ExpansionDirection::Undirected,
        ] {
            let mut e = expansion();
            e.direction = direction;
            e.relation = None;
            let native_units = minimum_work(&mut a, &pa, &c.query(), e);
            assert!(native_units > 0);
            assert_eq!(minimum_work(&mut b, &pb, &c.query(), e), native_units);
            let exact = ReadPolicy {
                max_work_units: native_units,
                ..no_staging()
            };
            let expected = a
                .search_beacon_graph_index_filtered(&c.query(), &pa, query(), e, exact, &[HIGH])
                .unwrap();
            assert_eq!(
                b.search_beacon_graph_index_filtered(&c.query(), &pb, query(), e, exact, &[HIGH],)
                    .unwrap(),
                expected
            );
            assert!(matches!(
                b.search_beacon_graph_index_filtered(
                    &c.query(),
                    &pb,
                    query(),
                    e,
                    ReadPolicy {
                        max_work_units: native_units - 1,
                        ..no_staging()
                    },
                    &[HIGH],
                ),
                Err(Error::Index(BeaconError::WorkBudgetExceeded))
            ));
        }
        a.close();
        b.close();
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
