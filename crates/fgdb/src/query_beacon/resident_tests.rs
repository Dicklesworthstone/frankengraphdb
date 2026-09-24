//! Real embedded source -> retained Beacon generation. These are lab tests,
//! not assertions against a hand-built document cache or an alternate graph.

use asupersync::lab::run_async_under_lab;
use super::{Error, Options};
use crate::{Database, DatabaseKeys, WriteBatch};
use fgdb_beacon::read::{ReadPolicy, Search};
use fgdb_beacon::{
    BeaconError, DistanceMetric, ExactHybridQuery, ExactRrfProfile, HnswConfig, TextMatch,
    VectorSearch,
};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, PurposeContexts, VId};

fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0x31; 32], DatabaseSecurityNamespaceId([0x32; 32]), [0x33; 32])
}

fn text(value: &str) -> CanonicalScalar {
    CanonicalScalar::ucs_basic_text(value).unwrap()
}

fn document(id: u128, words: &str, x: i64, y: i64) -> WriteBatch {
    let mut batch = WriteBatch::new(RelationId(1));
    batch.create_vertex(
        VId(id),
        vec![LabelId(1)],
        vec![
            (PropertyKeyId(1), text(words)),
            (PropertyKeyId(2), CanonicalScalar::Int(x)),
            (PropertyKeyId(3), CanonicalScalar::Int(y)),
        ],
    );
    batch
}

fn options() -> Options {
    let mut options = Options::text(PropertyKeyId(1));
    options.vertex_label = Some(LabelId(1));
    options.projection.vector = vec![PropertyKeyId(2), PropertyKeyId(3)];
    options.index.vector = Some(HnswConfig::new(2, DistanceMetric::SquaredEuclidean));
    options
}

fn text_query() -> Search<'static> {
    Search::Text { query: "graph storage", k: 10, mode: TextMatch::Any }
}

fn vector_query() -> Search<'static> {
    Search::Vector { query: &[0.0, 0.0], k: 10, mode: VectorSearch::Exact }
}

