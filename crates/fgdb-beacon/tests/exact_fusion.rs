use std::collections::HashSet;
use std::num::NonZeroU32;

use fgdb_beacon::{
    BeaconError, BeaconIndex, Bm25Config, DistanceMetric, ExactHybridQuery, ExactRrfProfile,
    ExactRrfScore, HnswConfig, IndexConfig, IndexDocument, IndexMutation, TextMatch, VectorSearch,
    WorkBudget, WorkControl,
};
use fgdb_types::VId;

fn rank(value: u32) -> Option<NonZeroU32> {
    Some(NonZeroU32::new(value).unwrap())
}

fn budget() -> WorkBudget {
    WorkBudget::new(1_000_000)
}

fn config() -> IndexConfig {
    IndexConfig {
        vector: Some(HnswConfig::new(1, DistanceMetric::SquaredEuclidean)),
        text: Some(Bm25Config::default()),
        ..IndexConfig::default()
    }
}

fn index() -> BeaconIndex {
    BeaconIndex::build(
        config(),
        [
            IndexDocument {
                id: VId(1),
                vector: Some(vec![10.0]),
                text: Some("red".into()),
            },
            IndexDocument {
                id: VId(2),
                vector: Some(vec![20.0]),
                text: Some("red red".into()),
            },
            IndexDocument {
                id: VId(3),
                vector: Some(vec![0.0]),
                text: Some("blue".into()),
            },
        ],
        &mut budget(),
    )
    .unwrap()
}

fn query() -> ExactHybridQuery<'static> {
    ExactHybridQuery {
        vector: &[0.0],
        text: "red",
        k: 3,
        vector_candidates: 1,
        text_candidates: 2,
        vector_mode: VectorSearch::Exact,
        text_mode: TextMatch::Any,
        profile: ExactRrfProfile::default(),
    }
}

#[test]
fn ranks_are_one_based_missing_lanes_are_zero_and_profiles_are_validated() {
    assert!(ExactRrfProfile::new(0, 1, 1).is_err());
    assert!(ExactRrfProfile::new(60, 0, 0).is_err());
    let profile = ExactRrfProfile::default();
    let score = ExactRrfScore::from_ranks(profile, rank(1), rank(2));
    assert_eq!((score.numerator(), score.denominator()), (123, 3782));
    let missing = ExactRrfScore::from_ranks(profile, rank(1), None);
    assert_eq!((missing.numerator(), missing.denominator()), (1, 61));
    let zero = ExactRrfScore::from_ranks(profile, None, None);
    assert_eq!((zero.numerator(), zero.denominator()), (0, 1));
    assert_eq!(zero.decimal().unwrap().coefficient(), 0);
}

#[test]
fn equal_rationals_have_equal_hashes_and_order_across_profiles() {
    let a = ExactRrfScore::from_ranks(ExactRrfProfile::new(1, 1, 0).unwrap(), rank(1), None);
    let b = ExactRrfScore::from_ranks(ExactRrfProfile::new(2, 2, 0).unwrap(), rank(2), None);
    assert_eq!(a, b);
    assert_eq!(a.cmp(&b), std::cmp::Ordering::Equal);
    assert_eq!(HashSet::from([a, b]).len(), 1);
}

#[test]
fn exact_order_survives_float_and_display_score_collisions() {
    let profile = ExactRrfProfile::new(u32::MAX, 1, 1).unwrap();
    let outer = ExactRrfScore::from_ranks(profile, rank(1), rank(3));
    let middle = ExactRrfScore::from_ranks(profile, rank(2), rank(2));
    assert!(outer > middle);
    let k0 = f64::from(u32::MAX);
    assert_eq!(1.0 / (k0 + 1.0) + 1.0 / (k0 + 3.0), 2.0 / (k0 + 2.0));
    assert_eq!(outer.decimal().unwrap(), middle.decimal().unwrap());
}

#[test]
fn canonical_decimal_rounds_once_with_half_even_ties() {
    // 10^18 / 2^19 = 1907348632812.5; three times that is 5722045898437.5.
    let even =
        ExactRrfScore::from_ranks(ExactRrfProfile::new(524_287, 1, 0).unwrap(), rank(1), None);
    let odd =
        ExactRrfScore::from_ranks(ExactRrfProfile::new(524_287, 3, 0).unwrap(), rank(1), None);
    assert_eq!(even.decimal().unwrap().coefficient(), 1_907_348_632_812);
    assert_eq!(odd.decimal().unwrap().coefficient(), 5_722_045_898_438);
}

