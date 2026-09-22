use std::collections::BTreeMap;

use fgdb_beacon::{
    BeaconError, BeaconIndex, Bm25, Bm25Config, DistanceMetric, HnswConfig, HybridQuery,
    IndexConfig, IndexDocument, IndexMutation, TextMatch, VectorSearch, WorkBudget,
};
use fgdb_types::VId;

fn budget() -> WorkBudget {
    WorkBudget::new(100_000_000)
}

fn config() -> IndexConfig {
    IndexConfig {
        vector: Some(HnswConfig::new(1, DistanceMetric::SquaredEuclidean)),
        text: Some(Bm25Config::default()),
        ..IndexConfig::default()
    }
}

fn upsert(id: u128, value: f32, text: &str) -> IndexMutation {
    IndexMutation::Upsert(IndexDocument {
        id: VId(id),
        vector: Some(vec![value]),
        text: Some(text.to_owned()),
    })
}

fn initial() -> BeaconIndex {
    let mut index = BeaconIndex::new(config()).unwrap();
    index
        .apply_batch(
            [upsert(1, 0.0, "red"), upsert(2, 10.0, "blue")],
            &mut budget(),
        )
        .unwrap();
    index
}

#[test]
fn old_snapshots_survive_updates_deletes_and_compaction() {
    let mut index = initial();
    let old = index.snapshot();
    index
        .apply_batch(
            [upsert(1, 20.0, "green"), IndexMutation::Delete(VId(2))],
            &mut budget(),
        )
        .unwrap();
    index.compact(&mut budget()).unwrap();
    let now = index.snapshot();
    assert_eq!(old.stats().documents, 2);
    assert_eq!(now.stats().documents, 1);
    assert_eq!(
        old.knn(&[0.0], 1, VectorSearch::Exact, |_| true, &mut budget())
            .unwrap()[0]
            .distance,
        0.0
    );
    assert_eq!(
        now.knn(&[0.0], 1, VectorSearch::Exact, |_| true, &mut budget())
            .unwrap()[0]
            .distance,
        400.0
    );
    assert_eq!(
        old.text_search("red", 10, TextMatch::Any, |_| true, &mut budget())
            .unwrap()
            .len(),
        1
    );
    assert!(
        now.text_search("red blue", 10, TextMatch::Any, |_| true, &mut budget())
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        now.text_search("green", 10, TextMatch::Any, |_| true, &mut budget())
            .unwrap()[0]
            .id,
        VId(1)
    );
}

#[test]
fn invalid_batch_cannot_publish_one_lane_or_one_vertex() {
    let mut index = initial();
    let old = index.snapshot();
    let error = index
        .apply_batch(
            [upsert(1, 99.0, "changed"), upsert(3, f32::NAN, "invalid")],
            &mut budget(),
        )
        .unwrap_err();
    assert!(matches!(error, BeaconError::NonFinite { .. }));
    assert_eq!(index.snapshot().stats(), old.stats());
    assert_eq!(
        index
            .snapshot()
            .knn(&[0.0], 10, VectorSearch::Exact, |_| true, &mut budget())
            .unwrap(),
        old.knn(&[0.0], 10, VectorSearch::Exact, |_| true, &mut budget())
            .unwrap()
    );
    assert_eq!(
        index
            .snapshot()
            .text_search("red changed", 10, TextMatch::Any, |_| true, &mut budget())
            .unwrap(),
        old.text_search("red changed", 10, TextMatch::Any, |_| true, &mut budget())
            .unwrap()
    );
}

