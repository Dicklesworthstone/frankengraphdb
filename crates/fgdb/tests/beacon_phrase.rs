//! Phrase requests exercise the real database sources and existing Beacon
//! adapters, not a hand-built document list standing in for graph storage.
use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb::{Database, DatabaseKeys, MemVfs, QueryError, WriteBatch, WriteError, WriteTxnError};
use fgdb_beacon::expansion::{ExpansionDirection, ExpansionLimits, ExpansionSpec};
use fgdb_beacon::read::{ReadError, ReadOptions, ReadPolicy, Rows, Search};
use fgdb_beacon::{ExactHybridQuery, ExactRrfProfile, GraphHybridQuery, TextMatch, VectorSearch};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId, SchemaEpoch};
use fgdb_types::{
    CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId,
};
use fgdb_warden::{
    Authority, Error as WardenError, Grant, LimitDimension, QueryLimits, Restriction, Scope,
};

const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0xd4; 32]);
const L: LabelId = LabelId(1);
const T: PropertyKeyId = PropertyKeyId(1);
type Options = ReadOptions<PropertyKeyId, LabelId>;

fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0xd3; 32], NS, [0xd5; 32])
}
fn text(value: &str) -> CanonicalScalar {
    CanonicalScalar::ucs_basic_text(value).unwrap()
}
fn options() -> Options {
    let mut options = Options::text(T);
    options.vertex_label = Some(L);
    options
}
fn phrase() -> Search<'static> {
    Search::Text {
        query: "red blue",
        k: 10,
        mode: TextMatch::Phrase,
    }
}
fn ids(rows: &Rows) -> Vec<VId> {
    let mut ids: Vec<_> = match rows {
        Rows::Text(hits) => hits.iter().map(|hit| hit.id).collect(),
        Rows::Hybrid(hits) => hits.iter().map(|hit| hit.id).collect(),
        Rows::Vector(hits) => hits.iter().map(|hit| hit.id).collect(),
    };
    ids.sort();
    ids
}
async fn database(cx: &CommitCx, hidden: bool) -> Database<MemVfs> {
    let mut db = Database::open_memory(cx, keys()).await.unwrap();
    let mut batch = WriteBatch::new(RelationId(1));
    for (id, words) in [
        (1, "blue red"),
        (2, "prefix red blue tail"),
        (3, "red red blue"),
    ] {
        batch.create_vertex(
            VId(id),
            if hidden {
                vec![L, LabelId(99)]
            } else {
                vec![L]
            },
            vec![(T, text(words))],
        );
    }
    if hidden {
        batch.create_vertex(
            VId(90),
            vec![LabelId(99)],
            vec![(T, text("red blue red blue red blue"))],
        );
        batch.create_vertex(
            VId(91),
            vec![LabelId(99)],
            vec![(T, CanonicalScalar::Int(7))],
        );
    }
    batch.add_edge(EId(1), VId(2), VId(1), vec![]);
    db.write(cx, batch).await.unwrap();
    db
}