#[test]
fn full_admitted_domain_keeps_cross_products_and_decimal_scaling_in_range() {
    let ranks = [
        None,
        rank(1),
        rank(2),
        rank(31),
        rank(u32::MAX - 1),
        rank(u32::MAX),
    ];
    let mut scores = Vec::new();
    for k0 in [1, 60, u32::MAX] {
        for (vector, text) in [(1, 0), (0, 1), (1, 1), (u16::MAX, u16::MAX)] {
            let profile = ExactRrfProfile::new(k0, vector, text).unwrap();
            for vr in ranks {
                for tr in ranks {
                    let score = ExactRrfScore::from_ranks(profile, vr, tr);
                    score.decimal().unwrap();
                    assert!(score.numerator() < (1_u128 << 50));
                    assert!(score.denominator() < (1_u128 << 66));
                    scores.push(score);
                }
            }
        }
    }
    for a in &scores {
        for b in &scores {
            let left = a.numerator().checked_mul(b.denominator()).unwrap();
            let right = b.numerator().checked_mul(a.denominator()).unwrap();
            assert_eq!(a.cmp(b), left.cmp(&right));
            assert_eq!(a == b, left == right);
        }
    }
    let max = ExactRrfScore::from_ranks(
        ExactRrfProfile::new(1, u16::MAX, u16::MAX).unwrap(),
        rank(1),
        rank(1),
    );
    assert_eq!(
        max.decimal().unwrap().coefficient(),
        65_535_000_000_000_000_000_000
    );
}

#[test]
fn separate_depths_union_evidence_and_canonical_top_k_use_the_native_lanes() {
    let snapshot = index().snapshot();
    let q = query();
    let hits = snapshot
        .hybrid_search_exact_fusion(q, |_| true, &mut budget())
        .unwrap();
    assert_eq!(hits.len(), 3);
    let vector = snapshot
        .knn(q.vector, 1, VectorSearch::Exact, |_| true, &mut budget())
        .unwrap();
    let text = snapshot
        .text_search(q.text, 2, TextMatch::Any, |_| true, &mut budget())
        .unwrap();
    for hit in &hits {
        let vr = vector.iter().position(|entry| entry.id == hit.id);
        let tr = text.iter().position(|entry| entry.id == hit.id);
        let vr = vr.and_then(|i| NonZeroU32::new(u32::try_from(i).unwrap() + 1));
        let tr = tr.and_then(|i| NonZeroU32::new(u32::try_from(i).unwrap() + 1));
        assert_eq!(hit.vector_rank, vr);
        assert_eq!(hit.text_rank, tr);
        assert_eq!(hit.score, ExactRrfScore::from_ranks(q.profile, vr, tr));
        assert_eq!(hit.decimal_score, hit.score.decimal().unwrap());
    }
    assert!(hits.windows(2).all(|pair| pair[0].score > pair[1].score
        || (pair[0].score == pair[1].score && pair[0].id < pair[1].id)));
    for k in 0..=3 {
        let smaller = ExactHybridQuery { k, ..q };
        assert_eq!(
            snapshot
                .hybrid_search_exact_fusion(smaller, |_| true, &mut budget())
                .unwrap(),
            hits[..k],
        );
    }
    let filtered = snapshot
        .hybrid_search_exact_fusion(q, |id| id != VId(3), &mut budget())
        .unwrap();
    assert!(filtered.iter().all(|hit| hit.id != VId(3)));
    assert_eq!(
        filtered
            .iter()
            .find(|hit| hit.id == VId(1))
            .unwrap()
            .vector_rank,
        rank(1)
    );
}

