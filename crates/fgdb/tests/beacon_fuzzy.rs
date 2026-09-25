//! Fuzzy reads use real native graph generations and Warden permits. These
//! assertions exercise composition; the independent edit oracle is in Beacon.
use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb::{Database, DatabaseKeys, MemVfs, QueryError, WriteBatch, WriteError, WriteTxnError};
use fgdb_beacon::expansion::{ExpansionDirection, ExpansionLimits, ExpansionSpec};
use fgdb_beacon::read::{ReadError, ReadOptions, ReadPolicy, Rows, Search};
use fgdb_beacon::{
    BeaconError, EditDistance, ExactHybridQuery, ExactRrfProfile, GraphHybridQuery,
    TextMatch, VectorSearch,
};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId, SchemaEpoch};
use fgdb_types::{
    CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId,
};
use fgdb_warden::{Authority, Error as Denial, Grant, LimitDimension, QueryLimits, Restriction, Scope};

const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0xe4; 32]);
const LABEL: LabelId = LabelId(1);
const HIDDEN: LabelId = LabelId(9);
const TEXT: PropertyKeyId = PropertyKeyId(1);
type Options = ReadOptions<PropertyKeyId, LabelId>;

fn text(value: &str) -> CanonicalScalar {
    CanonicalScalar::ucs_basic_text(value).unwrap()
}

fn options() -> Options {
    Options::text(TEXT)
}

fn mode(max_expansions: usize) -> TextMatch {
    TextMatch::Fuzzy {
        distance: EditDistance::One,
        require_all: true,
        max_expansions,
    }
}

fn request() -> Search<'static> {
    Search::Text { query: "cat dog", k: 10, mode: mode(4) }
}

fn ids(rows: &Rows) -> Vec<VId> {
    let mut ids: Vec<_> = match rows {
        Rows::Text(rows) => rows.iter().map(|row| row.id).collect(),
        Rows::Vector(rows) => rows.iter().map(|row| row.id).collect(),
        Rows::Hybrid(rows) => rows.iter().map(|row| row.id).collect(),
    };
    ids.sort();
    ids
}

async fn database(cx: &CommitCx, hidden: bool) -> Database<MemVfs> {
    let mut db = Database::open_memory(cx, DatabaseKeys::new([0xe3; 32], NS, [0xe5; 32]))
        .await.unwrap();
    let mut batch = WriteBatch::new(RelationId(1));
    for (id, words) in [(1, "cat cot"), (2, "coat dog"), (3, "cat dog")] {
        let mut props = vec![(TEXT, text(words))];
        let mut labels = vec![LABEL];
        if hidden {
            props.push((PropertyKeyId(99), text("bat fog rat log")));
            labels.push(HIDDEN);
        }
        batch.create_vertex(VId(id), labels, props);
    }
    batch.create_vertex(VId(4), vec![LABEL], vec![]);
    batch.add_edge(EId(1), VId(1), VId(4), vec![]);
    if hidden {
        batch.create_vertex(VId(100), vec![HIDDEN], vec![(TEXT, text("bat fog"))]);
        batch.create_vertex(VId(101), vec![HIDDEN], vec![(TEXT, CanonicalScalar::Int(17))]);
        batch.add_edge(EId(2), VId(1), VId(100), vec![]);
        batch.add_edge(EId(3), VId(100), VId(3), vec![]);
    }
    db.write(cx, batch).await.unwrap();
    if hidden {
        let mut update = WriteBatch::new(RelationId(1));
        update.set_vertex_property(VId(100), TEXT, Some(text("rat log")));
        db.write(cx, update).await.unwrap();
    }
    db
}

