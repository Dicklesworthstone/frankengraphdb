use super::*;
use crate::{Database, DatabaseKeys, MemVfs, WriteBatch};
use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb_beacon::expansion::{ExpansionDirection, ExpansionLimits};
use fgdb_beacon::{
    DistanceMetric, ExactHybridQuery, ExactRrfProfile, HnswConfig, TextMatch, VectorSearch,
};
use fgdb_delta_types::{LabelId, PropertyKeyId, SchemaEpoch};
use fgdb_gql::{GqlQueryPolicy, GraphSymbol, GraphSymbolKind};
use fgdb_types::context::SimulationCheckpointProbe;
use fgdb_types::{CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, EId, PurposeContexts};
use fgdb_warden::{Authority, Error as WardenError, Grant, LimitDimension, QueryLimits, Scope};
use std::sync::Arc;

const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x85; 32]);
const L: LabelId = LabelId(1);
const R: RelationId = RelationId(1);
const T: PropertyKeyId = PropertyKeyId(1);
const X: PropertyKeyId = PropertyKeyId(2);
const HIGH: VId = VId(u128::MAX);
const NOW: u64 = 100;
const EXPIRES: u64 = 10_000;

fn issuer() -> Authority {
    Authority::new(AuthKey::from_seed(0x8551), NS, "session-graph", SchemaEpoch(0), 1).unwrap()
}
fn grant() -> Grant {
    let mut value = Grant::read_only(
        "main", EXPIRES,
        QueryLimits { max_nodes: 4, max_work: 100_000_000, max_rows: 8 },
    );
    value.labels = Scope::only([L]);
    value.relations = Scope::only([R]);
    value.properties = Scope::only([T, X]);
    value
}
fn host() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 8, 10_000_000, 10_000)
}
fn catalog(_: GraphSymbolKind, _: &str) -> Option<GraphSymbol> { None }
fn options() -> Options {
    let mut value = Options::text(T);
    value.vertex_label = Some(L);
    value.projection.vector = vec![X];
    value.index.vector = Some(HnswConfig::new(1, DistanceMetric::SquaredEuclidean));
    value
}
fn graph_options() -> Options {
    let mut value = options();
    // No index can be built, and the dormant projection does not fit one.
    value.index.text = None;
    value.index.vector = None;
    value.index.max_text_bytes = 0;
    value.index.max_vector_values = 0;
    value.projection.vector = vec![X, X];
    value
}
fn query() -> GraphHybridQuery<'static> {
    GraphHybridQuery {
        retrieval: ExactHybridQuery {
            text: "graph", vector: &[0.0], k: 1,
            text_candidates: 1, vector_candidates: 1,
            text_mode: TextMatch::Any, vector_mode: VectorSearch::Exact,
            profile: ExactRrfProfile::default(),
        },
        graph_candidates: 1, graph_weight: 100,
    }
}
fn expansion() -> ExpansionSpec<'static, RelationId> {
    ExpansionSpec {
        seeds: &[VId(0)], relation: Some(R), direction: ExpansionDirection::Outgoing,
        max_hops: 2, include_seeds: false, limits: ExpansionLimits::default(),
    }
}
async fn fixture(cx: &CommitCx, hidden: bool) -> Database<MemVfs> {
    let mut db = Database::open_memory(cx, DatabaseKeys::new([0x84; 32], NS, [0x86; 32]))
        .await.unwrap();
    let mut batch = WriteBatch::new(R);
    for (id, x) in [(VId(0), 0), (HIGH, 9), (VId(9), 1)] {
        batch.create_vertex(id, vec![L], vec![
            (T, CanonicalScalar::ucs_basic_text("graph").unwrap()),
            (X, CanonicalScalar::Int(x)),
        ]);
    }
    batch.create_vertex(VId(1), vec![L], vec![]);
    for (eid, src, dst) in [
        (0, VId(0), VId(1)), (1, VId(0), VId(1)), (2, VId(1), HIGH),
        (3, HIGH, VId(0)), (4, VId(1), VId(1)),
    ] { batch.add_edge(EId(eid), src, dst, vec![]); }
    if hidden {
        batch.create_vertex(VId(90), vec![LabelId(99)], vec![
            (T, CanonicalScalar::Int(99)),
            (X, CanonicalScalar::ucs_basic_text("invalid hidden vector").unwrap()),
        ]);
        batch.add_edge(EId(10), VId(0), VId(90), vec![]);
        batch.add_edge(EId(11), VId(90), HIGH, vec![]);
    }
    db.write(cx, batch).await.unwrap();
    if hidden {
        let mut forbidden = WriteBatch::new(RelationId(99));
        forbidden.add_edge(EId(u128::MAX), VId(0), HIGH, vec![]);
        db.write(cx, forbidden).await.unwrap();
    }
    db
}