#[test]
fn phrase_reads_follow_staged_properties_labels_and_historical_index_generations() {
    let ((), report) = run_async_under_lab(0xbeac_3101, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let commit = c.commit();
        let query = c.query();
        let mut db = database(&commit, false).await;
        let at = db.frontier().unwrap();
        let view = db.read_session().unwrap();
        let options = options();
        let mut resident = db.prepare_beacon_index(&query, &options).unwrap();
        let retained = resident.snapshot();
        let before = db.beacon_search(&query, &options, phrase()).unwrap();
        assert_eq!(ids(&before), [VId(2), VId(3)]);
        let mut txn = db.begin(&c.txn()).unwrap();
        let mut writes = WriteBatch::new(RelationId(1));
        writes.set_vertex_property(VId(1), T, Some(text("red blue")));
        writes.set_vertex_label(VId(2), L, false);
        writes.set_vertex_property(VId(3), T, None);
        writes.create_vertex(VId(4), vec![L], vec![(T, text("red blue red"))]);
        txn.write(&mut db, writes).unwrap();
        let staged = txn.beacon_search(&db, &query, &options, phrase()).unwrap();
        assert_eq!(ids(&staged), [VId(1), VId(4)]);
        assert_eq!(db.frontier().unwrap(), at);
        assert_eq!(
            db.beacon_search(&query, &options, phrase()).unwrap(),
            before
        );
        txn.commit(&mut db, &commit).await.unwrap();
        assert_eq!(
            db.beacon_search(&query, &options, phrase()).unwrap(),
            staged
        );
        resident
            .refresh(&query, &db, None, ReadPolicy::default())
            .unwrap();
        assert_eq!(
            resident
                .search(&query, phrase(), ReadPolicy::default())
                .unwrap(),
            staged
        );
        let mut historical = options.clone();
        historical.as_of = Some(at);
        db.compact(&commit).await.unwrap();
        assert_eq!(
            db.beacon_search(&query, &historical, phrase()).unwrap(),
            before
        );
        drop(db);
        assert_eq!(
            view.beacon_search(&query, &options, phrase()).unwrap(),
            before
        );
        assert_eq!(
            retained
                .search(&query, phrase(), ReadPolicy::default())
                .unwrap(),
            before
        );
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

fn hybrid() -> ExactHybridQuery<'static> {
    ExactHybridQuery {
        vector: &[],
        text: "red blue",
        k: 10,
        vector_candidates: 0,
        text_candidates: 10,
        vector_mode: VectorSearch::Exact,
        text_mode: TextMatch::Phrase,
        profile: ExactRrfProfile::new(60, 0, 1).unwrap(),
    }
}

#[test]
fn phrase_candidate_ranks_compose_with_graph_only_hits_before_final_top_k() {
    let ((), report) = run_async_under_lab(0xbeac_3102, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let commit = c.commit();
        let query = c.query();
        let mut db = database(&commit, false).await;
        let options = options();
        let base = db
            .beacon_search(&query, &options, Search::Hybrid(hybrid()))
            .unwrap();
        assert_eq!(ids(&base), [VId(2), VId(3)]);
        let q = GraphHybridQuery {
            retrieval: hybrid(),
            graph_candidates: 10,
            graph_weight: 100,
        };
        let spec = ExpansionSpec {
            seeds: &[VId(2)],
            relation: Some(RelationId(1)),
            direction: ExpansionDirection::Outgoing,
            max_hops: 1,
            include_seeds: false,
            limits: ExpansionLimits::default(),
        };
        let hits = db.beacon_search_graph(&query, &options, q, spec).unwrap();
        assert_eq!(hits.len(), 3);
        assert_eq!(hits[0].id, VId(1));
        assert_eq!(
            hits[0].text_rank, None,
            "a reversed phrase contributes no text rank"
        );
        assert_eq!(hits[0].graph_rank.unwrap().get(), 1);
        assert_eq!(
            (hits[0].score.numerator(), hits[0].score.denominator()),
            (100, 61)
        );
        // A staged text change updates positional evidence without changing
        // graph distance; the transaction and its eventual commit must agree.
        let mut txn = db.begin(&c.txn()).unwrap();
        let mut writes = WriteBatch::new(RelationId(1));
        writes.set_vertex_property(VId(1), T, Some(text("red blue")));
        txn.write(&mut db, writes).unwrap();
        let staged = txn
            .beacon_search_graph(&db, &query, &options, q, spec)
            .unwrap();
        assert!(staged[0].text_rank.is_some());
        txn.commit(&mut db, &commit).await.unwrap();
        assert_eq!(
            staged,
            db.beacon_search_graph(&query, &options, q, spec).unwrap()
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

fn grant() -> Grant {
    let mut grant = Grant::read_only(
        "main",
        1000,
        QueryLimits {
            max_nodes: 100,
            max_rows: 100,
            max_work: 1_000_000,
        },
    );
    grant.labels = Scope::only([L]);
    grant.properties = Scope::only([T]);
    grant.relations = Scope::only([RelationId(1)]);
    grant
}

#[test]
fn scoped_phrase_scores_and_work_thresholds_ignore_hidden_text_and_masked_fields() {
    let ((), report) = run_async_under_lab(0xbeac_3103, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let query = c.query();
        let full = database(&c.commit(), true).await;
        let clean = database(&c.commit(), false).await;
        let issuer = Authority::new(
            AuthKey::from_seed(3103),
            NS,
            "host-graph",
            SchemaEpoch(0),
            1,
        )
        .unwrap();
        let token = issuer.issue_at(&grant(), 100).unwrap();
        for search in [phrase(), Search::Hybrid(hybrid())] {
            let expected = clean
                .beacon_search_authorized(
                    &query,
                    &issuer,
                    &token,
                    "main",
                    &options(),
                    search,
                    || 100,
                )
                .unwrap();
            assert_eq!(ids(&expected), [VId(2), VId(3)]);
            assert_eq!(
                full.beacon_search_authorized(
                    &query,
                    &issuer,
                    &token,
                    "main",
                    &options(),
                    search,
                    || 100
                )
                .unwrap(),
                expected
            );
            let threshold = |db: &Database<MemVfs>| {
                let (mut low, mut high) = (0u64, 1_000_000u64);
                while low < high {
                    let mid = low + (high - low) / 2;
                    let bounded = token.attenuate(Restriction::MaxWork(mid)).unwrap();
                    match db.beacon_search_authorized(
                        &query,
                        &issuer,
                        &bounded,
                        "main",
                        &options(),
                        search,
                        || 100,
                    ) {
                        Ok(rows) => {
                            assert_eq!(rows, expected);
                            high = mid;
                        }
                        Err(ReadError::Interrupted(QueryError::Authorization(
                            WardenError::LimitExceeded(LimitDimension::Work),
                        ))) => low = mid + 1,
                        other => panic!("unexpected phrase refusal: {other:?}"),
                    }
                }
                assert!(low > 0 && low < 1_000_000);
                low
            };
            assert_eq!(threshold(&full), threshold(&clean));
        }
        let mut masked = grant();
        masked.properties = Scope::only([PropertyKeyId(2)]);
        let masked = issuer.issue_at(&masked, 100).unwrap();
        assert!(
            full.beacon_search_authorized(
                &query,
                &issuer,
                &masked,
                "main",
                &options(),
                phrase(),
                || 100
            )
            .unwrap()
            .is_empty()
        );
        // Remove user label selection so the malformed hidden text is actually
        // reached by this privileged negative control, not silently excluded.
        let mut unscoped = options();
        unscoped.vertex_label = None;
        assert!(full.beacon_search(&query, &unscoped, phrase()).is_err());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn phrase_reads_retain_non_hit_conflicts_across_savepoint_rollback() {
    let ((), report) = run_async_under_lab(0xbeac_3104, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let commit = c.commit();
        let mut db = database(&commit, false).await;
        let mut txn = db.begin(&c.txn()).unwrap();
        txn.savepoint(&db, "before").unwrap();
        assert_eq!(
            ids(&txn
                .beacon_search(&db, &c.query(), &options(), phrase())
                .unwrap()),
            [VId(2), VId(3)]
        );
        txn.rollback_to_savepoint(&db, "before").unwrap();
        let mut winner = WriteBatch::new(RelationId(1));
        winner.set_vertex_property(VId(1), T, Some(text("red blue")));
        db.write(&commit, winner).await.unwrap();
        assert!(matches!(
            txn.finish(&mut db, &commit).await,
            Err(WriteTxnError::Write(WriteError::FirstCommitterWins {
                law: "FG-LAW-FCW-READ-01",
                ..
            }))
        ));
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn every_public_phrase_checkpoint_interrupts_without_releasing_a_result_prefix() {
    use fgdb_types::context::SimulationCheckpointProbe;
    use std::sync::Arc;
    let ((), report) = run_async_under_lab(0xbeac_3105, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let query = c.query();
        let db = database(&c.commit(), false).await;
        for k in [0, 1] {
            let request = Search::Text {
                query: "red blue",
                k,
                mode: TextMatch::Phrase,
            };
            let trace = Arc::new(SimulationCheckpointProbe::new(None));
            let observed = query.with_checkpoint_probe(Arc::clone(&trace));
            let expected = db.beacon_search(&observed, &options(), request).unwrap();
            assert_eq!(expected.len(), k);
            for stop in 1..=trace.calls() {
                let probe = Arc::new(SimulationCheckpointProbe::new(Some(stop)));
                let interrupted = query.with_checkpoint_probe(Arc::clone(&probe));
                assert!(
                    matches!(
                        db.beacon_search(&interrupted, &options(), request),
                        Err(ReadError::Interrupted(_))
                    ),
                    "k={k}, checkpoint={stop}"
                );
                assert_eq!(probe.calls(), stop);
                assert_eq!(
                    db.beacon_search(&interrupted, &options(), request).unwrap(),
                    expected
                );
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
