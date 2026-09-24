use std::cell::Cell;

use fgdb_beacon::{
    BeaconError, BeaconIndex, Bm25Config, DistanceMetric, HnswConfig, IndexConfig,
    IndexDocument, IndexMutation, IndexSnapshot, TextMatch, VectorSearch, WorkBudget,
    WorkControl,
};
use fgdb_types::VId;

#[derive(Debug, PartialEq, Eq)]
enum SourceError {
    Read { offset: usize, detail: String },
    Index(BeaconError),
}

impl From<BeaconError> for SourceError {
    fn from(error: BeaconError) -> Self {
        Self::Index(error)
    }
}

fn budget() -> WorkBudget {
    WorkBudget::new(10_000_000)
}

fn config() -> IndexConfig {
    IndexConfig {
        vector: Some(HnswConfig::new(1, DistanceMetric::SquaredEuclidean)),
        text: Some(Bm25Config::default()),
        ..IndexConfig::default()
    }
}

fn document(id: u128, value: f32, text: &str) -> IndexDocument {
    IndexDocument {
        id: VId(id),
        vector: Some(vec![value]),
        text: Some(text.to_owned()),
    }
}

fn initial(config: IndexConfig) -> BeaconIndex {
    BeaconIndex::build(
        config,
        [document(1, 0.0, "red"), document(2, 10.0, "blue")],
        &mut budget(),
    )
    .unwrap()
}

fn mutations() -> Vec<IndexMutation> {
    vec![
        IndexMutation::Upsert(document(1, 3.0, "new")),
        IndexMutation::Delete(VId(2)),
        IndexMutation::Upsert(document(3, 4.0, "changed")),
        IndexMutation::Upsert(document(1, 5.0, "last")),
        IndexMutation::Delete(VId(3)),
        IndexMutation::Upsert(document(4, 6.0, "final")),
    ]
}

fn assert_same(left: &IndexSnapshot, right: &IndexSnapshot) {
    assert_eq!(left.stats(), right.stats());
    assert_eq!(
        left.knn(&[0.0], 20, VectorSearch::Exact, |_| true, &mut budget())
            .unwrap(),
        right
            .knn(&[0.0], 20, VectorSearch::Exact, |_| true, &mut budget())
            .unwrap(),
    );
    for query in ["red blue", "new changed", "last final"] {
        assert_eq!(
            left.text_search(query, 20, TextMatch::Any, |_| true, &mut budget())
                .unwrap(),
            right
                .text_search(query, 20, TextMatch::Any, |_| true, &mut budget())
                .unwrap(),
        );
    }
}

#[test]
fn both_entrypoints_check_work_before_every_source_poll() {
    for units in 0..=2 {
        let mut index = initial(config());
        let old = index.snapshot();
        let polls = Cell::new(0);
        let source = std::iter::from_fn(|| {
            polls.set(polls.get() + 1);
            Some(Ok::<_, SourceError>(IndexMutation::Delete(VId(99))))
        });
        assert_eq!(
            index.try_apply_batch(source, &mut WorkBudget::new(units)),
            Err(SourceError::Index(BeaconError::WorkBudgetExceeded)),
        );
        assert_eq!(polls.get(), usize::from(units == 2));
        assert_same(&index.snapshot(), &old);

        polls.set(0);
        let source = std::iter::from_fn(|| {
            polls.set(polls.get() + 1);
            Some(IndexMutation::Delete(VId(99)))
        });
        assert_eq!(
            index.apply_batch(source, &mut WorkBudget::new(units)),
            Err(BeaconError::WorkBudgetExceeded),
        );
        assert_eq!(polls.get(), usize::from(units == 2));
        assert_same(&index.snapshot(), &old);
    }
}

#[test]
fn source_error_at_every_prefix_preserves_its_payload_and_the_whole_generation() {
    let operations = mutations();
    for cut in 0..=operations.len() {
        let mut index = initial(config());
        let old = index.snapshot();
        let polls = Cell::new(0);
        let source = std::iter::from_fn(|| {
            let offset = polls.get();
            polls.set(offset + 1);
            assert!(offset <= cut, "source was polled after its error");
            Some(if offset == cut {
                Err(SourceError::Read {
                    offset,
                    detail: format!("source failure at {offset}"),
                })
            } else {
                Ok(operations[offset].clone())
            })
        });
        assert_eq!(
            index.try_apply_batch(source, &mut budget()),
            Err(SourceError::Read {
                offset: cut,
                detail: format!("source failure at {cut}"),
            }),
        );
        assert_eq!(polls.get(), cut + 1);
        assert_same(&index.snapshot(), &old);
    }
}