#[test]
fn reusable_generation_owns_its_definition_and_survives_writer_changes_and_drop() {
    let ((), report) = run_async_under_lab(0xbeac_1001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, document(1, "graph graph", 1, 2)).await.unwrap();
        db.write(&commit, document(2, "storage graph", 3, 4)).await.unwrap();
        let mut definition = options();
        let resident = db.prepare_beacon_index(&query, &definition).unwrap();
        let pinned = resident.snapshot();
        let basis = db.frontier().unwrap();
        assert_eq!(resident.source_sequence(), basis);
        assert_eq!(pinned.source_sequence(), basis);
        assert!(resident.belongs_to(&db));
        assert_eq!(pinned.stats().documents, 2);
        let expected_text = db.beacon_search(&query, &definition, text_query()).unwrap();
        let expected_vector = db.beacon_search(&query, &definition, vector_query()).unwrap();
        let approximate = Search::Vector {
            query: &[0.0, 0.0], k: 10, mode: VectorSearch::Approximate { ef_search: 32 },
        };
        let hybrid = Search::Hybrid(ExactHybridQuery {
            vector: &[0.0, 0.0], text: "graph storage", k: 10,
            vector_candidates: 10, text_candidates: 10,
            vector_mode: VectorSearch::Approximate { ef_search: 32 },
            text_mode: TextMatch::Any, profile: ExactRrfProfile::default(),
        });
        let expected_approximate = db.beacon_search(&query, &definition, approximate).unwrap();
        let expected_hybrid = db.beacon_search(&query, &definition, hybrid).unwrap();

        // Delivery can forbid all graph-source staging: a retained search does
        // not revisit the graph, decode rows, or rebuild an HNSW generation.
        let delivery = ReadPolicy {
            max_source_scratch: 0,
            max_staging_rows: 0,
            ..ReadPolicy::default()
        };
        definition.vertex_label = Some(LabelId(999));
        definition.projection.text = Some(PropertyKeyId(999));
        definition.projection.vector.reverse();
        definition.index.vector.as_mut().unwrap().metric = DistanceMetric::NegativeDotProduct;
        definition.policy.max_work_units = 0;
        assert_eq!(resident.search(&query, text_query(), delivery).unwrap(), expected_text);
        assert_eq!(pinned.search(&query, vector_query(), delivery).unwrap(), expected_vector);
        assert_eq!(resident.search(&query, approximate, delivery).unwrap(), expected_approximate);
        assert_eq!(pinned.search(&query, hybrid, delivery).unwrap(), expected_hybrid);

        db.write(&commit, document(3, "graph storage", 0, 0)).await.unwrap();
        assert_ne!(db.beacon_search(&query, &options(), vector_query()).unwrap(), expected_vector);
        assert_eq!(resident.source_sequence(), basis);
        assert_eq!(resident.search(&query, text_query(), delivery).unwrap(), expected_text);
        let foreign = Database::open_memory(&commit, keys()).await.unwrap();
        assert!(!resident.belongs_to(&foreign));
        let moved = db;
        assert!(resident.belongs_to(&moved));
        drop(moved);
        drop(resident);
        assert_eq!(pinned.search(&query, text_query(), delivery).unwrap(), expected_text);
        assert_eq!(pinned.search(&query, vector_query(), delivery).unwrap(), expected_vector);
        // This pins an actual approximate topology, not a claim that a future
        // refreshed/rebuilt topology must return the same approximate answer.
        assert_eq!(pinned.search(&query, approximate, delivery).unwrap(), expected_approximate);
        assert_eq!(pinned.search(&query, hybrid, delivery).unwrap(), expected_hybrid);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn initial_historical_cut_uses_old_properties_labels_and_tombstones_for_both_lanes() {
    let ((), report) = run_async_under_lab(0xbeac_1002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, document(1, "graph", 1, 0)).await.unwrap();
        let basis = db.write(&commit, document(2, "storage", 2, 0)).await.unwrap();
        let mut later = WriteBatch::new(RelationId(1));
        later.set_vertex_property(VId(1), PropertyKeyId(1), Some(text("new")));
        later.set_vertex_property(VId(1), PropertyKeyId(2), Some(CanonicalScalar::Int(8)));
        later.set_vertex_label(VId(1), LabelId(1), false);
        later.delete_vertex(VId(2));
        db.write(&commit, later).await.unwrap();
        let mut definition = options();
        definition.as_of = Some(basis);
        let resident = db.prepare_beacon_index(&query, &definition).unwrap();
        assert_eq!(resident.source_sequence(), basis);
        assert_eq!(resident.snapshot().stats().documents, 2);
        for search in [text_query(), vector_query()] {
            assert_eq!(
                resident.search(&query, search, ReadPolicy::default()).unwrap(),
                db.beacon_search(&query, &definition, search).unwrap(),
            );
        }
        assert!(db.beacon_search(&query, &options(), text_query()).unwrap().is_empty());
        definition.as_of = Some(fgdb_types::CommitSeq(db.frontier().unwrap().0 + 1));
        assert!(matches!(db.prepare_beacon_index(&query, &definition),
            Err(Error::Source(crate::ReadError::BeyondFrontier { .. }))));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn retained_construction_admits_explicit_lanes_without_changing_one_shot_lane_elision() {
    let ((), report) = run_async_under_lab(0xbeac_1003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        // This integer must not be silently rounded to a different f32 point.
        db.write(&commit, document(1, "graph", 16_777_217, 0)).await.unwrap();
        let mut definition = options();
        assert_eq!(db.beacon_search(&query, &definition, text_query()).unwrap().len(), 1);
        assert!(matches!(db.prepare_beacon_index(&query, &definition),
            Err(Error::Index(BeaconError::InvalidQuery(_)))));
        definition.index.vector = None;
        let resident = db.prepare_beacon_index(&query, &definition).unwrap();
        assert_eq!(resident.search(&query, text_query(), ReadPolicy::default()).unwrap().len(), 1);
        assert!(matches!(resident.search(&query, vector_query(), ReadPolicy::default()),
            Err(Error::Index(BeaconError::Disabled("vector")))));
        let secret = "private_projection_sentinel";
        db.write(&commit, document(2, secret, 1, 0)).await.unwrap();
        let next = db.prepare_beacon_index(&query, &definition).unwrap();
        assert!(!format!("{next:?} {:?}", next.snapshot()).contains(secret));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn query_and_build_budgets_refuse_without_consuming_the_retained_generation() {
    let ((), report) = run_async_under_lab(0xbeac_1004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, document(1, "graph", 1, 0)).await.unwrap();
        let mut definition = options();
        let resident = db.prepare_beacon_index(&query, &definition).unwrap();
        let expected = resident.search(&query, text_query(), ReadPolicy::default()).unwrap();
        let before = resident.source_sequence();
        let small = ReadPolicy { max_result_rows: 9, ..ReadPolicy::default() };
        assert!(matches!(resident.search(&query, text_query(), small),
            Err(Error::Index(BeaconError::ResourceLimit { resource: "result rows", limit: 9 }))));
        let no_work = ReadPolicy { max_work_units: 0, ..ReadPolicy::default() };
        assert!(resident.search(&query,
            Search::Text { query: "", k: 0, mode: TextMatch::Any }, no_work).is_err());
        definition.policy.max_staging_rows = 0;
        assert!(db.prepare_beacon_index(&query, &definition).is_err());
        assert_eq!(resident.source_sequence(), before);
        assert_eq!(resident.search(&query, text_query(), ReadPolicy::default()).unwrap(), expected);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn refresh_updates_both_lanes_statistics_and_membership_without_changing_old_pins() {
    let ((), report) = run_async_under_lab(0xbeac_1005, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, document(1, "graph", 1, 0)).await.unwrap();
        db.write(&commit, document(2, "storage", 2, 0)).await.unwrap();
        db.write(&commit, document(3, "unrelated words change corpus statistics", 3, 0)).await.unwrap();
        let definition = options();
        let mut resident = db.prepare_beacon_index(&query, &definition).unwrap();
        let before = resident.snapshot();
        let before_text = before.search(&query, text_query(), ReadPolicy::default()).unwrap();
        let before_vector = before.search(&query, vector_query(), ReadPolicy::default()).unwrap();
        let mut update = document(4, "storage graph graph", 0, 0);
        update.set_vertex_property(VId(1), PropertyKeyId(1), Some(text("storage storage")));
        update.set_vertex_property(VId(1), PropertyKeyId(2), Some(CanonicalScalar::Int(9)));
        update.set_vertex_label(VId(2), LabelId(1), false);
        update.delete_vertex(VId(3));
        let through = db.write(&commit, update).await.unwrap();
        let progress = resident.refresh(&query, &db, None, ReadPolicy::default()).unwrap();
        assert_eq!(progress.from, before.source_sequence());
        assert_eq!(progress.through, through);
        assert_eq!(progress.commits, 1);
        assert_eq!(progress.touched_vertices, 4);
        assert_eq!(resident.snapshot().stats().documents, 2);
        for search in [text_query(), vector_query()] {
            assert_eq!(resident.search(&query, search, ReadPolicy::default()).unwrap(),
                db.beacon_search(&query, &definition, search).unwrap());
        }
        assert_eq!(before.search(&query, text_query(), ReadPolicy::default()).unwrap(), before_text);
        assert_eq!(before.search(&query, vector_query(), ReadPolicy::default()).unwrap(), before_vector);

        // Re-entry, a missing coordinate, and null text update the live lane
        // counters and document-frequency population, not just the hit filter.
        let mut next = WriteBatch::new(RelationId(1));
        next.set_vertex_label(VId(2), LabelId(1), true);
        next.set_vertex_property(VId(1), PropertyKeyId(3), None);
        next.set_vertex_property(VId(4), PropertyKeyId(1), Some(CanonicalScalar::Null));
        db.write(&commit, next).await.unwrap();
        resident.refresh(&query, &db, None, ReadPolicy::default()).unwrap();
        assert_eq!(resident.snapshot().stats().documents, 3);
        assert_eq!(resident.snapshot().stats().vector_documents, 2);
        for search in [text_query(), vector_query()] {
            assert_eq!(resident.search(&query, search, ReadPolicy::default()).unwrap(),
                db.beacon_search(&query, &definition, search).unwrap());
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn refresh_uses_the_requested_cut_not_newer_values_and_can_retry_a_failed_projection() {
    let ((), report) = run_async_under_lab(0xbeac_1006, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, document(1, "graph", 1, 0)).await.unwrap();
        let mut resident = db.prepare_beacon_index(&query, &options()).unwrap();
        let mut change = WriteBatch::new(RelationId(1));
        change.set_vertex_property(VId(1), PropertyKeyId(2), Some(CanonicalScalar::Int(2)));
        let target = db.write(&commit, change).await.unwrap();
        let mut invalid = WriteBatch::new(RelationId(1));
        invalid.set_vertex_property(VId(1), PropertyKeyId(2), Some(text("not a coordinate")));
        db.write(&commit, invalid).await.unwrap();

        resident.refresh(&query, &db, Some(target), ReadPolicy::default()).unwrap();
        let old = resident.snapshot();
        let rows = old.search(&query, vector_query(), ReadPolicy::default()).unwrap();
        let mut at_target = options();
        at_target.as_of = Some(target);
        assert_eq!(rows, db.beacon_search(&query, &at_target, vector_query()).unwrap());
        assert!(matches!(resident.refresh(&query, &db, None, ReadPolicy::default()),
            Err(Error::Index(BeaconError::InvalidQuery(_)))));
        assert_eq!(resident.source_sequence(), target);
        assert_eq!(resident.search(&query, vector_query(), ReadPolicy::default()).unwrap(), rows);
        assert_eq!(resident.snapshot().stats(), old.stats());

        let mut fixed = WriteBatch::new(RelationId(1));
        fixed.set_vertex_property(VId(1), PropertyKeyId(2), Some(CanonicalScalar::Int(3)));
        let end = db.write(&commit, fixed).await.unwrap();
        let progress = resident.refresh(&query, &db, None, ReadPolicy::default()).unwrap();
        assert_eq!(progress.commits, end.0 - target.0);
        assert_eq!(progress.touched_vertices, 1);
        assert_eq!(resident.search(&query, vector_query(), ReadPolicy::default()).unwrap(),
            db.beacon_search(&query, &options(), vector_query()).unwrap());
        assert_eq!(old.search(&query, vector_query(), ReadPolicy::default()).unwrap(), rows);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn edge_only_and_normalized_noop_commits_advance_without_graph_source_admission() {
    let ((), report) = run_async_under_lab(0xbeac_1007, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, document(1, "graph", 1, 0)).await.unwrap();
        db.write(&commit, document(2, "storage", 2, 0)).await.unwrap();
        let mut resident = db.prepare_beacon_index(&query, &options()).unwrap();
        let before = resident.snapshot();
        let mut edge = WriteBatch::new(RelationId(2));
        edge.add_edge(fgdb_types::EId(1), VId(1), VId(2), vec![]);
        db.write(&commit, edge).await.unwrap();
        let mut noop = WriteBatch::new(RelationId(1));
        noop.ensure_vertex(VId(1), vec![], vec![]);
        let target = db.write(&commit, noop).await.unwrap();
        let no_source = ReadPolicy {
            max_source_scratch: 0, max_staging_rows: 0, ..ReadPolicy::default()
        };
        let progress = resident.refresh(&query, &db, None, no_source).unwrap();
        assert_eq!(progress.through, target);
        assert_eq!(progress.commits, 2);
        assert_eq!(progress.touched_vertices, 0);
        assert_eq!(resident.snapshot().stats(), before.stats());
        assert_eq!(resident.search(&query, text_query(), no_source).unwrap(),
            before.search(&query, text_query(), no_source).unwrap());
        let no_change = resident.refresh(&query, &db, None, no_source).unwrap();
        assert_eq!(no_change.commits, 0);
        assert_eq!(no_change.from, no_change.through);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn foreign_backwards_future_and_resource_refusals_preserve_sequence_and_contents() {
    let ((), report) = run_async_under_lab(0xbeac_1008, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, document(1, "graph", 1, 0)).await.unwrap();
        let mut resident = db.prepare_beacon_index(&query, &options()).unwrap();
        let before = resident.snapshot();
        let expected = before.search(&query, text_query(), ReadPolicy::default()).unwrap();
        let other = Database::open_memory(&commit, keys()).await.unwrap();
        assert!(matches!(resident.refresh(&query, &other, None, ReadPolicy::default()),
            Err(Error::ForeignDatabase)));
        assert!(matches!(resident.refresh(&query, &db, Some(fgdb_types::CommitSeq(0)), ReadPolicy::default()),
            Err(Error::BeforeSource { .. })));
        assert!(matches!(resident.refresh(&query, &db, Some(fgdb_types::CommitSeq(u64::MAX)), ReadPolicy::default()),
            Err(Error::Source(crate::ReadError::BeyondFrontier { .. }))));
        db.write(&commit, document(2, "storage", 2, 0)).await.unwrap();
        for policy in [
            ReadPolicy { max_staging_rows: 0, ..ReadPolicy::default() },
            ReadPolicy { max_source_scratch: 0, ..ReadPolicy::default() },
            ReadPolicy { max_work_units: 0, ..ReadPolicy::default() },
        ] {
            assert!(resident.refresh(&query, &db, None, policy).is_err());
            assert_eq!(resident.source_sequence(), before.source_sequence());
            assert_eq!(resident.snapshot().stats(), before.stats());
            assert_eq!(resident.search(&query, text_query(), ReadPolicy::default()).unwrap(), expected);
        }
        resident.refresh(&query, &db, None, ReadPolicy::default()).unwrap();
        assert_eq!(resident.snapshot().stats().documents, 2);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn transient_vertex_history_and_multi_relation_changes_are_coalesced_at_the_final_cut() {
    let ((), report) = run_async_under_lab(0xbeac_1009, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, document(1, "graph", 1, 0)).await.unwrap();
        let mut definition = options();
        definition.index.max_batch_operations = 2;
        let mut resident = db.prepare_beacon_index(&query, &definition).unwrap();
        db.write(&commit, document(2, "storage", 2, 0)).await.unwrap();
        let mut delete = WriteBatch::new(RelationId(9));
        delete.delete_vertex(VId(2));
        db.write(&commit, delete).await.unwrap();
        for (relation, value) in [
            (RelationId(2), 2),
            (RelationId(3), 3),
            (RelationId(4), 4),
            (RelationId(5), 5),
        ] {
            let mut change = WriteBatch::new(relation);
            change.set_vertex_property(VId(1), PropertyKeyId(2), Some(CanonicalScalar::Int(value)));
            db.write(&commit, change).await.unwrap();
        }
        let policy = ReadPolicy { max_staging_rows: 2, ..ReadPolicy::default() };
        let progress = resident.refresh(&query, &db, None, policy).unwrap();
        assert_eq!(progress.commits, 6);
        assert_eq!(progress.touched_vertices, 2);
        assert_eq!(resident.snapshot().stats().documents, 1);
        for search in [text_query(), vector_query()] {
            assert_eq!(resident.search(&query, search, ReadPolicy::default()).unwrap(),
                db.beacon_search(&query, &definition, search).unwrap());
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn indexed_refresh_uses_only_affected_histories_under_a_two_entry_source_budget() {
    let ((), report) = run_async_under_lab(0xbeac_1010, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = document(0, "graph", 1, 0);
        seed.create_vertex(VId(u128::MAX), vec![LabelId(1)], vec![
            (PropertyKeyId(1), text("storage")),
            (PropertyKeyId(2), CanonicalScalar::Int(2)),
            (PropertyKeyId(3), CanonicalScalar::Int(0)),
        ]);
        // Unselected histories include wrong-typed values. A point refresh
        // must neither project them nor allocate a full-table winner set.
        for id in 1..=2048 {
            seed.create_vertex(VId(id), vec![LabelId(2)], vec![
                (PropertyKeyId(1), CanonicalScalar::Int(99)),
                (PropertyKeyId(2), text("not a coordinate")),
            ]);
        }
        db.write(&commit, seed).await.unwrap();
        let definition = options();
        let mut resident = db.prepare_beacon_index(&query, &definition).unwrap();
        let old = resident.snapshot();
        let before = old.search(&query, vector_query(), ReadPolicy::default()).unwrap();
        let mut changes = WriteBatch::new(RelationId(3));
        changes.set_vertex_property(VId(0), PropertyKeyId(2), Some(CanonicalScalar::Int(3)));
        changes.set_vertex_property(VId(u128::MAX), PropertyKeyId(1), Some(text("graph storage")));
        let target = db.write(&commit, changes).await.unwrap();
        let mut future = WriteBatch::new(RelationId(4));
        future.set_vertex_property(VId(0), PropertyKeyId(2), Some(text("future invalid")));
        db.write(&commit, future).await.unwrap();
        let policy = ReadPolicy {
            max_staging_rows: 2,
            max_source_scratch: 2,
            ..ReadPolicy::default()
        };
        // These two entries admit the affected-ID set exactly. A heap/table
        // scan, or a separately staged winner per ID, exhausts this allowance.
        let progress = resident.refresh(&query, &db, Some(target), policy).unwrap();
        assert_eq!(progress.touched_vertices, 2);
        assert_eq!(resident.snapshot().stats().documents, 2);
        let mut historical = definition.clone();
        historical.as_of = Some(target);
        for search in [text_query(), vector_query()] {
            assert_eq!(resident.search(&query, search, policy).unwrap(),
                db.beacon_search(&query, &historical, search).unwrap());
        }
        assert!(matches!(resident.refresh(&query, &db, None, policy),
            Err(Error::Index(BeaconError::InvalidQuery(_)))));
        assert_eq!(resident.source_sequence(), target);
        assert_eq!(old.search(&query, vector_query(), policy).unwrap(), before);

        // Final visibility precedes projection even when earlier tail versions
        // were malformed for this index. Both absence and label exits delete.
        let mut remove = WriteBatch::new(RelationId(5));
        remove.set_vertex_label(VId(0), LabelId(1), false);
        remove.delete_vertex(VId(u128::MAX));
        let end = db.write(&commit, remove).await.unwrap();
        let progress = resident.refresh(&query, &db, None, policy).unwrap();
        assert_eq!(progress.through, end);
        assert_eq!(progress.touched_vertices, 2);
        assert_eq!(resident.snapshot().stats().documents, 0);
        for search in [text_query(), vector_query()] {
            assert!(resident.search(&query, search, policy).unwrap().is_empty());
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn every_public_refresh_checkpoint_preserves_typed_interruption_and_retry() {
    use fgdb_types::context::SimulationCheckpointProbe;
    use std::sync::Arc;

    let ((), report) = run_async_under_lab(0xbeac_1011, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, document(1, "graph", 1, 0)).await.unwrap();
        let mut definition = options();
        definition.index.max_segments = 1;
        let original = db.prepare_beacon_index(&query, &definition).unwrap();
        let old = original.snapshot();
        let before = [text_query(), vector_query()].map(|search| {
            old.search(&query, search, ReadPolicy::default()).unwrap()
        });
        let mut changes = document(2, "storage", 2, 0);
        changes.set_vertex_property(VId(1), PropertyKeyId(2), Some(CanonicalScalar::Int(3)));
        let target = db.write(&commit, changes).await.unwrap();
        let trace = Arc::new(SimulationCheckpointProbe::new(None));
        let observed = query.with_checkpoint_probe(Arc::clone(&trace));
        let mut successful = original.clone();
        successful.refresh(&observed, &db, None, ReadPolicy::default()).unwrap();
        let calls = trace.calls();
        assert!(calls > 2);
        let after = [text_query(), vector_query()].map(|search| {
            successful.search(&query, search, ReadPolicy::default()).unwrap()
        });
        for stop in 1..=calls {
            let probe = Arc::new(SimulationCheckpointProbe::new(Some(stop)));
            let interrupted = query.with_checkpoint_probe(Arc::clone(&probe));
            let mut candidate = original.clone();
            assert!(matches!(candidate.refresh(&interrupted, &db, None, ReadPolicy::default()),
                Err(Error::Interrupted(_))), "checkpoint {stop}");
            assert_eq!(probe.calls(), stop, "do not resample after interruption");
            assert_eq!(candidate.source_sequence(), old.source_sequence());
            assert_eq!(candidate.snapshot().stats(), old.stats());
            for (search, expected) in [text_query(), vector_query()].into_iter().zip(&before) {
                assert_eq!(candidate.search(&query, search, ReadPolicy::default()).unwrap(), *expected);
            }
            // The simulation probe interrupts exactly once. The SAME context
            // and index must be reusable; no poisoned generation/cursor remains.
            candidate.refresh(&interrupted, &db, None, ReadPolicy::default()).unwrap();
            assert_eq!(candidate.source_sequence(), target);
            for (search, expected) in [text_query(), vector_query()].into_iter().zip(&after) {
                assert_eq!(candidate.search(&query, search, ReadPolicy::default()).unwrap(), *expected);
            }
        }
        assert_eq!(contexts.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