#[test]
fn filtered_session_fusion_refills_every_modality_without_losing_authorized_transit() {
    let ((), report) = run_async_under_lab(0xbeac_5801, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let db = fixture(&c.commit(), true).await;
        let clean = fixture(&c.commit(), false).await;
        let issuer = issuer();
        let token = issuer.issue_at(&grant(), NOW).unwrap();
        let mut session = db.authorized_read_session(
            &c.query(), &issuer, &token, "main", catalog, host(), || NOW,
        ).unwrap();
        for (v, t) in [(1, 1), (1, 0), (0, 1), (0, 0)] {
            let mut q = query();
            let opt = if v == 0 && t == 0 {
                q = GraphHybridQuery::graph_only(1, 1, 100, 60).unwrap();
                graph_options()
            } else {
                q.retrieval.profile = ExactRrfProfile::new(60, v, t).unwrap();
                options()
            };
            for direction in [ExpansionDirection::Outgoing, ExpansionDirection::Incoming,
                ExpansionDirection::Undirected] {
                let mut e = expansion();
                e.direction = direction;
                e.relation = None;
                let expected = clean.beacon_search_graph_filtered(
                    &c.query(), &opt, q, e, |id| id == HIGH,
                ).unwrap();
                let actual = session.beacon_search_graph_filtered(
                    &c.query(), &opt, q, e, &[VId(90), HIGH],
                ).unwrap();
                assert_eq!(actual, expected);
                assert_eq!(actual.len(), 1);
                assert_eq!(actual[0].id, HIGH);
                assert_eq!(actual[0].graph_rank.unwrap().get(), 1);
                assert_eq!(actual[0].graph_hops,
                    Some(if direction == ExpansionDirection::Outgoing { 2 } else { 1 }));
                assert_eq!(actual[0].vector_rank.map(|r| r.get()), (v != 0).then_some(1));
                assert_eq!(actual[0].text_rank.map(|r| r.get()), (t != 0).then_some(1));
            }
        }
        let expected = session.beacon_search_graph_filtered(
            &c.query(), &options(), query(), expansion(), &[HIGH],
        ).unwrap();
        assert_eq!((expected[0].score.numerator(), expected[0].score.denominator()), (102, 61));
        let mut ann = query();
        ann.retrieval.vector_mode = VectorSearch::Approximate { ef_search: 32 };
        assert_eq!(session.beacon_search_graph_filtered(
            &c.query(), &options(), ann, expansion(), &[HIGH],
        ).unwrap(), expected);
        assert!(session.beacon_search_graph_filtered(
            &c.query(), &options(), query(), expansion(), &[],
        ).unwrap().is_empty());
        assert_eq!(session.beacon_search_graph(&c.query(), &options(), query(), expansion()).unwrap(),
            clean.beacon_search_graph(&c.query(), &options(), query(), expansion()).unwrap());
        session.close();
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn graph_only_session_never_projects_values_and_keeps_historical_pins() {
    let ((), report) = run_async_under_lab(0xbeac_5802, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let mut db = fixture(&c.commit(), true).await;
        // Even malformed VISIBLE scalar fields are irrelevant to topology.
        let mut edit = WriteBatch::new(R);
        edit.set_vertex_property(HIGH, T, Some(CanonicalScalar::Int(42)));
        edit.set_vertex_property(HIGH, X,
            Some(CanonicalScalar::ucs_basic_text("not an embedding").unwrap()));
        db.write(&c.commit(), edit).await.unwrap();
        let at = db.frontier().unwrap();
        let issuer = issuer();
        let token = issuer.issue_at(&grant(), NOW).unwrap();
        let mut session = db.authorized_read_session(
            &c.query(), &issuer, &token, "main", catalog, host(), || NOW,
        ).unwrap();
        let opt = graph_options();
        assert!(opt.index.validate().is_err());
        let q = GraphHybridQuery::graph_only(1, 1, 100, 60).unwrap();
        let before = session.beacon_search_graph_filtered(
            &c.query(), &opt, q, expansion(), &[HIGH],
        ).unwrap();
        assert_eq!(before[0].graph_hops, Some(2));
        assert_eq!((before[0].score.numerator(), before[0].score.denominator()), (100, 61));
        assert!(session.beacon_search_graph_filtered(
            &c.query(), &options(), query(), expansion(), &[],
        ).is_err(), "reject-all cannot hide invalid enabled fields");
        let mut edit = WriteBatch::new(R);
        edit.delete_vertex(VId(1));
        db.write(&c.commit(), edit).await.unwrap();
        let mut later = db.authorized_read_session(
            &c.query(), &issuer, &token, "main", catalog, host(), || NOW,
        ).unwrap();
        assert!(later.beacon_search_graph_filtered(
            &c.query(), &opt, q, expansion(), &[HIGH],
        ).unwrap().is_empty());
        let mut historical = opt.clone();
        historical.as_of = Some(at);
        assert_eq!(later.beacon_search_graph_filtered(
            &c.query(), &historical, q, expansion(), &[HIGH],
        ).unwrap(), before);
        db.compact(&c.commit()).await.unwrap();
        drop(db);
        assert_eq!(session.beacon_search_graph_filtered(
            &c.query(), &opt, q, expansion(), &[HIGH],
        ).unwrap(), before);
        assert_eq!(later.beacon_search_graph_filtered(
            &c.query(), &historical, q, expansion(), &[HIGH],
        ).unwrap(), before);
        session.close();
        later.close();
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn reject_all_preserves_whole_source_limits_and_typed_budget_failures() {
    let ((), report) = run_async_under_lab(0xbeac_5803, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let db = fixture(&c.commit(), true).await;
        let issuer = issuer();
        let token = issuer.issue_at(&grant(), NOW).unwrap();
        let mut session = db.authorized_read_session(
            &c.query(), &issuer, &token, "main", catalog, host(), || NOW,
        ).unwrap();
        let q = GraphHybridQuery::graph_only(1, 1, 1, 60).unwrap();
        for ids in [&[][..], &[HIGH][..]] {
            for case in 0..5 {
                let mut opt = graph_options();
                let mut e = expansion();
                match case {
                    0 => opt.policy.max_work_units = 0,
                    1 => opt.policy.max_staging_rows = 0,
                    2 => opt.index.max_documents = 0,
                    3 => e.limits.max_input_edges = 4, // all five logical edges are admitted
                    _ => e.limits.max_visited_vertices = 2,
                }
                let error = session.beacon_search_graph_filtered(&c.query(), &opt, q, e, ids);
                assert!(matches!(error, Err(Error::Index(BeaconError::WorkBudgetExceeded))
                    | Err(Error::Index(BeaconError::ResourceLimit { .. }))), "{error:?}");
                assert!(!session.is_closed());
            }
        }
        for ids in [&[HIGH, VId(0)][..], &[HIGH, HIGH][..]] {
            assert!(matches!(session.beacon_search_graph_filtered(
                &c.query(), &graph_options(), q, expansion(), ids,
            ), Err(Error::Index(BeaconError::InvalidQuery(_)))));
        }
        let too_many = vec![VId(0); 100_001];
        assert!(matches!(session.beacon_search_graph_filtered(
            &c.query(), &graph_options(), q, expansion(), &too_many,
        ), Err(Error::Index(BeaconError::ResourceLimit { resource: "eligible identities", .. }))));
        let oversized = GraphHybridQuery::graph_only(9, 9, 1, 60).unwrap();
        assert!(matches!(session.beacon_search_graph_filtered(
            &c.query(), &graph_options(), oversized, expansion(), &[],
        ), Err(Error::Interrupted(QueryError::Authorization(
            WardenError::LimitExceeded(LimitDimension::Rows))))));
        session.close();
        // Four visible vertices and five allowed logical edges: nine records.
        for maximum in [8, 9] {
            let mut limited = db.authorized_read_session(
                &c.query(), &issuer, &token, "main", catalog,
                GqlQueryPolicy::new(maximum, 8, 10_000_000, 10_000), || NOW,
            ).unwrap();
            let result = limited.beacon_search_graph_filtered(
                &c.query(), &graph_options(), q, expansion(), &[],
            );
            if maximum == 8 {
                assert!(matches!(result,
                    Err(Error::Interrupted(QueryError::Pattern(GqlQueryError::Rows(error))))
                    if error.dimension == GqlBudgetDimension::SnapshotRecords
                        && error.limit == 8 && error.observed == 9));
            } else { assert!(result.unwrap().is_empty()); }
            assert!(!limited.is_closed());
            limited.close();
        }
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn graph_only_zero_hops_needs_no_edges_and_disabled_graph_limits_stay_unused() {
    let ((), report) = run_async_under_lab(0xbeac_5804, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let db = fixture(&c.commit(), true).await;
        let issuer = issuer();
        let token = issuer.issue_at(&grant(), NOW).unwrap();
        let mut session = db.authorized_read_session(
            &c.query(), &issuer, &token, "main", catalog, host(), || NOW,
        ).unwrap();
        let mut e = expansion();
        e.seeds = &[VId(9), VId(90), HIGH, HIGH, VId(999)];
        e.max_hops = 0;
        e.include_seeds = true;
        e.limits.max_input_edges = 0;
        e.limits.max_source_scratch = 0;
        let q = GraphHybridQuery::graph_only(2, 2, 1, 60).unwrap();
        let rows = session.beacon_search_graph(&c.query(), &graph_options(), q, e).unwrap();
        assert_eq!(rows.iter().map(|r| (r.id, r.graph_hops)).collect::<Vec<_>>(),
            [(VId(9), Some(0)), (HIGH, Some(0))]);
        assert!(session.beacon_search_graph_filtered(
            &c.query(), &graph_options(), q, e, &[VId(90)],
        ).unwrap().is_empty());
        let mut invalid = q;
        invalid.graph_weight = 0;
        invalid.retrieval.k = 0;
        assert!(matches!(session.beacon_search_graph_filtered(
            &c.query(), &graph_options(), invalid, e, &[],
        ), Err(Error::Index(BeaconError::InvalidQuery(_)))));
        let mut disabled = query();
        disabled.graph_weight = 0;
        disabled.graph_candidates = 0;
        e.limits.max_vertices = 0;
        e.limits.max_seed_ids = 0;
        e.limits.max_visited_vertices = 0;
        let hits = session.beacon_search_graph_filtered(
            &c.query(), &options(), disabled, e, &[HIGH],
        ).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, HIGH);
        assert_eq!(hits[0].graph_rank, None);
        session.close();
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn filtered_session_checks_every_cancellation_and_final_expiry_boundary() {
    let ((), report) = run_async_under_lab(0xbeac_5805, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let db = fixture(&c.commit(), true).await;
        let issuer = issuer();
        let token = issuer.issue_at(&grant(), NOW).unwrap();
        let mut session = db.authorized_read_session(
            &c.query(), &issuer, &token, "main", catalog, host(), || NOW,
        ).unwrap();
        for (q, opt) in [(query(), options()),
            (GraphHybridQuery::graph_only(0, 1, 1, 60).unwrap(), graph_options())] {
            for ids in [&[HIGH][..], &[][..]] {
                let probe = Arc::new(SimulationCheckpointProbe::new(None));
                let expected = session.beacon_search_graph_filtered(
                    &c.query().with_checkpoint_probe(probe.clone()), &opt, q, expansion(), ids,
                ).unwrap();
                for cut in 1..=probe.calls() {
                    let probe = Arc::new(SimulationCheckpointProbe::new(Some(cut)));
                    let result = session.beacon_search_graph_filtered(
                        &c.query().with_checkpoint_probe(probe.clone()), &opt, q, expansion(), ids,
                    );
                    assert!(matches!(result, Err(Error::Interrupted(_))), "cut={cut}: {result:?}");
                    assert!(probe.calls() >= cut);
                    assert!(!session.is_closed());
                    assert_eq!(session.beacon_search_graph_filtered(
                        &c.query(), &opt, q, expansion(), ids,
                    ).unwrap(), expected);
                }
            }
        }
        session.close();
        for ids in [&[HIGH][..], &[][..]] {
            let seen = Cell::new(0);
            let stop = Cell::new(None);
            let mut session = db.authorized_read_session(
                &c.query(), &issuer, &token, "main", catalog, host(), || {
                    let n = seen.get();
                    seen.set(n + 1);
                    if stop.get().is_some_and(|limit| n >= limit) { EXPIRES } else { NOW }
                },
            ).unwrap();
            seen.set(0);
            session.beacon_search_graph_filtered(
                &c.query(), &options(), query(), expansion(), ids,
            ).unwrap();
            let last = seen.get() - 1;
            seen.set(0);
            stop.set(Some(last));
            assert!(matches!(session.beacon_search_graph_filtered(
                &c.query(), &options(), query(), expansion(), ids,
            ), Err(Error::Interrupted(QueryError::Authorization(WardenError::Expired)))));
            assert!(session.is_closed());
            assert!(matches!(session.beacon_search_graph_filtered(
                &c.query(), &options(), query(), expansion(), ids,
            ), Err(Error::Interrupted(QueryError::Authorization(WardenError::ExecutionStopped)))));
        }
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