#[test]
fn fallible_and_infallible_batches_preserve_last_write_wins_and_snapshot_parity() {
    for max_segments in [1, 8] {
        let original = initial(IndexConfig { max_segments, ..config() });
        let old = original.snapshot();
        let mut direct = original.clone();
        let mut fallible = original.clone();
        let direct_report = direct.apply_batch(mutations(), &mut budget()).unwrap();
        let report = fallible
            .try_apply_batch(mutations().into_iter().map(Ok::<_, SourceError>), &mut budget())
            .unwrap();
        assert_eq!(report, direct_report);
        assert_eq!(report.operations, 6);
        assert_eq!(report.distinct_vertices, 4);
        assert_eq!(report.compacted, max_segments == 1);
        assert_same(&fallible.snapshot(), &direct.snapshot());
        assert_same(&old, &original.snapshot());
        let rows = fallible.snapshot()
            .knn(&[0.0], 20, VectorSearch::Exact, |_| true, &mut budget()).unwrap();
        assert_eq!(rows.iter().map(|row| row.id).collect::<Vec<_>>(), [VId(1), VId(4)]);
        assert_eq!(rows[0].distance, 25.0);
        assert_eq!(rows[1].distance, 36.0);
        assert_eq!(old.stats().documents, 2);
        assert_eq!(old.knn(&[0.0], 1, VectorSearch::Exact, |_| true, &mut budget())
            .unwrap()[0].distance, 0.0);
    }
}

#[test]
fn invalid_superseded_payload_is_not_hidden_by_a_later_delete() {
    let mut index = initial(config());
    let old = index.snapshot();
    let source = [
        Ok::<_, SourceError>(IndexMutation::Upsert(document(1, 99.0, "changed"))),
        Ok(IndexMutation::Upsert(document(3, f32::NAN, "new"))),
        Ok(IndexMutation::Delete(VId(3))),
    ];
    assert_eq!(
        index.try_apply_batch(source, &mut budget()),
        Err(SourceError::Index(BeaconError::NonFinite { coordinate: 0 })),
    );
    assert_same(&index.snapshot(), &old);
}

#[test]
fn exact_batch_limit_accepts_eof_but_refuses_an_extra_item_without_publishing() {
    let original = initial(IndexConfig { max_batch_operations: 2, ..config() });
    let mut exact = original.clone();
    let report = exact.try_apply_batch(
        [Ok::<_, SourceError>(IndexMutation::Delete(VId(1))),
         Ok(IndexMutation::Delete(VId(2)))],
        &mut budget(),
    ).unwrap();
    assert_eq!(report.operations, 2);
    assert_eq!(exact.snapshot().stats().documents, 0);

    let mut index = original.clone();
    let polls = Cell::new(0);
    let source = std::iter::from_fn(|| {
        let offset = polls.get();
        polls.set(offset + 1);
        assert!(offset < 3, "over-limit source was polled again");
        Some(Ok::<_, SourceError>(IndexMutation::Delete(VId(1))))
    });
    assert_eq!(
        index.try_apply_batch(source, &mut budget()),
        Err(SourceError::Index(BeaconError::ResourceLimit {
            resource: "batch operations", limit: 2,
        })),
    );
    // One lookahead distinguishes exact-bound EOF from an oversized stream.
    assert_eq!(polls.get(), 3);
    assert_same(&index.snapshot(), &original.snapshot());
}

