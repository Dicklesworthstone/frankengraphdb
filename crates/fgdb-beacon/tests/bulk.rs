use std::cell::Cell;

use fgdb_beacon::{
    BeaconError, BeaconIndex, DistanceMetric, HnswConfig, IndexConfig, IndexDocument,
    IndexMutation, TextMatch, VectorSearch, WorkBudget, WorkControl,
};
use fgdb_types::VId;

fn config() -> IndexConfig {
    IndexConfig {
        vector: Some(HnswConfig::new(1, DistanceMetric::SquaredEuclidean)),
        max_batch_operations: 1,
        ..IndexConfig::default()
    }
}

fn document(id: u128) -> IndexDocument {
    IndexDocument {
        id: VId(id),
        vector: Some(vec![id as f32]),
        text: Some(format!("common term{}", id % 3)),
    }
}

fn work() -> WorkBudget {
    WorkBudget::new(100_000_000)
}

#[test]
fn bulk_build_has_one_segment_and_preserves_incremental_batch_policy() {
    let mut index = BeaconIndex::build(config(), (0..64).rev().map(document), &mut work()).unwrap();
    assert_eq!(index.snapshot().stats().documents, 64);
    assert_eq!(index.snapshot().stats().segments, 1);
    assert_eq!(index.config().max_batch_operations, 1);
    assert!(matches!(
        index.apply_batch(
            [IndexMutation::Delete(VId(0)), IndexMutation::Delete(VId(1)),],
            &mut work()
        ),
        Err(BeaconError::ResourceLimit {
            resource: "batch operations",
            ..
        })
    ));
    assert_eq!(index.snapshot().stats().documents, 64);
}

#[test]
fn bulk_build_matches_incremental_history_in_both_lanes() {
    let bulk = BeaconIndex::build(config(), (0..24).rev().map(document), &mut work()).unwrap();
    let mut incremental = BeaconIndex::new(config()).unwrap();
    for id in 0..24 {
        incremental
            .apply_batch([IndexMutation::Upsert(document(id))], &mut work())
            .unwrap();
    }
    let a = bulk.snapshot();
    let b = incremental.snapshot();
    assert_eq!(a.stats().text, b.stats().text);
    assert_eq!(
        a.knn(&[12.5], 7, VectorSearch::Exact, |_| true, &mut work())
            .unwrap(),
        b.knn(&[12.5], 7, VectorSearch::Exact, |_| true, &mut work())
            .unwrap()
    );
    for mode in [TextMatch::Any, TextMatch::All] {
        assert_eq!(
            a.text_search("common term1", 12, mode, |_| true, &mut work())
                .unwrap(),
            b.text_search("common term1", 12, mode, |_| true, &mut work())
                .unwrap()
        );
    }
}

#[test]
fn replacement_preserves_retained_snapshots_and_can_empty_the_corpus() {
    let mut index = BeaconIndex::build(config(), [document(1), document(2)], &mut work()).unwrap();
    let old = index.snapshot();
    index.replace_all([document(9)], &mut work()).unwrap();
    assert_eq!(index.snapshot().stats().documents, 1);
    assert_eq!(old.stats().documents, 2);
    assert_eq!(
        old.knn(&[0.0], 1, VectorSearch::Exact, |_| true, &mut work())
            .unwrap()[0]
            .id,
        VId(1)
    );
    index.replace_all([], &mut work()).unwrap();
    assert_eq!(index.snapshot().stats().segments, 0);
    assert_eq!(index.snapshot().stats().text.total_tokens, 0);
    assert_eq!(
        old.text_search("common", 10, TextMatch::Any, |_| true, &mut work())
            .unwrap()
            .len(),
        2
    );
}

#[derive(Debug, PartialEq, Eq)]
enum SourceError {
    BrokenRead,
    Index(BeaconError),
}
impl From<BeaconError> for SourceError {
    fn from(error: BeaconError) -> Self {
        Self::Index(error)
    }
}

