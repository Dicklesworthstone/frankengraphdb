//! Public API regressions: Boolean predicates belong to one document version.

use fgdb_beacon::{
    BeaconError, BeaconIndex, Bm25, Bm25Config, BooleanTextQuery, IndexConfig, IndexDocument,
    IndexMutation, IndexSnapshot, TextHit, WorkBudget,
};
use fgdb_types::VId;

fn unlimited() -> WorkBudget {
    WorkBudget::new(usize::MAX)
}

fn document(id: u128, text: Option<&str>) -> IndexDocument {
    IndexDocument {
        id: VId(id),
        vector: None,
        text: text.map(str::to_owned),
    }
}

fn bits(hits: &[TextHit]) -> Vec<(VId, u64)> {
    hits.iter()
        .map(|hit| (hit.id, hit.score.to_bits()))
        .collect()
}

fn query() -> BooleanTextQuery<'static> {
    BooleanTextQuery {
        required: "red",
        optional: "blue green",
        excluded: "banned",
        minimum_optional: 1,
    }
}

fn assert_matches_fresh_corpus(snapshot: &IndexSnapshot, documents: &[(u128, &str)]) {
    let flat = Bm25::build(
        Bm25Config::default(),
        documents
            .iter()
            .map(|(id, text)| (VId(*id), (*text).to_owned())),
        &mut unlimited(),
    )
    .unwrap();
    for required in ["", "red", "red blue", "missing"] {
        for minimum_optional in 0..=3 {
            let query = BooleanTextQuery {
                required,
                minimum_optional,
                ..query()
            };
            for k in [0, 1, 3, usize::MAX] {
                for filtered in [false, true] {
                    let eligible = |id: VId| !filtered || id.0.is_multiple_of(2);
                    let actual = snapshot
                        .text_search_boolean(query, k, eligible, &mut unlimited())
                        .unwrap();
                    let expected = flat
                        .search_boolean(query, k, eligible, &mut unlimited())
                        .unwrap();
                    assert_eq!(bits(&actual), bits(&expected), "{query:?}, k={k}");
                }
            }
        }
    }
}

#[test]
fn updates_deletes_compaction_and_rebuild_preserve_pinned_boolean_results() {
    let original = [
        (1, "red banned"),
        (2, "red blue"),
        (3, "green"),
        (u128::MAX, "red green blue"),
        (1_u128 << 100, ""),
    ];
    let mut index = BeaconIndex::build(
        IndexConfig::default(),
        original
            .iter()
            .map(|(id, text)| document(*id, Some(text)))
            .chain(std::iter::once(document(10, None))),
        &mut unlimited(),
    )
    .unwrap();
    let pinned = index.snapshot();
    assert_matches_fresh_corpus(&pinned, &original);
    index
        .apply_batch(
            [
                IndexMutation::Upsert(document(1, Some("red blue"))),
                IndexMutation::Upsert(document(2, Some("red banned blue"))),
                IndexMutation::Upsert(document(3, Some("red green blue"))),
                IndexMutation::Delete(VId(u128::MAX)),
            ],
            &mut unlimited(),
        )
        .unwrap();
    let changed = [
        (1, "red blue"),
        (2, "red banned blue"),
        (3, "red green blue"),
        (1_u128 << 100, ""),
    ];
    let segmented = index.snapshot();
    assert_eq!(segmented.stats().segments, 2);
    assert_matches_fresh_corpus(&segmented, &changed);
    assert_matches_fresh_corpus(&pinned, &original);
    index.compact(&mut unlimited()).unwrap();
    assert_eq!(index.snapshot().stats().segments, 1);
    assert_matches_fresh_corpus(&index.snapshot(), &changed);
    assert_matches_fresh_corpus(&segmented, &changed);
    assert_matches_fresh_corpus(&pinned, &original);
    index
        .replace_all([document(42, Some("red blue green"))], &mut unlimited())
        .unwrap();
    assert_matches_fresh_corpus(&index.snapshot(), &[(42, "red blue green")]);
    assert_matches_fresh_corpus(&pinned, &original);
    assert_matches_fresh_corpus(&segmented, &changed);
}

