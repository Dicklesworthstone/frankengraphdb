//! Three-lane retrieval through real Warden permits and native historical
//! sources. These laws compare full scores and refusal thresholds, not only
//! final IDs; they do not claim timing or physical-I/O noninterference.
use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb::{Database, DatabaseKeys, MemVfs, QueryError, WriteBatch};
use fgdb_beacon::expansion::{ExpansionDirection, ExpansionLimits, ExpansionSpec};
use fgdb_beacon::read::{ReadError, ReadOptions};
use fgdb_beacon::{
    BeaconError, DistanceMetric, ExactHybridQuery, ExactRrfProfile, GraphHybridHit,
    GraphHybridQuery, HnswConfig, TextMatch, VectorSearch,
};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId, SchemaEpoch};
use fgdb_types::{
    CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts,
    QueryCx, VId,
};
use fgdb_warden::{
    Authority, CapabilityToken, Error, Grant, LimitDimension, QueryLimits, Restriction, Rights, Scope,
};
use std::cell::Cell;

type Options = ReadOptions<PropertyKeyId, LabelId>;
type SearchError = ReadError<fgdb::ReadError, QueryError>;
const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x93; 32]);
const BRANCH: &str = "host-graph-hybrid";
const L: LabelId = LabelId(1);
const H: LabelId = LabelId(99);
const T: PropertyKeyId = PropertyKeyId(1);
const X: PropertyKeyId = PropertyKeyId(2);
const SECRET: PropertyKeyId = PropertyKeyId(3);
const R: RelationId = RelationId(1);
const HIDDEN_R: RelationId = RelationId(99);
const NOW: u64 = 100;
const EXPIRES: u64 = 1000;

fn text(value: &str) -> CanonicalScalar {
    CanonicalScalar::ucs_basic_text(value).unwrap()
}
fn authority(seed: u64) -> Authority {
    Authority::new(AuthKey::from_seed(seed), NS, "host-graph", SchemaEpoch(0), 1).unwrap()
}
fn grant() -> Grant {
    let mut grant = Grant::read_only(BRANCH, EXPIRES, QueryLimits {
        max_nodes: 1000, max_work: 1_000_000, max_rows: 1000,
    });
    grant.labels = Scope::only([L]);
    grant.properties = Scope::only([T, X]);
    grant.relations = Scope::only([R]);
    grant
}
fn options() -> Options {
    let mut options = Options::text(T);
    options.projection.vector = vec![X];
    options.index.vector = Some(HnswConfig::new(1, DistanceMetric::SquaredEuclidean));
    options
}
fn query(mode: VectorSearch) -> GraphHybridQuery<'static> {
    GraphHybridQuery {
        retrieval: ExactHybridQuery {
            vector: &[0.0], text: "graph", k: 3,
            vector_candidates: 3, text_candidates: 3,
            vector_mode: mode, text_mode: TextMatch::Any,
            profile: ExactRrfProfile::default(),
        },
        graph_candidates: 8, graph_weight: 100,
    }
}
fn expansion(seeds: &[VId]) -> ExpansionSpec<'_, RelationId> {
    ExpansionSpec {
        seeds, relation: None, direction: ExpansionDirection::Outgoing,
        max_hops: 3, include_seeds: false, limits: ExpansionLimits::default(),
    }
}
async fn database(cx: &CommitCx, hidden: bool, seed: u64) -> Database<MemVfs> {
    let mut db = Database::open_memory(cx, DatabaseKeys::new([0x91; 32], NS, [0x92; 32]))
        .await.unwrap();
    let mut batch = WriteBatch::new(R);
    for id in 1..=3 {
        let labels = if hidden && id == 1 { vec![L, H] } else { vec![L] };
        let mut props = vec![
            (T, text(if (id + u128::from(seed)) % 2 == 0 { "graph graph" } else { "graph" })),
            (X, CanonicalScalar::Int((id * 7 + u128::from(seed % 5)) as i64)),
        ];
        if hidden {
            // Even a selected visible vertex can carry forbidden bad types.
            props.push((SECRET, text("not a numeric coordinate")));
        }
        batch.create_vertex(VId(id), labels, props);
    }
    // Parallel edges count as two input edges but one shortest-path arc.
    batch.add_edge(EId(1), VId(1), VId(2), vec![]);
    batch.add_edge(EId(2), VId(1), VId(2), vec![]);
    batch.add_edge(EId(3), VId(2), VId(3), vec![]);
    if hidden {
        for id in 100..108 {
            batch.create_vertex(VId(id), vec![H], vec![
                (T, CanonicalScalar::Int(123)),
                (X, CanonicalScalar::Int(16_777_217)),
            ]);
            batch.add_edge(EId(id), VId(1), VId(id), vec![]);
            batch.add_edge(EId(id + 100), VId(id), VId(3), vec![]);
        }
    }
    db.write(cx, batch).await.unwrap();
    if hidden {
        let mut shortcut = WriteBatch::new(HIDDEN_R);
        shortcut.add_edge(EId(900), VId(1), VId(3), vec![]);
        db.write(cx, shortcut).await.unwrap();
        let mut history = WriteBatch::new(R);
        history.set_vertex_property(VId(100), T, Some(text("hidden replacement history")));
        db.write(cx, history).await.unwrap();
    }
    db
}
fn search(
    db: &Database<MemVfs>, cx: &QueryCx, issuer: &Authority, token: &CapabilityToken,
    options: &Options, query: GraphHybridQuery<'_>, expansion: ExpansionSpec<'_, RelationId>,
) -> Result<Vec<GraphHybridHit>, SearchError> {
    db.beacon_search_graph_authorized(cx, issuer, token, BRANCH, options, query, expansion, || NOW)
}
fn refused(result: Result<Vec<GraphHybridHit>, SearchError>, expected: Error) {
    assert!(matches!(
        &result,
        Err(ReadError::Interrupted(QueryError::Authorization(error))) if *error == expected
    ), "expected {expected:?}, got {result:?}");
}
fn threshold(mut high: u64, mut admits: impl FnMut(u64) -> bool) -> u64 {
    assert!(admits(high), "the ceiling must admit");
    let mut low = 0;
    while low < high {
        let middle = low + (high - low) / 2;
        if admits(middle) { high = middle; } else { low = middle + 1; }
    }
    low
}