#[test]
fn staging_limits_stop_the_source_before_another_poll() {
    let mut index = initial(IndexConfig { max_text_bytes: 32, ..config() });
    let old = index.snapshot();
    let polls = Cell::new(0);
    let source = std::iter::from_fn(|| {
        assert_eq!(polls.get(), 0, "oversized source was polled again");
        polls.set(1);
        Some(Ok::<_, SourceError>(IndexMutation::Upsert(document(1, 0.0, &"x".repeat(33)))))
    });
    assert_eq!(
        index.try_apply_batch(source, &mut budget()),
        Err(SourceError::Index(BeaconError::ResourceLimit {
            resource: "staged text bytes", limit: 32,
        })),
    );
    assert_eq!(polls.get(), 1);
    assert_same(&index.snapshot(), &old);
}

#[derive(Default)]
struct Count {
    calls: usize,
    units: usize,
}

impl WorkControl for Count {
    fn charge(&mut self, units: usize) -> Result<(), BeaconError> {
        self.calls += 1;
        self.units += units;
        Ok(())
    }
}

struct RefuseAt {
    calls: usize,
    at: usize,
}

impl WorkControl for RefuseAt {
    fn charge(&mut self, _: usize) -> Result<(), BeaconError> {
        let current = self.calls;
        self.calls += 1;
        if current == self.at { Err(BeaconError::Cancelled) } else { Ok(()) }
    }
}

#[test]
fn every_batch_checkpoint_can_refuse_with_both_lanes_and_statistics_unchanged() {
    for max_segments in [1, 8] {
        let original = initial(IndexConfig { max_segments, ..config() });
        let old = original.snapshot();
        let mut expected = original.clone();
        let mut count = Count::default();
        expected.try_apply_batch(
            mutations().into_iter().map(Ok::<_, SourceError>), &mut count,
        ).unwrap();
        assert!(count.calls > 10);
        for at in 0..count.calls {
            let mut index = original.clone();
            assert_eq!(
                index.try_apply_batch(
                    mutations().into_iter().map(Ok::<_, SourceError>),
                    &mut RefuseAt { calls: 0, at },
                ),
                Err(SourceError::Index(BeaconError::Cancelled)),
            );
            assert_same(&index.snapshot(), &old);
        }
        let mut exact = original.clone();
        exact.try_apply_batch(
            mutations().into_iter().map(Ok::<_, SourceError>),
            &mut WorkBudget::new(count.units),
        ).unwrap();
        assert_same(&exact.snapshot(), &expected.snapshot());
        let mut short = original.clone();
        assert_eq!(short.try_apply_batch(
            mutations().into_iter().map(Ok::<_, SourceError>),
            &mut WorkBudget::new(count.units - 1),
        ), Err(SourceError::Index(BeaconError::WorkBudgetExceeded)));
        assert_same(&short.snapshot(), &old);
    }
}

#[test]
fn compaction_can_refuse_at_final_publication_without_replacing_the_generation() {
    let mut index = initial(config());
    index.apply_batch([IndexMutation::Upsert(document(3, 2.0, "new"))], &mut budget()).unwrap();
    let old = index.snapshot();
    assert_eq!(old.stats().segments, 2);
    let mut expected = index.clone();
    let mut count = Count::default();
    expected.compact(&mut count).unwrap();
    assert_eq!(expected.snapshot().stats().segments, 1);
    assert_eq!(index.compact(&mut RefuseAt { calls: 0, at: count.calls - 1 }),
        Err(BeaconError::Cancelled));
    assert_same(&index.snapshot(), &old);
    index.compact(&mut budget()).unwrap();
    assert_same(&index.snapshot(), &expected.snapshot());
}

struct CancelAfterEof<'a>(&'a Cell<bool>);

impl WorkControl for CancelAfterEof<'_> {
    fn charge(&mut self, _: usize) -> Result<(), BeaconError> {
        if self.0.get() { Err(BeaconError::Cancelled) } else { Ok(()) }
    }
}

#[test]
fn cancellation_observed_at_empty_source_eof_is_not_reported_as_success() {
    let mut index = initial(config());
    let old = index.snapshot();
    let cancelled = Cell::new(false);
    let source = std::iter::from_fn(|| {
        cancelled.set(true);
        None::<Result<IndexMutation, SourceError>>
    });
    assert_eq!(index.try_apply_batch(source, &mut CancelAfterEof(&cancelled)),
        Err(SourceError::Index(BeaconError::Cancelled)));
    assert_same(&index.snapshot(), &old);
}