#[test]
fn fallible_source_is_not_mistaken_for_successful_end_of_stream() {
    let mut index = BeaconIndex::build(config(), [document(1)], &mut work()).unwrap();
    let old = index.snapshot();
    let error = index
        .try_replace_all(
            [
                Ok(document(4)),
                Err(SourceError::BrokenRead),
                Ok(document(5)),
            ],
            &mut work(),
        )
        .unwrap_err();
    assert_eq!(error, SourceError::BrokenRead);
    assert_eq!(index.snapshot().stats(), old.stats());
    assert_eq!(
        index
            .snapshot()
            .knn(&[0.0], 1, VectorSearch::Exact, |_| true, &mut work())
            .unwrap()[0]
            .id,
        VId(1)
    );
    let result = BeaconIndex::try_build(
        config(),
        [Err::<IndexDocument, _>(SourceError::BrokenRead)],
        &mut work(),
    );
    assert_eq!(result.unwrap_err(), SourceError::BrokenRead);
}

#[test]
fn duplicate_and_invalid_bulk_rows_leave_both_old_lanes_intact() {
    let mut index = BeaconIndex::build(config(), [document(1)], &mut work()).unwrap();
    assert_eq!(
        index
            .replace_all([document(3), document(3)], &mut work())
            .unwrap_err(),
        BeaconError::DuplicateVertex(VId(3))
    );
    let invalid = IndexDocument {
        vector: Some(vec![f32::NAN]),
        ..document(5)
    };
    assert!(matches!(
        index.replace_all([document(4), invalid], &mut work()),
        Err(BeaconError::NonFinite { .. })
    ));
    assert_eq!(index.snapshot().stats().documents, 1);
    assert_eq!(
        index
            .snapshot()
            .text_search("term1", 10, TextMatch::Any, |_| true, &mut work())
            .unwrap()[0]
            .id,
        VId(1)
    );
}

#[test]
fn live_limits_stop_polling_an_oversized_source() {
    let mut policy = config();
    policy.max_documents = 2;
    let polls = Cell::new(0);
    let source = (0..100).map(|id| {
        polls.set(polls.get() + 1);
        document(id)
    });
    let result = BeaconIndex::build(policy, source, &mut work());
    assert!(matches!(
        result,
        Err(BeaconError::ResourceLimit {
            resource: "live documents",
            limit: 2
        })
    ));
    assert_eq!(polls.get(), 3);
}

#[test]
fn cancellation_precedes_source_polling() {
    struct Cancel;
    impl WorkControl for Cancel {
        fn charge(&mut self, _: usize) -> Result<(), BeaconError> {
            Err(BeaconError::Cancelled)
        }
    }
    let polls = Cell::new(0);
    let source = (0..100).map(|id| {
        polls.set(polls.get() + 1);
        document(id)
    });
    assert!(matches!(
        BeaconIndex::build(config(), source, &mut Cancel),
        Err(BeaconError::Cancelled)
    ));
    assert_eq!(polls.get(), 0);
}

#[test]
fn budget_failure_sweep_never_publishes_a_partial_replacement() {
    let original = BeaconIndex::build(config(), [document(1)], &mut work()).unwrap();
    let mut failed = 0;
    let mut succeeded = 0;
    for units in 0..1000 {
        let mut index = original.clone();
        let result = index.replace_all([document(4), document(5)], &mut WorkBudget::new(units));
        match result {
            Err(BeaconError::WorkBudgetExceeded) => {
                failed += 1;
                assert_eq!(index.snapshot().stats(), original.snapshot().stats());
                assert_eq!(
                    index
                        .snapshot()
                        .knn(&[0.0], 1, VectorSearch::Exact, |_| true, &mut work())
                        .unwrap()[0]
                        .id,
                    VId(1)
                );
            }
            Ok(stats) => {
                succeeded += 1;
                assert_eq!(stats.documents, 2);
                assert_eq!(stats.segments, 1);
                assert_eq!(
                    index
                        .snapshot()
                        .text_search("common", 10, TextMatch::Any, |_| true, &mut work())
                        .unwrap()
                        .len(),
                    2
                );
            }
            Err(error) => panic!("unexpected refusal: {error}"),
        }
    }
    assert!(failed > 10);
    assert!(succeeded > 0);
}