#[test]
fn duplicate_modalities_fuse_once_and_zero_weight_does_not_open_a_disabled_lane() {
    let snapshot = index().snapshot();
    let q = ExactHybridQuery {
        vector_candidates: 3,
        ..query()
    };
    let hits = snapshot
        .hybrid_search_exact_fusion(q, |_| true, &mut budget())
        .unwrap();
    assert_eq!(hits.len(), 3);
    for id in [VId(1), VId(2)] {
        let hit = hits.iter().find(|hit| hit.id == id).unwrap();
        assert!(hit.vector_rank.is_some() && hit.text_rank.is_some());
    }
    let text_only = BeaconIndex::build(
        IndexConfig::default(),
        [IndexDocument {
            id: VId(9),
            vector: None,
            text: Some("red".into()),
        }],
        &mut budget(),
    )
    .unwrap();
    let q = ExactHybridQuery {
        vector: &[f32::NAN],
        vector_mode: VectorSearch::Approximate { ef_search: 0 },
        profile: ExactRrfProfile::new(60, 0, 2).unwrap(),
        k: 1,
        ..query()
    };
    let hits = text_only
        .snapshot()
        .hybrid_search_exact_fusion(q, |_| true, &mut budget())
        .unwrap();
    assert_eq!(hits[0].id, VId(9));
    assert_eq!(hits[0].vector_rank, None);
    assert_eq!(
        (hits[0].score.numerator(), hits[0].score.denominator()),
        (2, 61)
    );
}

#[test]
fn invalid_shape_and_native_source_failures_are_not_hidden() {
    let snapshot = index().snapshot();
    let invalid = ExactHybridQuery { k: 4, ..query() };
    assert!(matches!(
        snapshot.hybrid_search_exact_fusion(invalid, |_| true, &mut budget()),
        Err(BeaconError::InvalidQuery(_))
    ));
    let invalid = ExactHybridQuery {
        vector: &[],
        ..query()
    };
    assert!(matches!(
        snapshot.hybrid_search_exact_fusion(invalid, |_| true, &mut budget()),
        Err(BeaconError::Dimension {
            expected: 1,
            actual: 0
        })
    ));
    let invalid = ExactHybridQuery {
        vector_mode: VectorSearch::Approximate { ef_search: 0 },
        ..query()
    };
    assert!(matches!(
        snapshot.hybrid_search_exact_fusion(invalid, |_| true, &mut budget()),
        Err(BeaconError::InvalidQuery(_))
    ));
}

#[test]
fn pinned_fusion_survives_writer_updates_and_compaction() {
    let mut index = index();
    let old = index.snapshot();
    let expected = old
        .hybrid_search_exact_fusion(query(), |_| true, &mut budget())
        .unwrap();
    index
        .apply_batch(
            [
                IndexMutation::Delete(VId(3)),
                IndexMutation::Upsert(IndexDocument {
                    id: VId(2),
                    vector: Some(vec![0.0]),
                    text: Some("blue".into()),
                }),
            ],
            &mut budget(),
        )
        .unwrap();
    index.compact(&mut budget()).unwrap();
    assert_eq!(
        old.hybrid_search_exact_fusion(query(), |_| true, &mut budget())
            .unwrap(),
        expected
    );
    assert_ne!(
        index
            .snapshot()
            .hybrid_search_exact_fusion(query(), |_| true, &mut budget())
            .unwrap(),
        expected
    );
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
    call: usize,
    at: usize,
}
impl WorkControl for RefuseAt {
    fn charge(&mut self, _: usize) -> Result<(), BeaconError> {
        let current = self.call;
        self.call += 1;
        if current == self.at {
            Err(BeaconError::Cancelled)
        } else {
            Ok(())
        }
    }
}

#[test]
fn every_checkpoint_including_final_publication_can_refuse_without_partial_success() {
    let snapshot = index().snapshot();
    let mut count = Count::default();
    let expected = snapshot
        .hybrid_search_exact_fusion(query(), |_| true, &mut count)
        .unwrap();
    assert!(count.calls > 10);
    for at in 0..count.calls {
        let mut work = RefuseAt { call: 0, at };
        assert_eq!(
            snapshot.hybrid_search_exact_fusion(query(), |_| true, &mut work),
            Err(BeaconError::Cancelled)
        );
    }
    assert_eq!(
        snapshot
            .hybrid_search_exact_fusion(query(), |_| true, &mut WorkBudget::new(count.units))
            .unwrap(),
        expected
    );
    assert_eq!(
        snapshot.hybrid_search_exact_fusion(
            query(),
            |_| true,
            &mut WorkBudget::new(count.units - 1)
        ),
        Err(BeaconError::WorkBudgetExceeded)
    );
}