#[test]
fn all_directions_seed_shapes_and_masked_projections_match_removed_data() {
    for seed in [3, 17, 101] {
        let ((), report) = run_async_under_lab(0xbeac_2300 + seed, move |root| async move {
            let c = PurposeContexts::narrow_runtime_root(&root);
            let full = database(&c.commit(), true, seed).await;
            let clean = database(&c.commit(), false, seed).await;
            let issuer = authority(seed);
            let token = issuer.issue_at(&grant(), NOW).unwrap();
            for mode in [VectorSearch::Exact, VectorSearch::Approximate { ef_search: 16 }] {
                for direction in [ExpansionDirection::Outgoing, ExpansionDirection::Incoming,
                    ExpansionDirection::Undirected] {
                    for seeds in [vec![VId(1)], vec![VId(3), VId(3)],
                        vec![VId(100), VId(999)], vec![]] {
                        let mut e = expansion(&seeds);
                        e.direction = direction;
                        for include in [false, true] {
                            e.include_seeds = include;
                            let actual = search(&full, &c.query(), &issuer, &token, &options(), query(mode), e).unwrap();
                            let expected = clean.beacon_search_graph(&c.query(), &options(), query(mode), e).unwrap();
                            assert_eq!(actual, expected, "seed={seed}, direction={direction:?}, seeds={seeds:?}");
                        }
                    }
                }
            }
            // A forbidden property is absent even on a visible vertex; no
            // bad-type error, coordinate padding or hidden score may escape.
            let mut masked = options();
            masked.projection.vector = vec![SECRET];
            assert_eq!(
                search(&full, &c.query(), &issuer, &token, &masked, query(VectorSearch::Exact), expansion(&[VId(1)])).unwrap(),
                clean.beacon_search_graph(&c.query(), &masked, query(VectorSearch::Exact), expansion(&[VId(1)])).unwrap(),
            );
            masked = options();
            masked.vertex_label = Some(H);
            assert!(search(&full, &c.query(), &issuer, &token, &masked,
                query(VectorSearch::Exact), expansion(&[VId(100)])).unwrap().is_empty());
            // Force the forbidden relation as the requested graph lane. It
            // must not contribute even though both shortcut endpoints are visible.
            let mut e = expansion(&[VId(1)]);
            e.relation = Some(HIDDEN_R);
            assert!(search(&full, &c.query(), &issuer, &token, &options(),
                query(VectorSearch::Exact), e).unwrap().iter().all(|hit| hit.graph_hops.is_none()));
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }
}

#[test]
fn historical_winners_precede_authorization_and_retired_edges_do_not_reappear() {
    let ((), report) = run_async_under_lab(0xbeac_2401, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let mut db = database(&c.commit(), true, 3).await;
        let issuer = authority(2401);
        let token = issuer.issue_at(&grant(), NOW).unwrap();
        let q = query(VectorSearch::Exact);
        let e = expansion(&[VId(1)]);
        let mut old = options();
        old.as_of = Some(db.frontier().unwrap());
        let before = search(&db, &c.query(), &issuer, &token, &old, q, e).unwrap();
        assert_eq!(before.iter().find(|hit| hit.id == VId(3)).unwrap().graph_hops, Some(2));
        let mut update = WriteBatch::new(R);
        update.set_vertex_label(VId(2), L, false);
        update.set_vertex_label(VId(2), H, true);
        update.set_vertex_property(VId(2), X, Some(text("hidden successor must not be projected")));
        update.delete_edge(EId(3));
        update.add_edge(EId(4), VId(1), VId(3), vec![]);
        db.write(&c.commit(), update).await.unwrap();
        let current = search(&db, &c.query(), &issuer, &token, &options(), q, e).unwrap();
        assert!(current.iter().all(|hit| hit.id != VId(2)));
        assert_eq!(current.iter().find(|hit| hit.id == VId(3)).unwrap().graph_hops, Some(1));
        assert_eq!(search(&db, &c.query(), &issuer, &token, &old, q, e).unwrap(), before);
        let mut deletion = WriteBatch::new(R);
        deletion.delete_edge(EId(4));
        db.write(&c.commit(), deletion).await.unwrap();
        assert!(search(&db, &c.query(), &issuer, &token, &options(), q, e)
            .unwrap().iter().all(|hit| hit.graph_hops.is_none()));
        assert_eq!(search(&db, &c.query(), &issuer, &token, &old, q, e).unwrap(), before);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn hidden_history_moves_no_signed_or_native_work_threshold() {
    let ((), report) = run_async_under_lab(0xbeac_2402, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let full = database(&c.commit(), true, 17).await;
        let clean = database(&c.commit(), false, 17).await;
        let issuer = authority(2402);
        let token = issuer.issue_at(&grant(), NOW).unwrap();
        let q = query(VectorSearch::Exact);
        let e = expansion(&[VId(1)]);
        for native in [false, true] {
            let measure = |db: &Database<MemVfs>| threshold(1_000_000, |limit| {
                let mut o = options();
                let restricted = if native {
                    o.policy.max_work_units = usize::try_from(limit).unwrap();
                    token.attenuate(Restriction::MaxWork(1_000_000)).unwrap()
                } else {
                    token.attenuate(Restriction::MaxWork(limit)).unwrap()
                };
                let result = search(db, &c.query(), &issuer, &restricted, &o, q, e);
                if result.is_ok() { return true; }
                if native {
                    assert!(matches!(result, Err(ReadError::Index(BeaconError::WorkBudgetExceeded))));
                } else {
                    refused(result, Error::LimitExceeded(LimitDimension::Work));
                }
                false
            });
            let observed = measure(&full);
            assert!(observed > 1);
            assert_eq!(observed, measure(&clean), "native={native}");
        }
        for (restriction, dimension) in [(Restriction::MaxNodes(2), LimitDimension::Nodes),
            (Restriction::MaxRows(2), LimitDimension::Rows)] {
            let restricted = token.attenuate(restriction).unwrap();
            for db in [&full, &clean] {
                refused(search(db, &c.query(), &issuer, &restricted, &options(), q, e),
                    Error::LimitExceeded(dimension));
            }
        }
        let exact = token.attenuate(Restriction::MaxNodes(3)).unwrap()
            .attenuate(Restriction::MaxRows(3)).unwrap();
        let mut bounded = e;
        bounded.limits.max_vertices = 3;
        bounded.limits.max_input_edges = 3;
        for db in [&full, &clean] {
            assert_eq!(search(db, &c.query(), &issuer, &exact, &options(), q, bounded).unwrap().len(), 3);
            let mut too_small = bounded;
            too_small.limits.max_input_edges = 2;
            assert!(matches!(search(db, &c.query(), &issuer, &exact, &options(), q, too_small),
                Err(ReadError::Index(BeaconError::ResourceLimit { resource: "expansion input edges", limit: 2 }))));
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn every_clock_cut_including_empty_and_zero_k_delivery_preserves_expiry() {
    let ((), report) = run_async_under_lab(0xbeac_2403, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let db = database(&c.commit(), false, 3).await;
        let issuer = authority(2403);
        let token = issuer.issue_at(&grant(), NOW).unwrap();
        for empty in 0..3 {
            let mut o = options();
            let mut q = query(VectorSearch::Exact);
            if empty == 1 { o.vertex_label = Some(H); }
            if empty == 2 { q.retrieval.k = 0; }
            let count = Cell::new(0usize);
            let result = db.beacon_search_graph_authorized(
                &c.query(), &issuer, &token, BRANCH, &o, q, expansion(&[VId(1)]), || {
                    count.set(count.get() + 1);
                    NOW
                },
            ).unwrap();
            assert_eq!(result.is_empty(), empty != 0);
            assert!(count.get() > 4);
            for cut in 0..count.get() {
                let seen = Cell::new(0usize);
                refused(db.beacon_search_graph_authorized(
                    &c.query(), &issuer, &token, BRANCH, &o, q, expansion(&[VId(1)]), || {
                        let at = seen.get();
                        seen.set(at + 1);
                        if at >= cut { EXPIRES } else { NOW }
                    },
                ), Error::Expired);
                assert_eq!(seen.get(), cut + 1, "continued after expiry at {cut}");
            }
            // A refused execution cannot poison the next request's new permit.
            assert_eq!(search(&db, &c.query(), &issuer, &token, &o, q, expansion(&[VId(1)])).unwrap(), result);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn authentication_precedes_source_and_graph_budget_errors() {
    let ((), report) = run_async_under_lab(0xbeac_2404, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let db = database(&c.commit(), true, 3).await;
        let issuer = authority(2404);
        let token = issuer.issue_at(&grant(), NOW).unwrap();
        let mut o = options();
        o.as_of = Some(CommitSeq(u64::MAX));
        let mut e = expansion(&[VId(1)]);
        e.limits.max_vertices = 0;
        let q = query(VectorSearch::Exact);
        refused(db.beacon_search_graph_authorized(&c.query(), &issuer, &token, "wrong-branch",
            &o, q, e, || NOW), Error::ScopeDenied);
        refused(search(&db, &c.query(), &authority(999), &token, &o, q, e), Error::Unauthenticated);
        let write = token.attenuate(Restriction::Rights(Rights::Write)).unwrap();
        refused(search(&db, &c.query(), &issuer, &write, &o, q, e), Error::PermissionDenied);
        assert!(matches!(search(&db, &c.query(), &issuer, &token, &o, q, e), Err(ReadError::Read(_))));
        o.as_of = None;
        assert!(matches!(search(&db, &c.query(), &issuer, &token, &o, q, e),
            Err(ReadError::Index(BeaconError::ResourceLimit { resource: "expansion vertices", limit: 0 }))));
        // A disabled graph lane must not consume its zero capacity or inspect
        // a seed-list limit; the text/vector retrieval remains useful.
        let mut disabled = q;
        disabled.graph_weight = 0;
        e.limits.max_seed_ids = 0;
        let result = search(&db, &c.query(), &issuer, &token, &o, disabled, e).unwrap();
        assert_eq!(result.len(), 3);
        assert!(result.iter().all(|hit| hit.graph_hops.is_none()));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
