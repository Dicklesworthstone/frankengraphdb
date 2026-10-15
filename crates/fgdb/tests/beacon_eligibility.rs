//! Eligibility on reusable native Beacon generations: select candidates before
//! truncation, without changing the corpus, source cut or Warden authority.
#![forbid(unsafe_code)]
#![recursion_limit = "256"]

use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb::{Database, DatabaseKeys, MaintainedBeaconError, MemVfs, QueryError, WriteBatch};
use fgdb_beacon::read::{ReadError as SearchError, ReadOptions, ReadPolicy, Rows, Search};
use fgdb_beacon::{
    BeaconError, DistanceMetric, ExactHybridQuery, ExactRrfProfile, HnswConfig, TextMatch,
    VectorSearch,
};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId, SchemaEpoch};
use fgdb_gql::{GqlQueryPolicy, GraphSymbol, GraphSymbolKind};
use fgdb_types::{CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, PurposeContexts, VId};
use fgdb_warden::{Authority, Error as WardenError, Grant, QueryLimits, Scope};
use std::cell::Cell;

const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x35; 32]);
const VISIBLE: LabelId = LabelId(1);
const HIDDEN: LabelId = LabelId(99);
const TEXT: PropertyKeyId = PropertyKeyId(1);
const VECTOR: PropertyKeyId = PropertyKeyId(2);
const SECRET: PropertyKeyId = PropertyKeyId(99);
const NOW: u64 = 100;
const EXPIRES: u64 = 10_000;

fn options() -> ReadOptions<PropertyKeyId, LabelId> {
    let mut options = ReadOptions::text(TEXT);
    options.vertex_label = Some(VISIBLE);
    options.projection.vector = vec![VECTOR];
    options.index.vector = Some(HnswConfig::new(1, DistanceMetric::SquaredEuclidean));
    options
}
fn text(value: &str) -> CanonicalScalar {
    CanonicalScalar::ucs_basic_text(value).unwrap()
}
fn issuer() -> Authority {
    Authority::new(AuthKey::from_seed(0x35), NS, "graph", SchemaEpoch(0), 1).unwrap()
}
fn grant() -> Grant {
    let mut grant = Grant::read_only(
        "main",
        EXPIRES,
        QueryLimits {
            max_nodes: 3,
            max_work: 1_000_000,
            max_rows: 3,
        },
    );
    grant.labels = Scope::only([VISIBLE]);
    grant.properties = Scope::only([TEXT, VECTOR]);
    grant
}
fn host() -> GqlQueryPolicy {
    GqlQueryPolicy::new(3, 3, 1_000_000, 1000)
}
fn catalog(_: GraphSymbolKind, _: &str) -> Option<GraphSymbol> {
    None
}
async fn database(cx: &CommitCx, hidden: bool) -> Database<MemVfs> {
    let mut db = Database::open_memory(cx, DatabaseKeys::new([0x36; 32], NS, [0x37; 32]))
        .await
        .unwrap();
    let mut batch = WriteBatch::new(RelationId(1));
    // The first two text scores tie, so unfiltered top-1 is identity 1.
    // Identity 2 is also beyond the vector top-1. A post-filter loses it.
    for (id, words, coordinate) in [(1, "red", 0), (2, "red", 2), (3, "blue", 3)] {
        let mut labels = vec![VISIBLE];
        let mut properties = vec![
            (TEXT, text(words)),
            (VECTOR, CanonicalScalar::Int(coordinate)),
        ];
        if hidden {
            labels.push(HIDDEN);
            properties.push((SECRET, text("must not escape")));
        }
        batch.create_vertex(VId(id), labels, properties);
    }
    if hidden {
        for id in 100..132 {
            // Hidden malformed projections must never be inspected or passed
            // to eligibility. The visible-source budgets admit only 3 rows.
            batch.create_vertex(
                VId(id),
                vec![HIDDEN],
                vec![
                    (TEXT, CanonicalScalar::Int(7)),
                    (VECTOR, text("not a vector")),
                ],
            );
        }
    }
    db.write(cx, batch).await.unwrap();
    db
}
fn queries() -> [Search<'static>; 5] {
    [
        Search::Text {
            query: "red",
            k: 1,
            mode: TextMatch::Any,
        },
        Search::Text {
            query: "red",
            k: 1,
            mode: TextMatch::Phrase,
        },
        Search::Vector {
            query: &[0.0],
            k: 1,
            mode: VectorSearch::Exact,
        },
        Search::Vector {
            query: &[0.0],
            k: 1,
            mode: VectorSearch::Approximate { ef_search: 16 },
        },
        Search::Hybrid(ExactHybridQuery {
            vector: &[0.0],
            text: "red",
            k: 1,
            vector_candidates: 1,
            text_candidates: 1,
            vector_mode: VectorSearch::Exact,
            text_mode: TextMatch::Any,
            profile: ExactRrfProfile::default(),
        }),
    ]
}
fn ids(rows: &Rows) -> Vec<VId> {
    match rows {
        Rows::Text(rows) => rows.iter().map(|hit| hit.id).collect(),
        Rows::Vector(rows) => rows.iter().map(|hit| hit.id).collect(),
        Rows::Hybrid(rows) => rows.iter().map(|hit| hit.id).collect(),
    }
}