#[test]
fn exhaustion_at_many_construction_boundaries_leaves_the_previous_generation() {
    let mut refused = 0;
    let mut applied = 0;
    for units in 0..400 {
        let mut index = initial();
        let old = index.snapshot();
        let result = index.apply_batch(
            [upsert(1, 2.0, "new new"), upsert(3, 3.0, "new")],
            &mut WorkBudget::new(units),
        );
        if result.is_err() {
            refused += 1;
            assert_eq!(index.snapshot().stats(), old.stats());
            assert_eq!(
                index
                    .snapshot()
                    .knn(&[0.0], 1, VectorSearch::Exact, |_| true, &mut budget())
                    .unwrap()[0]
                    .distance,
                0.0
            );
            assert!(
                index
                    .snapshot()
                    .text_search("new", 10, TextMatch::Any, |_| true, &mut budget())
                    .unwrap()
                    .is_empty()
            );
        } else {
            applied += 1;
            assert_eq!(index.snapshot().stats().documents, 3);
            assert_eq!(
                index
                    .snapshot()
                    .text_search("new", 10, TextMatch::Any, |_| true, &mut budget())
                    .unwrap()
                    .len(),
                2
            );
        }
    }
    assert!(refused > 20);
    assert!(applied > 0);
}

#[test]
fn invalid_overwritten_operation_is_not_hidden() {
    let mut index = initial();
    assert!(
        index
            .apply_batch(
                [
                    upsert(3, f32::NAN, "invalid"),
                    IndexMutation::Delete(VId(3))
                ],
                &mut budget()
            )
            .is_err()
    );
    assert_eq!(index.snapshot().stats().documents, 2);
    index
        .apply_batch(
            [
                upsert(3, 3.0, "old"),
                upsert(3, 30.0, "new"),
                IndexMutation::Delete(VId(1)),
            ],
            &mut budget(),
        )
        .unwrap();
    assert_eq!(index.snapshot().stats().documents, 2);
    assert!(
        index
            .snapshot()
            .text_search("old red", 10, TextMatch::Any, |_| true, &mut budget())
            .unwrap()
            .is_empty()
    );
}

#[test]
fn old_versions_do_not_crowd_out_new_results() {
    let mut index = initial();
    for value in 1..7 {
        index
            .apply_batch([upsert(1, value as f32, "red")], &mut budget())
            .unwrap();
    }
    let results = index
        .snapshot()
        .knn(
            &[0.0],
            2,
            VectorSearch::Approximate { ef_search: 1 },
            |_| true,
            &mut budget(),
        )
        .unwrap();
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].id, VId(1));
    assert_eq!(results[0].distance, 36.0);
    assert_eq!(results[1].id, VId(2));
}

#[test]
fn every_segment_uses_the_live_corpus_statistics() {
    let mut index = initial();
    index
        .apply_batch([upsert(3, 3.0, "red red red blue")], &mut budget())
        .unwrap();
    index
        .apply_batch(
            [IndexMutation::Delete(VId(2)), upsert(1, 1.0, "red green")],
            &mut budget(),
        )
        .unwrap();
    let rebuilt = Bm25::build(
        Bm25Config::default(),
        [
            (VId(1), "red green".to_owned()),
            (VId(3), "red red red blue".to_owned()),
        ],
        &mut budget(),
    )
    .unwrap();
    assert_eq!(index.snapshot().stats().text, rebuilt.stats());
    assert_eq!(
        index
            .snapshot()
            .text_search(
                "red blue green",
                10,
                TextMatch::Any,
                |_| true,
                &mut budget()
            )
            .unwrap(),
        rebuilt
            .search(
                "red blue green",
                10,
                TextMatch::Any,
                |_| true,
                &mut budget()
            )
            .unwrap()
    );
}

