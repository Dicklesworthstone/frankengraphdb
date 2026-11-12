//! Boolean clauses reach native, maintained, historical and authorized reads
//! through the same bounded text lane. Literal membership is independent of
//! the kernel; live cold reads check maintained scores and generation pinning.

use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb::{Database, DatabaseKeys, MemVfs, QueryError, WriteBatch};
use fgdb_beacon::read::{ReadError, ReadOptions, ReadPolicy, Rows, Search};
use fgdb_beacon::{BeaconError, BooleanTextQuery, DistanceMetric, HnswConfig};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId, SchemaEpoch};
use fgdb_types::context::SimulationCheckpointProbe;
use fgdb_types::{CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, PurposeContexts, VId};
use fgdb_warden::{
    Authority, Error as Denial, Grant, LimitDimension, QueryLimits, Restriction, Scope,
};
use std::cell::Cell;
use std::sync::Arc;

const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0xb2; 32]);
const LABEL: LabelId = LabelId(1);
const HIDDEN: LabelId = LabelId(9);
const TEXT: PropertyKeyId = PropertyKeyId(1);
type Options = ReadOptions<PropertyKeyId, LabelId>;

fn text(value: &str) -> CanonicalScalar {
    CanonicalScalar::ucs_basic_text(value).unwrap()
}

fn request(k: usize) -> Search<'static> {
    Search::BooleanText {
        query: BooleanTextQuery {
            required: "graph",
            optional: "storage database",
            excluded: "deprecated",
            minimum_optional: 1,
        },
        k,
    }
}

fn ids(rows: &Rows) -> Vec<VId> {
    let rows = match rows {
        Rows::Text(rows) => Some(rows),
        _ => None,
    }
    .expect("Boolean text returned another result lane");
    let mut ids: Vec<_> = rows.iter().map(|row| row.id).collect();
    ids.sort_unstable();
    ids
}

async fn database(cx: &CommitCx, hidden: bool) -> Database<MemVfs> {
    let mut db = Database::open_memory(cx, DatabaseKeys::new([0xb1; 32], NS, [0xb3; 32]))
        .await
        .unwrap();
    let mut batch = WriteBatch::new(RelationId(1));
    for (id, words) in [
        (1, "graph graph storage"),
        (2, "graph database"),
        (3, "graph storage deprecated"),
        (4, "graph"),
        (5, "storage database"),
        (7, "graph storage database"),
    ] {
        batch.create_vertex(VId(id), vec![LABEL], vec![(TEXT, text(words))]);
    }
    batch.create_vertex(VId(6), vec![LABEL], vec![]);
    if hidden {
        batch.create_vertex(
            VId(99),
            vec![HIDDEN],
            vec![(TEXT, text("graph graph graph storage database deprecated"))],
        );
        batch.create_vertex(
            VId(100),
            vec![HIDDEN],
            vec![(TEXT, CanonicalScalar::Int(7))],
        );
    }
    db.write(cx, batch).await.unwrap();
    db
}

