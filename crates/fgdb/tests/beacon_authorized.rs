//! Real Warden permits and embedded historical sources. These are corpus-level
//! isolation laws, not timing/I/O/resource-failure noninterference evidence.
use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb::{Database, DatabaseKeys, MemVfs, QueryError, WriteBatch};
use fgdb_beacon::read::{ReadError, ReadOptions, Rows, Search};
use fgdb_beacon::{
    BeaconError, DistanceMetric, ExactHybridQuery, ExactRrfProfile, HnswConfig, TextMatch,
    VectorSearch,
};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId, SchemaEpoch};
use fgdb_types::{
    CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId, PurposeContexts, VId,
};
use fgdb_warden::{
    Authority, Error, Grant, LimitDimension, QueryLimits, Restriction, Rights, Scope,
};
use std::cell::Cell;

type Options = ReadOptions<PropertyKeyId, LabelId>;
type SearchError = ReadError<fgdb::ReadError, QueryError>;
const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0xba; 32]);
const BRANCH: &str = "host-beacon-branch";
const T: PropertyKeyId = PropertyKeyId(1);
const X: PropertyKeyId = PropertyKeyId(2);
const SECRET: PropertyKeyId = PropertyKeyId(3);
const L: LabelId = LabelId(1);
const H: LabelId = LabelId(99);
const NOW: u64 = 100;
const EXPIRES: u64 = 1000;

fn authority(seed: u64) -> Authority {
    Authority::new(
        AuthKey::from_seed(seed),
        NS,
        "host-graph",
        SchemaEpoch(0),
        1,
    )
    .unwrap()
}
fn grant() -> Grant {
    let mut grant = Grant::read_only(
        BRANCH,
        EXPIRES,
        QueryLimits {
            max_nodes: 1000,
            max_work: 1_000_000,
            max_rows: 1000,
        },
    );
    grant.labels = Scope::only([L]);
    grant.properties = Scope::only([T, X]);
    grant.relations = Scope::only([RelationId(1)]);
    grant
}
fn options() -> Options {
    let mut options = Options::text(T);
    options.projection.vector = vec![X];
    options.index.vector = Some(HnswConfig::new(1, DistanceMetric::SquaredEuclidean));
    options
}
fn text(value: &str) -> CanonicalScalar {
    CanonicalScalar::ucs_basic_text(value).unwrap()
}
fn searches() -> [Search<'static>; 4] {
    [
        Search::Text {
            query: "red",
            k: 3,
            mode: TextMatch::Any,
        },
        Search::Vector {
            query: &[0.0],
            k: 3,
            mode: VectorSearch::Exact,
        },
        Search::Vector {
            query: &[0.0],
            k: 3,
            mode: VectorSearch::Approximate { ef_search: 16 },
        },
        Search::Hybrid(ExactHybridQuery {
            vector: &[0.0],
            text: "red",
            k: 3,
            vector_candidates: 3,
            text_candidates: 3,
            vector_mode: VectorSearch::Approximate { ef_search: 16 },
            text_mode: TextMatch::Any,
            profile: ExactRrfProfile::default(),
        }),
    ]
}
async fn database(cx: &CommitCx, hidden: bool) -> Database<MemVfs> {
    let mut db = Database::open_memory(cx, DatabaseKeys::new([0x32; 32], NS, [0x67; 32]))
        .await
        .unwrap();
    let mut batch = WriteBatch::new(RelationId(1));
    for (id, labels, word, value) in [
        (1, vec![L, H], "red", 0),
        (2, vec![L], "red red", 10),
        (3, vec![L], "blue", 20),
    ] {
        let mut props = vec![(T, text(word)), (X, CanonicalScalar::Int(value))];
        if hidden {
            props.push((SECRET, text("invalid coordinate")));
        }
        batch.create_vertex(VId(id), labels, props);
    }
    if hidden {
        // A very different hidden text population must not perturb BM25.
        for id in 100..116 {
            batch.create_vertex(
                VId(id),
                vec![H],
                vec![
                    (T, text("red red red red red red red red red")),
                    (X, CanonicalScalar::Int(1)),
                ],
            );
        }
        // Neither value's type may be examined by restricted projection.
        batch.create_vertex(
            VId(116),
            vec![H],
            vec![
                (T, CanonicalScalar::Int(8)),
                (X, CanonicalScalar::Int(16_777_217)),
            ],
        );
    }
    db.write(cx, batch).await.unwrap();
    db
}
fn refused(result: Result<Rows, SearchError>, expected: Error) {
    match result {
        Err(ReadError::Interrupted(QueryError::Authorization(error))) => {
            assert_eq!(error, expected)
        }
        other => panic!("expected {expected:?}, got {other:?}"),
    }
}
fn ids(rows: &Rows) -> Vec<VId> {
    match rows {
        Rows::Text(rows) => rows.iter().map(|r| r.id).collect(),
        Rows::Vector(rows) => rows.iter().map(|r| r.id).collect(),
        Rows::Hybrid(rows) => rows.iter().map(|r| r.id).collect(),
    }
}