#[test]
fn fixed_segment_limit_compacts_without_changing_exact_results() {
    let mut policy = config();
    policy.max_segments = 2;
    let mut index = BeaconIndex::new(policy).unwrap();
    index
        .apply_batch([upsert(1, 1.0, "one")], &mut budget())
        .unwrap();
    let old = index.snapshot();
    index
        .apply_batch([upsert(2, 2.0, "two")], &mut budget())
        .unwrap();
    let report = index
        .apply_batch([upsert(1, 3.0, "three")], &mut budget())
        .unwrap();
    assert!(report.compacted);
    assert_eq!(report.segments, 1);
    let before = index.snapshot();
    index.compact(&mut budget()).unwrap();
    assert_eq!(
        before
            .knn(&[0.0], 10, VectorSearch::Exact, |_| true, &mut budget())
            .unwrap(),
        index
            .snapshot()
            .knn(&[0.0], 10, VectorSearch::Exact, |_| true, &mut budget())
            .unwrap()
    );
    assert_eq!(
        before
            .text_search("two three", 10, TextMatch::Any, |_| true, &mut budget())
            .unwrap(),
        index
            .snapshot()
            .text_search("two three", 10, TextMatch::Any, |_| true, &mut budget())
            .unwrap()
    );
    assert_eq!(
        old.text_search("one", 10, TextMatch::Any, |_| true, &mut budget())
            .unwrap()[0]
            .id,
        VId(1)
    );
}

#[test]
fn removing_all_documents_clears_segments_and_statistics() {
    let mut index = initial();
    index
        .apply_batch(
            [IndexMutation::Delete(VId(1)), IndexMutation::Delete(VId(2))],
            &mut budget(),
        )
        .unwrap();
    let stats = index.snapshot().stats();
    assert_eq!(stats.documents, 0);
    assert_eq!(stats.segments, 0);
    assert_eq!(stats.text.total_tokens, 0);
    assert_eq!(stats.text.distinct_terms, 0);
    assert!(
        index
            .snapshot()
            .knn(&[0.0], 10, VectorSearch::Exact, |_| true, &mut budget())
            .unwrap()
            .is_empty()
    );
}

#[test]
fn replacing_a_modality_removes_its_old_postings_and_statistics() {
    let mut index = initial();
    index
        .apply_batch(
            [IndexMutation::Upsert(IndexDocument {
                id: VId(1),
                vector: None,
                text: Some(String::new()),
            })],
            &mut budget(),
        )
        .unwrap();
    assert_eq!(index.snapshot().stats().vector_documents, 1);
    assert_eq!(index.snapshot().stats().text.documents, 2);
    assert!(
        index
            .snapshot()
            .text_search("red", 10, TextMatch::Any, |_| true, &mut budget())
            .unwrap()
            .is_empty()
    );
}