#[test]
fn resident_pinned_and_maintained_search_filter_before_each_candidate_limit() {
    let ((), report) = run_async_under_lab(0xbeac_5101, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let mut db = database(&c.commit(), false).await;
        let resident = db.prepare_beacon_index(&c.query(), &options()).unwrap();
        let pin = resident.snapshot();
        let handle = db
            .register_beacon_index(&c.query(), &options(), ReadPolicy::default())
            .unwrap();
        for query in queries() {
            let expected = db
                .beacon_search_filtered(&c.query(), &options(), query, |id| id == VId(2))
                .unwrap();
            assert_eq!(ids(&expected), [VId(2)]);
            let unfiltered = resident
                .search(&c.query(), query, ReadPolicy::default())
                .unwrap();
            assert_eq!(ids(&unfiltered), [VId(1)]);
            for answer in [
                resident
                    .search_filtered(&c.query(), query, ReadPolicy::default(), |id| id == VId(2)),
                pin.search_filtered(&c.query(), query, ReadPolicy::default(), |id| id == VId(2)),
            ] {
                assert_eq!(answer.unwrap(), expected);
            }
            assert_eq!(
                db.search_beacon_index_filtered(
                    &c.query(),
                    &handle,
                    query,
                    ReadPolicy::default(),
                    |id| id == VId(2),
                )
                .unwrap(),
                expected,
            );
            if let Rows::Hybrid(hits) = expected {
                assert_eq!(hits[0].vector_rank, std::num::NonZeroU32::new(1));
                assert_eq!(hits[0].text_rank, std::num::NonZeroU32::new(1));
                assert_eq!(hits[0].score.numerator(), 2);
                assert_eq!(hits[0].score.denominator(), 61);
            }
            assert_eq!(pin.stats().documents, 3);
            assert!(
                pin.search_filtered(&c.query(), query, ReadPolicy::default(), |_| false)
                    .unwrap()
                    .is_empty()
            );
            assert_eq!(
                resident
                    .search(&c.query(), query, ReadPolicy::default())
                    .unwrap(),
                unfiltered
            );
        }
        // Ordinary pins need no live database to apply a new eligibility set.
        drop(db);
        assert_eq!(
            ids(&pin
                .search_filtered(&c.query(), queries()[2], ReadPolicy::default(), |id| id
                    == VId(2),)
                .unwrap()),
            [VId(2)]
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn committed_updates_advance_only_maintained_search_while_old_pins_stay_historical() {
    let ((), report) = run_async_under_lab(0xbeac_5104, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let mut db = database(&c.commit(), false).await;
        let handle = db
            .register_beacon_index(&c.query(), &options(), ReadPolicy::default())
            .unwrap();
        let pin = db.beacon_index_snapshot(&c.query(), &handle).unwrap();
        let before = pin.source_sequence();
        let mut update = WriteBatch::new(RelationId(1));
        update.delete_vertex(VId(2));
        db.write(&c.commit(), update).await.unwrap();
        let status = db.beacon_index_status(&c.query(), &handle).unwrap();
        assert!(status.failure.is_none());
        assert_eq!(status.source_sequence, db.frontier().unwrap());
        assert_ne!(status.source_sequence, before);
        for query in queries() {
            assert!(
                db.search_beacon_index_filtered(
                    &c.query(),
                    &handle,
                    query,
                    ReadPolicy::default(),
                    |id| id == VId(2),
                )
                .unwrap()
                .is_empty()
            );
            assert_eq!(
                ids(&pin
                    .search_filtered(&c.query(), query, ReadPolicy::default(), |id| id == VId(2),)
                    .unwrap()),
                [VId(2)]
            );
        }
        assert_eq!(pin.source_sequence(), before);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn unavailable_foreign_and_removed_maintained_handles_refuse_before_predicates() {
    let ((), report) = run_async_under_lab(0xbeac_5105, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let mut db = database(&c.commit(), false).await;
        let other = database(&c.commit(), false).await;
        let handle = db
            .register_beacon_index(
                &c.query(),
                &options(),
                ReadPolicy {
                    max_work_units: 0,
                    ..ReadPolicy::default()
                },
            )
            .unwrap();
        let old = db.beacon_index_snapshot(&c.query(), &handle).unwrap();
        assert!(matches!(
            other.search_beacon_index_filtered(
                &c.query(),
                &handle,
                queries()[0],
                ReadPolicy::default(),
                |_| panic!("foreign corpus"),
            ),
            Err(MaintainedBeaconError::ForeignHandle)
        ));
        let mut update = WriteBatch::new(RelationId(1));
        update.set_vertex_property(VId(1), TEXT, Some(text("changed")));
        db.write(&c.commit(), update).await.unwrap();
        assert!(matches!(
            db.search_beacon_index_filtered(
                &c.query(),
                &handle,
                queries()[0],
                ReadPolicy::default(),
                |_| panic!("fenced corpus"),
            ),
            Err(MaintainedBeaconError::Unavailable { .. })
        ));
        assert_eq!(
            ids(&old
                .search_filtered(&c.query(), queries()[0], ReadPolicy::default(), |id| id
                    == VId(2),)
                .unwrap()),
            [VId(2)]
        );
        db.unregister_beacon_index(&c.query(), &handle).unwrap();
        assert!(matches!(
            db.search_beacon_index_filtered(
                &c.query(),
                &handle,
                queries()[0],
                ReadPolicy::default(),
                |_| panic!("removed corpus"),
            ),
            Err(MaintainedBeaconError::UnknownHandle)
        ));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn scoped_one_shot_and_prepared_selectors_do_not_admit_hidden_corpus_ids() {
    let ((), report) = run_async_under_lab(0xbeac_5102, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let db = database(&c.commit(), true).await;
        let clean = database(&c.commit(), false).await;
        let issuer = issuer();
        let token = issuer.issue_at(&grant(), NOW).unwrap();
        let mut session = db
            .authorized_read_session(&c.query(), &issuer, &token, "main", catalog, host(), || NOW)
            .unwrap();
        let prepared = session
            .prepare_beacon_index(&c.query(), &options())
            .unwrap();
        for query in queries() {
            let expected = clean
                .beacon_search_filtered(&c.query(), &options(), query, |id| id == VId(2))
                .unwrap();
            // Hidden and unknown caller-supplied IDs are ordinary nonmatches.
            // There is no callback capable of enumerating internal candidates.
            let eligible = [VId(2), VId(100), VId(u128::MAX)];
            assert_eq!(
                session
                    .search_beacon_index_filtered(
                        &c.query(),
                        &prepared,
                        query,
                        ReadPolicy::default(),
                        &eligible,
                    )
                    .unwrap(),
                expected
            );
            assert_eq!(
                session
                    .beacon_search_filtered(&c.query(), &options(), query, &eligible,)
                    .unwrap(),
                expected
            );
            assert!(
                session
                    .search_beacon_index_filtered(
                        &c.query(),
                        &prepared,
                        query,
                        ReadPolicy::default(),
                        &[VId(100)],
                    )
                    .unwrap()
                    .is_empty()
            );
        }
        session.close();
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn filtering_preserves_full_corpus_text_scores_and_does_not_stick_to_a_handle() {
    let ((), report) = run_async_under_lab(0xbeac_5103, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let db = database(&c.commit(), false).await;
        let issuer = issuer();
        let token = issuer.issue_at(&grant(), NOW).unwrap();
        let mut session = db
            .authorized_read_session(&c.query(), &issuer, &token, "main", catalog, host(), || NOW)
            .unwrap();
        let prepared = session
            .prepare_beacon_index(&c.query(), &options())
            .unwrap();
        let all_query = Search::Text {
            query: "red",
            k: 3,
            mode: TextMatch::Any,
        };
        // The established unfiltered one-shot path is the corpus-score oracle,
        // not an index built from only the eligible documents.
        let Rows::Text(full) = db.beacon_search(&c.query(), &options(), all_query).unwrap() else {
            panic!("text rows");
        };
        assert_eq!(full.len(), 2);
        for target in [2, 1, 2] {
            let expected = Rows::Text(
                full.iter()
                    .filter(|hit| hit.id == VId(target))
                    .cloned()
                    .collect(),
            );
            assert_eq!(
                session
                    .search_beacon_index_filtered(
                        &c.query(),
                        &prepared,
                        queries()[0],
                        ReadPolicy::default(),
                        &[VId(target)],
                    )
                    .unwrap(),
                expected
            );
        }
        assert!(
            session
                .search_beacon_index_filtered(
                    &c.query(),
                    &prepared,
                    all_query,
                    ReadPolicy::default(),
                    &[],
                )
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            session
                .search_beacon_index(&c.query(), &prepared, all_query, ReadPolicy::default(),)
                .unwrap(),
            Rows::Text(full)
        );
        session.close();
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn foreign_session_expiry_and_result_limits_precede_selector_validation() {
    let ((), report) = run_async_under_lab(0xbeac_5106, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let db = database(&c.commit(), true).await;
        let issuer = issuer();
        let token = issuer.issue_at(&grant(), NOW).unwrap();
        let now = Cell::new(NOW);
        let mut session = db
            .authorized_read_session(&c.query(), &issuer, &token, "main", catalog, host(), || {
                now.get()
            })
            .unwrap();
        let prepared = session
            .prepare_beacon_index(&c.query(), &options())
            .unwrap();
        let mut foreign = db
            .authorized_read_session(&c.query(), &issuer, &token, "main", catalog, host(), || NOW)
            .unwrap();
        assert!(matches!(
            foreign.search_beacon_index_filtered(
                &c.query(),
                &prepared,
                queries()[0],
                ReadPolicy::default(),
                &[VId(2), VId(2)],
            ),
            Err(SearchError::Interrupted(QueryError::Authorization(
                WardenError::ScopeDenied
            )))
        ));
        assert!(!foreign.is_closed());
        let too_many = Search::Text {
            query: "red",
            k: 4,
            mode: TextMatch::Any,
        };
        assert!(
            session
                .search_beacon_index_filtered(
                    &c.query(),
                    &prepared,
                    too_many,
                    ReadPolicy::default(),
                    &[],
                )
                .is_err()
        );
        assert!(
            session
                .search_beacon_index_filtered(
                    &c.query(),
                    &prepared,
                    queries()[0],
                    ReadPolicy {
                        max_work_units: 0,
                        ..ReadPolicy::default()
                    },
                    &[],
                )
                .is_err()
        );
        now.set(EXPIRES + 1);
        assert!(matches!(
            session.search_beacon_index_filtered(
                &c.query(),
                &prepared,
                queries()[0],
                ReadPolicy::default(),
                &[VId(2), VId(2)],
            ),
            Err(SearchError::Interrupted(QueryError::Authorization(
                WardenError::Expired
            )))
        ));
        assert!(session.is_closed());
        foreign.close();
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn reject_all_cannot_hide_invalid_visible_source_values() {
    let ((), report) = run_async_under_lab(0xbeac_5107, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let mut db = database(&c.commit(), true).await;
        let mut update = WriteBatch::new(RelationId(1));
        update.set_vertex_property(VId(2), VECTOR, Some(text("invalid visible coordinate")));
        db.write(&c.commit(), update).await.unwrap();
        let issuer = issuer();
        let token = issuer.issue_at(&grant(), NOW).unwrap();
        let mut session = db
            .authorized_read_session(&c.query(), &issuer, &token, "main", catalog, host(), || NOW)
            .unwrap();
        assert!(matches!(
            session.beacon_search_filtered(&c.query(), &options(), queries()[2], &[],),
            Err(SearchError::Index(BeaconError::InvalidQuery(_)))
        ));
        assert!(!session.is_closed());
        // A text-only request still does not inspect the disabled vector lane.
        assert!(
            session
                .beacon_search_filtered(&c.query(), &options(), queries()[0], &[],)
                .unwrap()
                .is_empty()
        );
        session.close();
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn selectors_are_canonical_bounded_and_charged_without_retaining_a_filter() {
    let ((), report) = run_async_under_lab(0xbeac_5108, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let db = database(&c.commit(), true).await;
        let issuer = issuer();
        let token = issuer.issue_at(&grant(), NOW).unwrap();
        let mut session = db
            .authorized_read_session(&c.query(), &issuer, &token, "main", catalog, host(), || NOW)
            .unwrap();
        let prepared = session
            .prepare_beacon_index(&c.query(), &options())
            .unwrap();
        for invalid in [[VId(2), VId(2)], [VId(2), VId(1)]] {
            assert!(matches!(
                session.search_beacon_index_filtered(
                    &c.query(),
                    &prepared,
                    queries()[0],
                    ReadPolicy::default(),
                    &invalid,
                ),
                Err(SearchError::Index(BeaconError::InvalidQuery(_)))
            ));
            assert!(matches!(
                session.beacon_search_filtered(&c.query(), &options(), queries()[0], &invalid,),
                Err(SearchError::Index(BeaconError::InvalidQuery(_)))
            ));
            assert!(!session.is_closed());
        }
        let at_limit: Vec<_> = (0..100_000).map(VId).collect();
        assert_eq!(
            ids(&session
                .search_beacon_index_filtered(
                    &c.query(),
                    &prepared,
                    queries()[0],
                    ReadPolicy::default(),
                    &at_limit,
                )
                .unwrap()),
            [VId(1)]
        );
        let oversized: Vec<_> = (0..100_001).map(VId).collect();
        assert!(matches!(
            session.search_beacon_index_filtered(
                &c.query(),
                &prepared,
                queries()[0],
                ReadPolicy::default(),
                &oversized,
            ),
            Err(SearchError::Index(BeaconError::ResourceLimit {
                resource: "eligible identities",
                limit: 100_000,
            }))
        ));
        let empty_work = (0..5000)
            .find(|&limit| {
                session
                    .search_beacon_index_filtered(
                        &c.query(),
                        &prepared,
                        queries()[0],
                        ReadPolicy {
                            max_work_units: limit,
                            ..ReadPolicy::default()
                        },
                        &[],
                    )
                    .is_ok()
            })
            .expect("small fixture has a bounded empty-selector search");
        let many: Vec<_> = (0..empty_work + 2)
            .map(|id| VId(id as u128 + 1000))
            .collect();
        // The selector alone now exceeds an allowance that admits the entire
        // same search with no eligible IDs. Failure cannot mutate the handle.
        assert!(
            session
                .search_beacon_index_filtered(
                    &c.query(),
                    &prepared,
                    queries()[0],
                    ReadPolicy {
                        max_work_units: empty_work,
                        ..ReadPolicy::default()
                    },
                    &many,
                )
                .is_err()
        );
        assert_eq!(
            ids(&session
                .search_beacon_index_filtered(
                    &c.query(),
                    &prepared,
                    queries()[0],
                    ReadPolicy::default(),
                    &[VId(2)],
                )
                .unwrap()),
            [VId(2)]
        );
        issuer.retire();
        assert!(matches!(
            session.search_beacon_index_filtered(
                &c.query(),
                &prepared,
                queries()[0],
                ReadPolicy::default(),
                &[],
            ),
            Err(SearchError::Interrupted(QueryError::Authorization(
                WardenError::AuthorityRetired
            )))
        ));
        assert!(session.is_closed());
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn every_live_clock_cut_refuses_before_filtered_rows_can_escape() {
    let ((), report) = run_async_under_lab(0xbeac_5109, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let db = database(&c.commit(), true).await;
        let issuer = issuer();
        let token = issuer.issue_at(&grant(), NOW).unwrap();
        for selected in [&[VId(2)][..], &[][..]] {
            let calls = Cell::new(0usize);
            let expire_at = Cell::new(None::<usize>);
            let clock = || {
                let at = calls.get() + 1;
                calls.set(at);
                if expire_at.get().is_some_and(|cut| at >= cut) {
                    EXPIRES + 1
                } else {
                    NOW
                }
            };
            let mut session = db
                .authorized_read_session(
                    &c.query(),
                    &issuer,
                    &token,
                    "main",
                    catalog,
                    host(),
                    clock,
                )
                .unwrap();
            let prepared = session
                .prepare_beacon_index(&c.query(), &options())
                .unwrap();
            calls.set(0);
            let answer = session
                .search_beacon_index_filtered(
                    &c.query(),
                    &prepared,
                    queries()[0],
                    ReadPolicy::default(),
                    selected,
                )
                .unwrap();
            assert_eq!(ids(&answer), selected);
            let cuts = calls.get();
            assert!(cuts > 0);
            session.close();
            for cut in 1..=cuts {
                expire_at.set(None);
                let mut session = db
                    .authorized_read_session(
                        &c.query(),
                        &issuer,
                        &token,
                        "main",
                        catalog,
                        host(),
                        clock,
                    )
                    .unwrap();
                let prepared = session
                    .prepare_beacon_index(&c.query(), &options())
                    .unwrap();
                calls.set(0);
                expire_at.set(Some(cut));
                assert!(matches!(
                    session.search_beacon_index_filtered(
                        &c.query(),
                        &prepared,
                        queries()[0],
                        ReadPolicy::default(),
                        selected,
                    ),
                    Err(SearchError::Interrupted(QueryError::Authorization(
                        WardenError::Expired
                    )))
                ));
                assert!(session.is_closed());
            }
        }
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