#[test]
fn hidden_documents_never_enter_bm25_statistics_ann_routing_or_fusion() {
    let ((), report) = run_async_under_lab(0xbeac_2001, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let full = database(&c.commit(), true).await;
        let scrubbed = database(&c.commit(), false).await;
        let issuer = authority(2001);
        let token = issuer.issue_at(&grant(), NOW).unwrap();
        for query in searches() {
            let rows = full
                .beacon_search_authorized(
                    &c.query(),
                    &issuer,
                    &token,
                    BRANCH,
                    &options(),
                    query,
                    || NOW,
                )
                .unwrap();
            let clean = scrubbed
                .beacon_search_authorized(
                    &c.query(),
                    &issuer,
                    &token,
                    BRANCH,
                    &options(),
                    query,
                    || NOW,
                )
                .unwrap();
            assert_eq!(rows, clean); // Includes BM25/distance/fusion values, not only IDs.
            assert_eq!(
                rows,
                scrubbed
                    .beacon_search(&c.query(), &options(), query)
                    .unwrap()
            );
            assert!(ids(&rows).iter().all(|id| id.0 < 100));
        }
        // The hidden invalid scalar really would fail unscoped execution.
        assert!(
            full.beacon_search(&c.query(), &options(), searches()[0])
                .is_err()
        );
        assert!(
            full.beacon_search(&c.query(), &options(), searches()[1])
                .is_err()
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn forbidden_properties_are_absent_before_type_checks_and_coordinate_assembly() {
    let ((), report) = run_async_under_lab(0xbeac_2002, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let full = database(&c.commit(), true).await;
        let clean = database(&c.commit(), false).await;
        let issuer = authority(2002);
        let token = issuer.issue_at(&grant(), NOW).unwrap();
        let mut o = options();
        o.projection.text = Some(SECRET);
        for query in [searches()[0], searches()[3]] {
            let rows = full
                .beacon_search_authorized(&c.query(), &issuer, &token, BRANCH, &o, query, || NOW)
                .unwrap();
            assert_eq!(
                rows,
                clean
                    .beacon_search_authorized(
                        &c.query(),
                        &issuer,
                        &token,
                        BRANCH,
                        &o,
                        query,
                        || NOW
                    )
                    .unwrap()
            );
            if let Rows::Text(rows) = rows {
                assert!(rows.is_empty());
            }
        }
        o = options();
        o.projection.vector = vec![X, SECRET];
        o.index.vector.as_mut().unwrap().dimensions = 2;
        let query = Search::Vector {
            query: &[0.0, 0.0],
            k: 3,
            mode: VectorSearch::Exact,
        };
        assert!(
            full.beacon_search_authorized(&c.query(), &issuer, &token, BRANCH, &o, query, || NOW)
                .unwrap()
                .is_empty()
        );
        // A later positive scope cannot undo an explicit property denial.
        let denied = token
            .attenuate(Restriction::DenyProperties([T].into()))
            .unwrap()
            .attenuate(Restriction::Properties(Scope::All))
            .unwrap();
        assert!(
            full.beacon_search_authorized(
                &c.query(),
                &issuer,
                &denied,
                BRANCH,
                &options(),
                searches()[0],
                || NOW
            )
            .unwrap()
            .is_empty()
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn original_label_clauses_and_masked_user_selection_are_separate() {
    let ((), report) = run_async_under_lab(0xbeac_2003, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let db = database(&c.commit(), true).await;
        let issuer = authority(2003);
        let token = issuer.issue_at(&grant(), NOW).unwrap();
        let both = token
            .attenuate(Restriction::Labels(Scope::only([H])))
            .unwrap();
        let rows = db
            .beacon_search_authorized(
                &c.query(),
                &issuer,
                &both,
                BRANCH,
                &options(),
                searches()[0],
                || NOW,
            )
            .unwrap();
        assert_eq!(ids(&rows), [VId(1)]); // L AND H, not intersection(L,H).
        for label in [L, H] {
            let mut o = options();
            o.vertex_label = Some(label);
            assert!(
                db.beacon_search_authorized(
                    &c.query(),
                    &issuer,
                    &both,
                    BRANCH,
                    &o,
                    searches()[0],
                    || NOW
                )
                .unwrap()
                .is_empty()
            );
        }
        let mut o = options();
        o.vertex_label = Some(H);
        assert!(
            db.beacon_search_authorized(
                &c.query(),
                &issuer,
                &token,
                BRANCH,
                &o,
                searches()[0],
                || NOW
            )
            .unwrap()
            .is_empty()
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn hidden_successors_do_not_resurrect_older_allowed_versions() {
    let ((), report) = run_async_under_lab(0xbeac_2004, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let mut db = database(&c.commit(), false).await;
        let issuer = authority(2004);
        let token = issuer.issue_at(&grant(), NOW).unwrap();
        let mut historical = options();
        historical.as_of = Some(db.read_session().unwrap().frontier());
        let before: Vec<_> = searches()
            .into_iter()
            .map(|q| {
                db.beacon_search_authorized(
                    &c.query(),
                    &issuer,
                    &token,
                    BRANCH,
                    &options(),
                    q,
                    || NOW,
                )
                .unwrap()
            })
            .collect();
        let mut update = WriteBatch::new(RelationId(1));
        update.set_vertex_label(VId(1), L, false);
        update.set_vertex_property(VId(1), T, Some(CanonicalScalar::Int(77)));
        update.set_vertex_property(VId(1), X, Some(CanonicalScalar::Int(16_777_217)));
        update.delete_vertex(VId(2));
        db.write(&c.commit(), update).await.unwrap();
        for (q, expected) in searches().into_iter().zip(before) {
            assert_eq!(
                db.beacon_search_authorized(
                    &c.query(),
                    &issuer,
                    &token,
                    BRANCH,
                    &historical,
                    q,
                    || NOW
                )
                .unwrap(),
                expected
            );
            let now = db
                .beacon_search_authorized(
                    &c.query(),
                    &issuer,
                    &token,
                    BRANCH,
                    &options(),
                    q,
                    || NOW,
                )
                .unwrap();
            assert!(ids(&now).iter().all(|id| *id == VId(3)));
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn signed_allowances_cover_admission_native_work_and_actual_delivery() {
    let ((), report) = run_async_under_lab(0xbeac_2005, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let db = database(&c.commit(), true).await;
        let issuer = authority(2005);
        let token = issuer.issue_at(&grant(), NOW).unwrap();
        for (restriction, dimension) in [
            (Restriction::MaxNodes(2), LimitDimension::Nodes),
            (Restriction::MaxWork(0), LimitDimension::Work),
            (Restriction::MaxRows(2), LimitDimension::Rows),
        ] {
            let restricted = token.attenuate(restriction).unwrap();
            refused(
                db.beacon_search_authorized(
                    &c.query(),
                    &issuer,
                    &restricted,
                    BRANCH,
                    &options(),
                    searches()[1],
                    || NOW,
                ),
                Error::LimitExceeded(dimension),
            );
        }
        let exact = token
            .attenuate(Restriction::MaxNodes(3))
            .unwrap()
            .attenuate(Restriction::MaxRows(3))
            .unwrap();
        for q in searches() {
            assert!(
                db.beacon_search_authorized(
                    &c.query(),
                    &issuer,
                    &exact,
                    BRANCH,
                    &options(),
                    q,
                    || NOW
                )
                .is_ok()
            );
        }
        // Signed node accounting is before user selection, even when it matches nothing.
        let mut none = options();
        none.vertex_label = Some(LabelId(123));
        let nodes = token.attenuate(Restriction::MaxNodes(2)).unwrap();
        refused(
            db.beacon_search_authorized(
                &c.query(),
                &issuer,
                &nodes,
                BRANCH,
                &none,
                searches()[0],
                || NOW,
            ),
            Error::LimitExceeded(LimitDimension::Nodes),
        );
        let mut native = options();
        native.policy.max_work_units = 0;
        assert!(matches!(
            db.beacon_search_authorized(
                &c.query(),
                &issuer,
                &token,
                BRANCH,
                &native,
                searches()[0],
                || NOW
            ),
            Err(ReadError::Index(BeaconError::WorkBudgetExceeded))
        ));
        let zero = token.attenuate(Restriction::MaxRows(0)).unwrap();
        let q = Search::Text {
            query: "red",
            k: 0,
            mode: TextMatch::Any,
        };
        assert!(
            db.beacon_search_authorized(&c.query(), &issuer, &zero, BRANCH, &options(), q, || NOW)
                .unwrap()
                .is_empty()
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn every_clock_boundary_including_empty_delivery_can_expire_without_results() {
    let ((), report) = run_async_under_lab(0xbeac_2006, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let db = database(&c.commit(), false).await;
        let issuer = authority(2006);
        let token = issuer.issue_at(&grant(), NOW).unwrap();
        for q in searches().into_iter().chain([
            Search::Text {
                query: "absent",
                k: 3,
                mode: TextMatch::Any,
            },
            Search::Text {
                query: "red",
                k: 0,
                mode: TextMatch::Any,
            },
        ]) {
            let count = Cell::new(0usize);
            db.beacon_search_authorized(&c.query(), &issuer, &token, BRANCH, &options(), q, || {
                count.set(count.get() + 1);
                NOW
            })
            .unwrap();
            assert!(count.get() > 10);
            for cut in 0..count.get() {
                let seen = Cell::new(0usize);
                refused(
                    db.beacon_search_authorized(
                        &c.query(),
                        &issuer,
                        &token,
                        BRANCH,
                        &options(),
                        q,
                        || {
                            let at = seen.get();
                            seen.set(at + 1);
                            if at >= cut { EXPIRES } else { NOW }
                        },
                    ),
                    Error::Expired,
                );
                assert_eq!(seen.get(), cut + 1, "execution continued after expiry");
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn live_retirement_and_clock_rollback_retain_their_original_typed_causes() {
    let ((), report) = run_async_under_lab(0xbeac_2007, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let db = database(&c.commit(), false).await;
        for q in [
            searches()[0],
            searches()[3],
            Search::Text {
                query: "absent",
                k: 3,
                mode: TextMatch::Any,
            },
        ] {
            let issuer = authority(2007);
            let token = issuer.issue_at(&grant(), NOW).unwrap();
            let count = Cell::new(0usize);
            db.beacon_search_authorized(&c.query(), &issuer, &token, BRANCH, &options(), q, || {
                count.set(count.get() + 1);
                NOW
            })
            .unwrap();
            for cut in [0, 1, count.get() / 2, count.get() - 2, count.get() - 1] {
                let issuer = authority(2007);
                let token = issuer.issue_at(&grant(), NOW).unwrap();
                let seen = Cell::new(0usize);
                refused(
                    db.beacon_search_authorized(
                        &c.query(),
                        &issuer,
                        &token,
                        BRANCH,
                        &options(),
                        q,
                        || {
                            if seen.get() == cut {
                                issuer.retire();
                            }
                            seen.set(seen.get() + 1);
                            NOW
                        },
                    ),
                    Error::AuthorityRetired,
                );
            }
            let seen = Cell::new(0usize);
            refused(
                db.beacon_search_authorized(
                    &c.query(),
                    &issuer,
                    &token,
                    BRANCH,
                    &options(),
                    q,
                    || {
                        let at = seen.get();
                        seen.set(at + 1);
                        if at == 0 { NOW + 1 } else { NOW }
                    },
                ),
                Error::ClockWentBackwards,
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn authentication_precedes_bad_frontier_or_data_and_does_not_grant_read_rights() {
    let ((), report) = run_async_under_lab(0xbeac_2008, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let db = database(&c.commit(), true).await;
        let issuer = authority(2008);
        let token = issuer.issue_at(&grant(), NOW).unwrap();
        let mut o = options();
        o.as_of = Some(CommitSeq(u64::MAX));
        let foreign = Authority::new(
            AuthKey::from_seed(2008),
            DatabaseSecurityNamespaceId([0x33; 32]),
            "host-graph",
            SchemaEpoch(0),
            1,
        )
        .unwrap();
        refused(
            db.beacon_search_authorized(
                &c.query(),
                &foreign,
                &token,
                BRANCH,
                &o,
                searches()[0],
                || NOW,
            ),
            Error::WrongAuthority,
        );
        let other_key = authority(2009);
        refused(
            db.beacon_search_authorized(
                &c.query(),
                &other_key,
                &token,
                BRANCH,
                &o,
                searches()[0],
                || NOW,
            ),
            Error::Unauthenticated,
        );
        refused(
            db.beacon_search_authorized(
                &c.query(),
                &issuer,
                &token,
                "different-branch",
                &o,
                searches()[0],
                || NOW,
            ),
            Error::ScopeDenied,
        );
        let write = token.attenuate(Restriction::Rights(Rights::Write)).unwrap();
        refused(
            db.beacon_search_authorized(
                &c.query(),
                &issuer,
                &write,
                BRANCH,
                &o,
                searches()[0],
                || NOW,
            ),
            Error::PermissionDenied,
        );
        assert!(matches!(
            db.beacon_search_authorized(
                &c.query(),
                &issuer,
                &token,
                BRANCH,
                &o,
                searches()[0],
                || NOW
            ),
            Err(ReadError::Read(_))
        ));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