#[test]
fn resource_limit_refusal_is_atomic() {
    let mut policy = config();
    policy.max_documents = 1;
    let mut index = BeaconIndex::new(policy).unwrap();
    index
        .apply_batch([upsert(1, 1.0, "one")], &mut budget())
        .unwrap();
    assert!(matches!(
        index.apply_batch([upsert(2, 2.0, "two")], &mut budget()),
        Err(BeaconError::ResourceLimit { .. })
    ));
    assert_eq!(index.snapshot().stats().documents, 1);
    assert_eq!(
        index
            .snapshot()
            .text_search("one", 10, TextMatch::Any, |_| true, &mut budget())
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn hybrid_fusion_uses_one_generation_and_stable_ties() {
    let index = initial();
    let query = HybridQuery {
        vector: &[0.0],
        text: "blue",
        k: 2,
        candidates: 2,
        vector_mode: VectorSearch::Exact,
        text_mode: TextMatch::Any,
        rank_constant: 60.0,
        vector_weight: 1.0,
        text_weight: 1.0,
    };
    let hits = index
        .snapshot()
        .hybrid_search(query, |_| true, &mut budget())
        .unwrap();
    assert_eq!(hits[0].id, VId(2));
    assert!(hits[0].vector_distance.is_some());
    assert!(hits[0].text_score.is_some());
    let bad = HybridQuery {
        candidates: 1,
        ..query
    };
    assert!(
        index
            .snapshot()
            .hybrid_search(bad, |_| true, &mut budget())
            .is_err()
    );
}

#[test]
fn deterministic_mutation_history_matches_rebuild_and_distance_oracle() {
    let mut index = BeaconIndex::new(config()).unwrap();
    let mut reference = BTreeMap::<u128, (f32, String)>::new();
    let mut seed = 11u64;
    for step in 0..120 {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        let id = u128::from((seed >> 32) % 20);
        if step % 5 == 0 {
            index
                .apply_batch([IndexMutation::Delete(VId(id))], &mut budget())
                .unwrap();
            reference.remove(&id);
        } else {
            let value = ((seed >> 16) % 100) as f32;
            let text = format!("common term{} term{}", id % 4, step % 3);
            index
                .apply_batch([upsert(id, value, &text)], &mut budget())
                .unwrap();
            reference.insert(id, (value, text));
        }
        let rebuilt = Bm25::build(
            Bm25Config::default(),
            reference
                .iter()
                .map(|(&id, (_, text))| (VId(id), text.clone())),
            &mut budget(),
        )
        .unwrap();
        assert_eq!(
            index
                .snapshot()
                .text_search("common term1", 7, TextMatch::Any, |_| true, &mut budget())
                .unwrap(),
            rebuilt
                .search("common term1", 7, TextMatch::Any, |_| true, &mut budget())
                .unwrap()
        );
        let mut expected: Vec<_> = reference
            .iter()
            .map(|(&id, (value, _))| {
                let difference = f64::from(*value) - 42.0;
                (id, difference * difference)
            })
            .collect();
        expected.sort_by(|a, b| a.1.total_cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
        expected.truncate(7);
        let actual = index
            .snapshot()
            .knn(&[42.0], 7, VectorSearch::Exact, |_| true, &mut budget())
            .unwrap();
        assert_eq!(
            actual
                .iter()
                .map(|hit| (hit.id.0, hit.distance))
                .collect::<Vec<_>>(),
            expected
        );
    }
}

#[test]
fn normalized_staging_has_aggregate_limits_before_publication() {
    let mut policy = config();
    policy.max_text_bytes = 8;
    let mut index = BeaconIndex::new(policy).unwrap();
    index
        .apply_batch([upsert(1, 1.0, "old")], &mut budget())
        .unwrap();
    let error = index
        .apply_batch(
            [upsert(2, 2.0, "12345"), upsert(3, 3.0, "67890")],
            &mut budget(),
        )
        .unwrap_err();
    assert!(matches!(
        error,
        BeaconError::ResourceLimit {
            resource: "staged text bytes",
            ..
        }
    ));
    assert_eq!(index.snapshot().stats().documents, 1);
    assert_eq!(
        index
            .snapshot()
            .text_search("old", 1, TextMatch::Any, |_| true, &mut budget())
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn overwritten_and_deleted_staged_values_release_their_staging_budget() {
    let mut policy = config();
    policy.max_text_bytes = 8;
    let mut index = BeaconIndex::new(policy).unwrap();
    index
        .apply_batch(
            [
                upsert(1, 1.0, "12345678"),
                upsert(1, 2.0, "short"),
                IndexMutation::Delete(VId(1)),
                upsert(2, 3.0, "abcdefgh"),
            ],
            &mut budget(),
        )
        .unwrap();
    assert_eq!(index.snapshot().stats().documents, 1);
    assert_eq!(index.snapshot().stats().text_bytes, 8);
}

#[test]
fn derived_writer_fork_does_not_publish_into_the_original() {
    let index = initial();
    let mut fork = index.clone();
    fork.apply_batch([upsert(1, 2.0, "replacement")], &mut budget())
        .unwrap();
    assert!(
        index
            .snapshot()
            .text_search("replacement", 1, TextMatch::Any, |_| true, &mut budget())
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        fork.snapshot()
            .text_search("replacement", 1, TextMatch::Any, |_| true, &mut budget())
            .unwrap()
            .len(),
        1
    );
}
