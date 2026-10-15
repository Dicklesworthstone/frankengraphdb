//! Eligibility on reusable native Beacon generations: select candidates before
//! truncation, without changing the corpus, source cut or Warden authority.
#![forbid(unsafe_code)]
#![recursion_limit = "256"]

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MaintainedBeaconError, MemVfs, WriteBatch};
use fgdb_beacon::read::{ReadOptions, ReadPolicy, Rows, Search};
use fgdb_beacon::{
    DistanceMetric, ExactHybridQuery, ExactRrfProfile, HnswConfig, TextMatch, VectorSearch,
};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_types::{CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, PurposeContexts, VId};

const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x35; 32]);
const VISIBLE: LabelId = LabelId(1);
const HIDDEN: LabelId = LabelId(99);
const TEXT: PropertyKeyId = PropertyKeyId(1);
const VECTOR: PropertyKeyId = PropertyKeyId(2);
const SECRET: PropertyKeyId = PropertyKeyId(99);

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
async fn database(cx: &CommitCx, hidden: bool) -> Database<MemVfs> {
    let mut db = Database::open_memory(cx, DatabaseKeys::new([0x36; 32], NS, [0x37; 32]))
        .await
        .unwrap();
    let mut batch = WriteBatch::new(RelationId(1));
    // The first two text scores tie, so unfiltered top-1 is identity 1.
    // Identity 2 is also beyond the vector top-1. A post-filter loses it.
    for (id, words, coordinate) in [(1, "red", 0), (2, "red", 2), (3, "blue", 3)] {
        let mut labels = vec![VISIBLE];
        let mut properties = vec![(TEXT, text(words)), (VECTOR, CanonicalScalar::Int(coordinate))];
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
                vec![(TEXT, CanonicalScalar::Int(7)), (VECTOR, text("not a vector"))],
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
            let unfiltered = resident.search(&c.query(), query, ReadPolicy::default()).unwrap();
            assert_eq!(ids(&unfiltered), [VId(1)]);
            for answer in [
                resident.search_filtered(&c.query(), query, ReadPolicy::default(), |id| id == VId(2)),
                pin.search_filtered(&c.query(), query, ReadPolicy::default(), |id| id == VId(2)),
            ] {
                assert_eq!(answer.unwrap(), expected);
            }
            assert_eq!(
                db.search_beacon_index_filtered(
                    &c.query(), &handle, query, ReadPolicy::default(), |id| id == VId(2),
                ).unwrap(),
                expected,
            );
            if let Rows::Hybrid(hits) = expected {
                assert_eq!(hits[0].vector_rank, std::num::NonZeroU32::new(1));
                assert_eq!(hits[0].text_rank, std::num::NonZeroU32::new(1));
                assert_eq!(hits[0].score.numerator(), 2);
                assert_eq!(hits[0].score.denominator(), 61);
            }
            assert_eq!(pin.stats().documents, 3);
            assert!(pin.search_filtered(&c.query(), query, ReadPolicy::default(), |_| false)
                .unwrap().is_empty());
            assert_eq!(resident.search(&c.query(), query, ReadPolicy::default()).unwrap(), unfiltered);
        }
        // Ordinary pins need no live database to apply a new eligibility set.
        drop(db);
        assert_eq!(ids(&pin.search_filtered(
            &c.query(), queries()[2], ReadPolicy::default(), |id| id == VId(2),
        ).unwrap()), [VId(2)]);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn committed_updates_advance_only_maintained_search_while_old_pins_stay_historical() {
    let ((), report) = run_async_under_lab(0xbeac_5104, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let mut db = database(&c.commit(), false).await;
        let handle = db.register_beacon_index(&c.query(), &options(), ReadPolicy::default()).unwrap();
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
            assert!(db.search_beacon_index_filtered(
                &c.query(), &handle, query, ReadPolicy::default(), |id| id == VId(2),
            ).unwrap().is_empty());
            assert_eq!(ids(&pin.search_filtered(
                &c.query(), query, ReadPolicy::default(), |id| id == VId(2),
            ).unwrap()), [VId(2)]);
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
        let handle = db.register_beacon_index(
            &c.query(), &options(), ReadPolicy { max_work_units: 0, ..ReadPolicy::default() },
        ).unwrap();
        let old = db.beacon_index_snapshot(&c.query(), &handle).unwrap();
        assert!(matches!(other.search_beacon_index_filtered(
            &c.query(), &handle, queries()[0], ReadPolicy::default(), |_| panic!("foreign corpus"),
        ), Err(MaintainedBeaconError::ForeignHandle)));
        let mut update = WriteBatch::new(RelationId(1));
        update.set_vertex_property(VId(1), TEXT, Some(text("changed")));
        db.write(&c.commit(), update).await.unwrap();
        assert!(matches!(db.search_beacon_index_filtered(
            &c.query(), &handle, queries()[0], ReadPolicy::default(), |_| panic!("fenced corpus"),
        ), Err(MaintainedBeaconError::Unavailable { .. })));
        assert_eq!(ids(&old.search_filtered(
            &c.query(), queries()[0], ReadPolicy::default(), |id| id == VId(2),
        ).unwrap()), [VId(2)]);
        db.unregister_beacon_index(&c.query(), &handle).unwrap();
        assert!(matches!(db.search_beacon_index_filtered(
            &c.query(), &handle, queries()[0], ReadPolicy::default(), |_| panic!("removed corpus"),
        ), Err(MaintainedBeaconError::UnknownHandle)));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