#[test]
fn clauses_keep_native_membership_eligibility_and_shared_query_limits() {
    let ((), report) = run_async_under_lab(0xbeac_b001, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let query = c.query();
        let db = database(&c.commit(), false).await;
        let options = Options::text(TEXT);
        let expected = db.beacon_search(&query, &options, request(10)).unwrap();
        assert_eq!(ids(&expected), [VId(1), VId(2), VId(7)]);
        for (clause, members) in [
            (
                BooleanTextQuery {
                    required: "graph",
                    optional: "storage database",
                    excluded: "deprecated",
                    minimum_optional: 2,
                },
                vec![VId(7)],
            ),
            (
                BooleanTextQuery {
                    optional: "storage database",
                    excluded: "deprecated",
                    ..BooleanTextQuery::default()
                },
                vec![VId(1), VId(2), VId(5), VId(7)],
            ),
            (
                BooleanTextQuery {
                    excluded: "deprecated",
                    ..BooleanTextQuery::default()
                },
                vec![],
            ),
            (
                BooleanTextQuery {
                    required: "graph",
                    excluded: "graph",
                    ..BooleanTextQuery::default()
                },
                vec![],
            ),
        ] {
            let rows = db
                .beacon_search(
                    &query,
                    &options,
                    Search::BooleanText {
                        query: clause,
                        k: 10,
                    },
                )
                .unwrap();
            assert_eq!(ids(&rows), members);
        }
        let last = match &expected {
            Rows::Text(all) => all.last().copied(),
            _ => None,
        }
        .expect("Boolean result contains matching text hits");
        assert_eq!(
            db.beacon_search_filtered(&query, &options, request(1), |id| id == last.id)
                .unwrap(),
            Rows::Text(vec![last]),
            "eligibility precedes top-k and preserves the original corpus score"
        );
        let mut unused_vector = options.clone();
        unused_vector.index.vector = Some(HnswConfig::new(0, DistanceMetric::SquaredEuclidean));
        unused_vector.projection.vector = vec![TEXT];
        assert_eq!(
            db.beacon_search(&query, &unused_vector, request(10))
                .unwrap(),
            expected,
            "Boolean text neither validates nor projects an inactive vector lane"
        );
        for k in [0, 1] {
            let mut bytes = options.clone();
            bytes.index.text.as_mut().unwrap().max_query_bytes = 30;
            assert!(matches!(
                db.beacon_search(&query, &bytes, request(k)),
                Err(ReadError::Index(BeaconError::ResourceLimit {
                    resource: "text bytes",
                    limit: 30
                }))
            ));
            let mut terms = options.clone();
            terms.index.text.as_mut().unwrap().max_query_terms = 3;
            let repeated = Search::BooleanText {
                query: BooleanTextQuery {
                    required: "graph",
                    optional: "graph storage",
                    excluded: "deprecated",
                    minimum_optional: 1,
                },
                k,
            };
            assert!(matches!(
                db.beacon_search(&query, &terms, repeated),
                Err(ReadError::Index(BeaconError::ResourceLimit {
                    resource: "distinct analyzed terms",
                    limit: 3,
                }))
            ));
        }
        let mut limited = options.clone();
        limited.policy.max_result_rows = 1;
        assert!(matches!(
            db.beacon_search(&query, &limited, request(2)),
            Err(ReadError::Index(BeaconError::ResourceLimit {
                resource: "result rows",
                limit: 1
            }))
        ));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn maintained_boolean_queries_pin_history_and_follow_staged_changes() {
    let ((), report) = run_async_under_lab(0xbeac_b002, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let commit = c.commit();
        let query = c.query();
        let mut db = database(&commit, false).await;
        let at = db.frontier().unwrap();
        let mut options = Options::text(TEXT);
        options.vertex_label = Some(LABEL);
        let cold = db.read_session().unwrap();
        let before = cold.beacon_search(&query, &options, request(10)).unwrap();
        let handle = db
            .register_beacon_index(&query, &options, ReadPolicy::default())
            .unwrap();
        let pinned = db.read_session().unwrap();
        let mut query_only = options.clone();
        query_only.policy.max_staging_rows = 0;
        query_only.policy.max_source_scratch = 0;
        assert_eq!(
            pinned
                .beacon_search(&query, &query_only, request(10))
                .unwrap(),
            before
        );
        assert!(
            cold.beacon_search(&query, &query_only, request(10))
                .is_err()
        );
        let mut mismatch = query_only.clone();
        mismatch.index.text.as_mut().unwrap().b = 0.5;
        assert!(db.beacon_search(&query, &mismatch, request(10)).is_err());

        let mut txn = db.begin(&c.txn()).unwrap();
        let mut changes = WriteBatch::new(RelationId(1));
        changes.delete_vertex(VId(1));
        changes.set_vertex_property(VId(2), TEXT, Some(text("graph database deprecated")));
        changes.set_vertex_property(VId(4), TEXT, Some(text("graph storage")));
        changes.set_vertex_label(VId(7), LABEL, false);
        changes.create_vertex(VId(8), vec![LABEL], vec![(TEXT, text("graph database"))]);
        txn.write(&mut db, changes).unwrap();
        let staged = txn
            .beacon_search(&db, &query, &options, request(10))
            .unwrap();
        assert_eq!(ids(&staged), [VId(4), VId(8)]);
        assert_eq!(
            db.beacon_search(&query, &query_only, request(10)).unwrap(),
            before
        );
        txn.commit(&mut db, &commit).await.unwrap();
        assert_eq!(
            db.beacon_search(&query, &query_only, request(10)).unwrap(),
            staged
        );
        db.unregister_beacon_index(&query, &handle).unwrap();
        assert_eq!(
            db.beacon_search(&query, &options, request(10)).unwrap(),
            staged
        );
        let mut historical = options.clone();
        historical.as_of = Some(at);
        db.compact(&commit).await.unwrap();
        assert_eq!(
            db.beacon_search(&query, &historical, request(10)).unwrap(),
            before
        );
        drop(db);
        assert_eq!(
            pinned
                .beacon_search(&query, &query_only, request(10))
                .unwrap(),
            before
        );
        assert_eq!(c.outstanding_obligations(), 0);
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
    grant.labels = Scope::only([LABEL]);
    grant.properties = Scope::only([TEXT]);
    grant.relations = Scope::only([RelationId(1)]);
    grant
}

#[test]
fn authorized_boolean_scores_and_work_exclude_hidden_corpus_and_types() {
    let ((), report) = run_async_under_lab(0xbeac_b003, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let query = c.query();
        let full = database(&c.commit(), true).await;
        let clean = database(&c.commit(), false).await;
        let options = Options::text(TEXT);
        let issuer = Authority::new(
            AuthKey::from_seed(0xb003),
            NS,
            "host-graph",
            SchemaEpoch(0),
            1,
        )
        .unwrap();
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let expected = clean
            .beacon_search_authorized(
                &query,
                &issuer,
                &token,
                "main",
                &options,
                request(10),
                || 100,
            )
            .unwrap();
        assert_eq!(ids(&expected), [VId(1), VId(2), VId(7)]);
        assert_eq!(
            full.beacon_search_authorized(
                &query,
                &issuer,
                &token,
                "main",
                &options,
                request(10),
                || 100,
            )
            .unwrap(),
            expected
        );
        for native in [false, true] {
            let threshold = |db: &Database<MemVfs>| {
                let (mut low, mut high) = (0u64, 1_000_000u64);
                while low < high {
                    let middle = low + (high - low) / 2;
                    let mut options = options.clone();
                    let restricted = token
                        .attenuate(Restriction::MaxWork(if native {
                            1_000_000
                        } else {
                            middle
                        }))
                        .unwrap();
                    if native {
                        options.policy.max_work_units = middle as usize;
                    }
                    match db.beacon_search_authorized(
                        &query,
                        &issuer,
                        &restricted,
                        "main",
                        &options,
                        request(10),
                        || 100,
                    ) {
                        Ok(rows) => {
                            assert_eq!(rows, expected);
                            high = middle;
                        }
                        Err(ReadError::Index(BeaconError::WorkBudgetExceeded)) if native => {
                            low = middle + 1
                        }
                        Err(ReadError::Interrupted(QueryError::Authorization(
                            Denial::LimitExceeded(LimitDimension::Work),
                        ))) if !native => low = middle + 1,
                        other => panic!("unexpected Boolean work refusal: {other:?}"),
                    }
                }
                assert!(low > 0 && low < 1_000_000);
                low
            };
            assert_eq!(threshold(&full), threshold(&clean), "native={native}");
        }
        let mut definition = options.clone();
        definition.vertex_label = Some(LABEL);
        full.register_beacon_index(&query, &definition, ReadPolicy::default())
            .unwrap();
        let masked = token
            .attenuate(Restriction::Properties(Scope::only([PropertyKeyId(2)])))
            .unwrap();
        assert!(
            full.beacon_search_authorized(
                &query,
                &issuer,
                &masked,
                "main",
                &definition,
                request(10),
                || 100,
            )
            .unwrap()
            .is_empty(),
            "a matching maintained index cannot unmask text"
        );
        assert!(
            full.beacon_search(&query, &options, request(10)).is_err(),
            "the privileged control reaches the malformed hidden value"
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn cancellation_and_final_authority_checks_discard_boolean_results() {
    let ((), report) = run_async_under_lab(0xbeac_b004, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let query = c.query();
        let db = database(&c.commit(), false).await;
        let options = Options::text(TEXT);
        let issuer = Authority::new(
            AuthKey::from_seed(0xb004),
            NS,
            "host-graph",
            SchemaEpoch(0),
            1,
        )
        .unwrap();
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let cold = db.read_session().unwrap();
        db.register_beacon_index(&query, &options, ReadPolicy::default())
            .unwrap();
        let warm = db.read_session().unwrap();
        for view in [&cold, &warm] {
            for k in [0, 1] {
                let trace = Arc::new(SimulationCheckpointProbe::new(None));
                let expected = view
                    .beacon_search(
                        &query.with_checkpoint_probe(trace.clone()),
                        &options,
                        request(k),
                    )
                    .unwrap();
                assert_eq!(expected.len(), k);
                for stop in [1, trace.calls() / 2, trace.calls()] {
                    let probe = Arc::new(SimulationCheckpointProbe::new(Some(stop)));
                    let interrupted = query.with_checkpoint_probe(probe.clone());
                    assert!(matches!(
                        view.beacon_search(&interrupted, &options, request(k)),
                        Err(ReadError::Interrupted(_))
                    ));
                    assert_eq!(probe.calls(), stop);
                    assert_eq!(
                        view.beacon_search(&interrupted, &options, request(k))
                            .unwrap(),
                        expected
                    );
                }
            }
        }
        let count = Cell::new(0usize);
        db.beacon_search_authorized(
            &query,
            &issuer,
            &token,
            "main",
            &options,
            request(1),
            || {
                count.set(count.get() + 1);
                100
            },
        )
        .unwrap();
        assert!(count.get() > 2);
        for cut in [0, count.get() / 2, count.get() - 1] {
            let seen = Cell::new(0usize);
            assert!(matches!(
                db.beacon_search_authorized(
                    &query,
                    &issuer,
                    &token,
                    "main",
                    &options,
                    request(1),
                    || {
                        let at = seen.get();
                        seen.set(at + 1);
                        if at >= cut { 1000 } else { 100 }
                    },
                ),
                Err(ReadError::Interrupted(QueryError::Authorization(
                    Denial::Expired
                )))
            ));
            assert_eq!(seen.get(), cut + 1);
        }
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