#[test]
fn fuzzy_reads_follow_transaction_changes_history_and_resident_refresh() {
    let ((), report) = run_async_under_lab(0xbeac_4101, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let commit = c.commit();
        let query = c.query();
        let mut db = database(&commit, false).await;
        let at = db.frontier().unwrap();
        let view = db.read_session().unwrap();
        let mut options = options();
        options.vertex_label = Some(LABEL);
        let mut resident = db.prepare_beacon_index(&query, &options).unwrap();
        let old = resident.snapshot();
        let before = db.beacon_search(&query, &options, request()).unwrap();
        assert_eq!(ids(&before), [VId(2), VId(3)]);
        let mut txn = db.begin(&c.txn()).unwrap();
        let mut writes = WriteBatch::new(RelationId(1));
        writes.set_vertex_property(VId(1), TEXT, Some(text("cat dog")));
        writes.delete_vertex(VId(2));
        writes.set_vertex_label(VId(3), LABEL, false);
        writes.create_vertex(VId(5), vec![LABEL], vec![(TEXT, text("cot dog"))]);
        txn.write(&mut db, writes).unwrap();
        let staged = txn.beacon_search(&db, &query, &options, request()).unwrap();
        assert_eq!(ids(&staged), [VId(1), VId(5)]);
        assert_eq!(db.frontier().unwrap(), at);
        assert_eq!(db.beacon_search(&query, &options, request()).unwrap(), before);
        txn.commit(&mut db, &commit).await.unwrap();
        assert_eq!(db.beacon_search(&query, &options, request()).unwrap(), staged);
        resident.refresh(&query, &db, None, ReadPolicy::default()).unwrap();
        assert_eq!(resident.search(&query, request(), ReadPolicy::default()).unwrap(), staged);
        let mut historical = options.clone();
        historical.as_of = Some(at);
        db.compact(&commit).await.unwrap();
        assert_eq!(db.beacon_search(&query, &historical, request()).unwrap(), before);
        drop(db);
        assert_eq!(view.beacon_search(&query, &options, request()).unwrap(), before);
        assert_eq!(old.search(&query, request(), ReadPolicy::default()).unwrap(), before);
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

fn hybrid() -> ExactHybridQuery<'static> {
    ExactHybridQuery {
        vector: &[],
        text: "cat dog",
        k: 10,
        vector_candidates: 0,
        text_candidates: 10,
        vector_mode: VectorSearch::Exact,
        text_mode: mode(4),
        profile: ExactRrfProfile::new(60, 0, 1).unwrap(),
    }
}

#[test]
fn graph_fusion_does_not_award_text_ranks_to_unsatisfied_fuzzy_groups() {
    let ((), report) = run_async_under_lab(0xbeac_4102, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let commit = c.commit();
        let query = c.query();
        let mut db = database(&commit, false).await;
        let options = options();
        let base = db.beacon_search(&query, &options, Search::Hybrid(hybrid())).unwrap();
        assert_eq!(ids(&base), [VId(2), VId(3)]);
        let q = GraphHybridQuery { retrieval: hybrid(), graph_candidates: 10, graph_weight: 100 };
        let spec = ExpansionSpec {
            seeds: &[VId(1)],
            relation: Some(RelationId(1)),
            direction: ExpansionDirection::Outgoing,
            max_hops: 1,
            include_seeds: false,
            limits: ExpansionLimits::default(),
        };
        let before = db.beacon_search_graph(&query, &options, q, spec).unwrap();
        assert_eq!(before.len(), 3);
        assert_eq!(before[0].id, VId(4));
        assert_eq!(before[0].text_rank, None);
        assert_eq!((before[0].score.numerator(), before[0].score.denominator()), (100, 61));
        assert!(before.iter().all(|hit| hit.id != VId(1)));
        let mut txn = db.begin(&c.txn()).unwrap();
        let mut writes = WriteBatch::new(RelationId(1));
        writes.set_vertex_property(VId(4), TEXT, Some(text("cat dog")));
        txn.write(&mut db, writes).unwrap();
        let staged = txn.beacon_search_graph(&db, &query, &options, q, spec).unwrap();
        assert_eq!(staged[0].id, VId(4));
        assert!(staged[0].text_rank.is_some());
        txn.commit(&mut db, &commit).await.unwrap();
        assert_eq!(db.beacon_search_graph(&query, &options, q, spec).unwrap(), staged);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

fn grant() -> Grant {
    let mut grant = Grant::read_only("main", 1000, QueryLimits {
        max_nodes: 100,
        max_rows: 100,
        max_work: 1_000_000,
    });
    grant.labels = Scope::only([LABEL]);
    grant.properties = Scope::only([TEXT]);
    grant.relations = Scope::only([RelationId(1)]);
    grant
}

#[test]
fn hidden_vocabulary_cannot_change_expansion_caps_scores_or_work_thresholds() {
    let ((), report) = run_async_under_lab(0xbeac_4103, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let query = c.query();
        let full = database(&c.commit(), true).await;
        let clean = database(&c.commit(), false).await;
        let issuer = Authority::new(AuthKey::from_seed(4103), NS, "host-graph", SchemaEpoch(0), 1).unwrap();
        let token = issuer.issue_at(&grant(), 100).unwrap();
        for request in [request(), Search::Hybrid(hybrid())] {
            let expected = clean.beacon_search_authorized(
                &query, &issuer, &token, "main", &options(), request, || 100,
            ).unwrap();
            assert_eq!(ids(&expected), [VId(2), VId(3)]);
            assert_eq!(full.beacon_search_authorized(
                &query, &issuer, &token, "main", &options(), request, || 100,
            ).unwrap(), expected);
            for native in [false, true] {
                let threshold = |db: &Database<MemVfs>| {
                    let (mut low, mut high) = (0u64, 1_000_000u64);
                    while low < high {
                        let middle = low + (high - low) / 2;
                        let mut options = options();
                        let restricted = token.attenuate(Restriction::MaxWork(
                            if native { 1_000_000 } else { middle },
                        )).unwrap();
                        if native {
                            options.policy.max_work_units = middle as usize;
                        }
                        match db.beacon_search_authorized(
                            &query, &issuer, &restricted, "main", &options, request, || 100,
                        ) {
                            Ok(rows) => { assert_eq!(rows, expected); high = middle; }
                            Err(ReadError::Index(BeaconError::WorkBudgetExceeded)) if native => low = middle + 1,
                            Err(ReadError::Interrupted(QueryError::Authorization(
                                Denial::LimitExceeded(LimitDimension::Work),
                            ))) if !native => low = middle + 1,
                            other => panic!("unexpected fuzzy limit result: {other:?}"),
                        }
                    }
                    assert!(low > 0 && low < 1_000_000);
                    low
                };
                assert_eq!(threshold(&full), threshold(&clean), "native={native}");
            }
        }
        // All four visible alternatives are counted, even under top-k = 1.
        let too_small = Search::Text { query: "cat dog", k: 1, mode: mode(3) };
        assert!(matches!(full.beacon_search_authorized(
            &query, &issuer, &token, "main", &options(), too_small, || 100,
        ), Err(ReadError::Index(BeaconError::ResourceLimit {
            resource: "fuzzy expanded terms", limit: 3,
        }))));
        // A masked text field contributes neither values, types nor words.
        let mut masked = grant();
        masked.properties = Scope::only([PropertyKeyId(2)]);
        let masked = issuer.issue_at(&masked, 100).unwrap();
        assert!(full.beacon_search_authorized(
            &query, &issuer, &masked, "main", &options(), request(), || 100,
        ).unwrap().is_empty());
        // This control reaches a REAL malformed hidden value; no user label
        // selection excludes it from the privileged source before projection.
        assert!(full.beacon_search(&query, &options(), request()).is_err());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn nonmatching_rows_remain_conflict_dependencies_after_savepoint_rollback() {
    let ((), report) = run_async_under_lab(0xbeac_4104, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let mut db = database(&c.commit(), false).await;
        let mut txn = db.begin(&c.txn()).unwrap();
        txn.savepoint(&db, "before-read").unwrap();
        let rows = txn.beacon_search(&db, &c.query(), &options(), request()).unwrap();
        assert_eq!(ids(&rows), [VId(2), VId(3)]);
        txn.rollback_to_savepoint(&db, "before-read").unwrap();
        let mut winner = WriteBatch::new(RelationId(1));
        winner.set_vertex_property(VId(1), TEXT, Some(text("cat dog")));
        db.write(&c.commit(), winner).await.unwrap();
        assert!(matches!(txn.finish(&mut db, &c.commit()).await,
            Err(WriteTxnError::Write(WriteError::FirstCommitterWins {
                law: "FG-LAW-FCW-READ-01", ..
            }))));
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn public_cancellation_and_final_authority_expiry_never_release_a_prefix() {
    use fgdb_types::context::SimulationCheckpointProbe;
    use std::cell::Cell;
    use std::sync::Arc;

    let ((), report) = run_async_under_lab(0xbeac_4105, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let query = c.query();
        let db = database(&c.commit(), false).await;
        let issuer = Authority::new(AuthKey::from_seed(4105), NS, "host-graph", SchemaEpoch(0), 1).unwrap();
        let token = issuer.issue_at(&grant(), 100).unwrap();
        for k in [0, 1] {
            let request = Search::Text { query: "cat dog", k, mode: mode(4) };
            let trace = Arc::new(SimulationCheckpointProbe::new(None));
            let observed = query.with_checkpoint_probe(Arc::clone(&trace));
            let expected = db.beacon_search(&observed, &options(), request).unwrap();
            assert_eq!(expected.len(), k);
            for stop in 1..=trace.calls() {
                let probe = Arc::new(SimulationCheckpointProbe::new(Some(stop)));
                let interrupted = query.with_checkpoint_probe(Arc::clone(&probe));
                assert!(matches!(db.beacon_search(&interrupted, &options(), request),
                    Err(ReadError::Interrupted(_))), "k={k} stop={stop}");
                assert_eq!(probe.calls(), stop);
                assert_eq!(db.beacon_search(&interrupted, &options(), request).unwrap(), expected);
            }
            let count = Cell::new(0usize);
            db.beacon_search_authorized(&query, &issuer, &token, "main", &options(), request, || {
                count.set(count.get() + 1);
                100
            }).unwrap();
            assert!(count.get() > 2);
            for cut in [0, count.get() / 2, count.get() - 1] {
                let seen = Cell::new(0usize);
                let result = db.beacon_search_authorized(
                    &query, &issuer, &token, "main", &options(), request, || {
                        let at = seen.get();
                        seen.set(at + 1);
                        if at >= cut { 1000 } else { 100 }
                    },
                );
                assert!(matches!(result, Err(ReadError::Interrupted(
                    QueryError::Authorization(Denial::Expired),
                ))));
                assert_eq!(seen.get(), cut + 1);
            }
        }
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