#[test]
fn clauses_never_mix_document_generations() {
    let mut index = BeaconIndex::build(
        IndexConfig::default(),
        [document(1, Some("red")), document(2, Some("red banned"))],
        &mut unlimited(),
    )
    .unwrap();
    index
        .apply_batch(
            [
                IndexMutation::Upsert(document(1, Some("blue"))),
                IndexMutation::Upsert(document(2, Some("red blue"))),
            ],
            &mut unlimited(),
        )
        .unwrap();
    let hits = index
        .snapshot()
        .text_search_boolean(query(), 10, |_| true, &mut unlimited())
        .unwrap();
    // Vertex 1 must not combine old red with new blue. Vertex 2 must not
    // inherit the old version's excluded term. Each fact is generation-local.
    assert_eq!(
        hits.iter().map(|hit| hit.id).collect::<Vec<_>>(),
        vec![VId(2)]
    );
    index
        .apply_batch([IndexMutation::Delete(VId(2))], &mut unlimited())
        .unwrap();
    assert!(
        index
            .snapshot()
            .text_search_boolean(query(), 10, |_| true, &mut unlimited())
            .unwrap()
            .is_empty()
    );
}

#[test]
fn one_shared_budget_covers_all_segments_and_failure_returns_no_prefix() {
    let mut index = BeaconIndex::new(IndexConfig::default()).unwrap();
    for id in 1..=4 {
        index
            .apply_batch(
                [IndexMutation::Upsert(document(id, Some("red blue green")))],
                &mut unlimited(),
            )
            .unwrap();
    }
    let snapshot = index.snapshot();
    assert_eq!(snapshot.stats().segments, 4);
    for required in ["", "red"] {
        let query = BooleanTextQuery {
            required,
            ..query()
        };
        let mut work = unlimited();
        let expected = snapshot
            .text_search_boolean(query, 3, |_| true, &mut work)
            .unwrap();
        let consumed = usize::MAX - work.remaining();
        for allowance in 0..consumed {
            assert_eq!(
                snapshot.text_search_boolean(query, 3, |_| true, &mut WorkBudget::new(allowance)),
                Err(BeaconError::WorkBudgetExceeded),
                "allowance {allowance} of {consumed}",
            );
        }
        let actual = snapshot
            .text_search_boolean(query, 3, |_| true, &mut WorkBudget::new(consumed))
            .unwrap();
        assert_eq!(bits(&actual), bits(&expected));
        assert_eq!(snapshot.stats(), index.snapshot().stats());
    }
}

#[test]
fn empty_snapshot_and_zero_k_still_validate_all_clauses() {
    let index = BeaconIndex::new(IndexConfig {
        text: Some(Bm25Config {
            max_query_bytes: 5,
            ..Bm25Config::default()
        }),
        ..IndexConfig::default()
    })
    .unwrap();
    let query = BooleanTextQuery {
        required: "red",
        excluded: "bad",
        ..BooleanTextQuery::default()
    };
    assert_eq!(
        index
            .snapshot()
            .text_search_boolean(query, 0, |_| true, &mut unlimited()),
        Err(BeaconError::ResourceLimit {
            resource: "text bytes",
            limit: 5
        }),
    );
}

#[test]
fn removing_a_text_lane_hides_old_postings_without_dropping_the_vertex() {
    let mut index = BeaconIndex::build(
        IndexConfig::default(),
        [document(u128::MAX, Some("red blue green"))],
        &mut unlimited(),
    )
    .unwrap();
    let pinned = index.snapshot();
    index
        .apply_batch(
            [IndexMutation::Upsert(document(u128::MAX, None))],
            &mut unlimited(),
        )
        .unwrap();
    let latest = index.snapshot();
    assert_eq!(latest.stats().documents, 1);
    assert_eq!(latest.stats().text.documents, 0);
    assert!(
        latest
            .text_search_boolean(query(), 1, |_| true, &mut unlimited())
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        pinned
            .text_search_boolean(query(), 1, |_| true, &mut unlimited())
            .unwrap()[0]
            .id,
        VId(u128::MAX)
    );
}
