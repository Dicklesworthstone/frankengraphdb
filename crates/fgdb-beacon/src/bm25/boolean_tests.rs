use super::*;
use crate::{TextMatch, WorkBudget};
use std::collections::BTreeSet;

fn unlimited() -> WorkBudget {
    WorkBudget::new(usize::MAX)
}

fn tokens(text: &str) -> BTreeSet<String> {
    text.split(|ch: char| !ch.is_alphanumeric())
        .filter(|term| !term.is_empty())
        .map(|term| term.chars().flat_map(char::to_lowercase).collect())
        .collect()
}

fn bits(hits: &[TextHit]) -> Vec<(VId, u64)> {
    hits.iter()
        .map(|hit| (hit.id, hit.score.to_bits()))
        .collect()
}

fn documents() -> Vec<(VId, String)> {
    [
        (0, ""),
        (1, "red red blue café"),
        (2, "red green"),
        (3, "blue green banned"),
        (4, "red blue green"),
        (5, "red red banned"),
        (1_u128 << 100, "CAFÉ red—blue"),
        (u128::MAX, "red blue green"),
    ]
    .into_iter()
    .map(|(id, text)| (VId(id), text.to_owned()))
    .collect()
}

// The admission oracle scans token sets rather than the Boolean posting
// driver. Existing Any retrieval supplies only the unchanged BM25 scorer.
fn reference(
    index: &Bm25,
    documents: &[(VId, String)],
    query: BooleanTextQuery<'_>,
    k: usize,
    filtered: bool,
) -> Vec<TextHit> {
    let required = tokens(query.required);
    let optional = tokens(query.optional);
    let excluded = tokens(query.excluded);
    let minimum = query.minimum_optional.max(usize::from(required.is_empty()));
    let admitted: BTreeSet<VId> = documents
        .iter()
        .filter(|(id, text)| {
            let document = tokens(text);
            (!filtered || id.0.is_multiple_of(2))
                && required.is_subset(&document)
                && optional.intersection(&document).count() >= minimum
                && excluded.is_disjoint(&document)
                && (!required.is_empty() || !optional.is_empty())
        })
        .map(|(id, _)| *id)
        .collect();
    index
        .search(
            &format!("{} {}", query.required, query.optional),
            k,
            TextMatch::Any,
            |id| admitted.contains(&id),
            &mut unlimited(),
        )
        .unwrap()
}

#[test]
fn boolean_matches_scan_oracle_and_bm25_bits() {
    let documents = documents();
    for k1 in [0.1, 1.2, f64::MAX] {
        for b in [0.0, 0.75, 1.0] {
            let index = Bm25::build(
                Bm25Config {
                    k1,
                    b,
                    ..Bm25Config::default()
                },
                documents.clone(),
                &mut unlimited(),
            )
            .unwrap();
            for required in ["", "red", "red blue", "CAFÉ", "missing"] {
                for optional in ["", "blue green", "red red blue", "green missing"] {
                    for excluded in ["", "banned", "red", "missing"] {
                        for minimum_optional in 0..=3 {
                            let query = BooleanTextQuery {
                                required,
                                optional,
                                excluded,
                                minimum_optional,
                            };
                            for k in [0, 1, 3, 100] {
                                for filtered in [false, true] {
                                    let actual = index
                                        .search_boolean(
                                            query,
                                            k,
                                            |id| !filtered || id.0.is_multiple_of(2),
                                            &mut unlimited(),
                                        )
                                        .unwrap();
                                    let expected =
                                        reference(&index, &documents, query, k, filtered);
                                    assert_eq!(bits(&actual), bits(&expected), "{query:?}, k={k}");
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn exclusions_and_optional_minimum_precede_top_k() {
    let index = Bm25::build(
        Bm25Config::default(),
        [
            (VId(1), "red banned".into()),
            (VId(2), "red blue green filler filler".into()),
        ],
        &mut unlimited(),
    )
    .unwrap();
    let query = BooleanTextQuery {
        required: "red",
        optional: "blue green",
        excluded: "banned",
        minimum_optional: 2,
    };
    let hits = index
        .search_boolean(query, 1, |_| true, &mut unlimited())
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].id, VId(2));
}

#[test]
fn clauses_share_limits_even_with_no_requested_results() {
    let config = Bm25Config {
        max_query_bytes: 12,
        max_query_terms: 2,
        ..Bm25Config::default()
    };
    let index = Bm25::build(config, documents(), &mut unlimited()).unwrap();
    for query in [
        BooleanTextQuery {
            required: "red",
            optional: "blue",
            excluded: "green",
            ..BooleanTextQuery::default()
        },
        BooleanTextQuery {
            required: "red",
            optional: "red",
            excluded: "red",
            ..BooleanTextQuery::default()
        },
        BooleanTextQuery {
            required: "red red",
            optional: "red red",
            ..BooleanTextQuery::default()
        },
    ] {
        assert!(matches!(
            index.search_boolean(query, 0, |_| true, &mut unlimited()),
            Err(BeaconError::ResourceLimit { .. })
        ));
    }
    let accepted = BooleanTextQuery {
        required: "red red",
        optional: "blue",
        ..BooleanTextQuery::default()
    };
    assert!(
        index
            .search_boolean(accepted, 10, |_| true, &mut unlimited())
            .is_ok()
    );
}

#[test]
fn every_insufficient_work_prefix_refuses_without_partial_success() {
    let index = Bm25::build(Bm25Config::default(), documents(), &mut unlimited()).unwrap();
    // Exercise both rare-required and optional-only posting drivers.
    for required in ["", "red"] {
        let query = BooleanTextQuery {
            required,
            optional: "blue green red",
            excluded: "banned",
            minimum_optional: 1,
        };
        struct Count(usize);
        impl WorkControl for Count {
            fn charge(&mut self, amount: usize) -> Result<(), BeaconError> {
                self.0 += amount;
                Ok(())
            }
        }
        let mut count = Count(0);
        let expected = index
            .search_boolean(query, 3, |_| true, &mut count)
            .unwrap();
        for allowance in 0..count.0 {
            assert!(
                index
                    .search_boolean(query, 3, |_| true, &mut WorkBudget::new(allowance))
                    .is_err(),
                "allowance {allowance} of {}",
                count.0
            );
        }
        let actual = index
            .search_boolean(query, 3, |_| true, &mut WorkBudget::new(count.0))
            .unwrap();
        assert_eq!(bits(&actual), bits(&expected));
    }
}

#[test]
fn empty_negative_only_and_impossible_queries_do_not_match() {
    let index = Bm25::build(Bm25Config::default(), documents(), &mut unlimited()).unwrap();
    for query in [
        BooleanTextQuery::default(),
        BooleanTextQuery {
            excluded: "banned",
            ..BooleanTextQuery::default()
        },
        BooleanTextQuery {
            optional: "red red",
            minimum_optional: 2,
            ..BooleanTextQuery::default()
        },
        BooleanTextQuery {
            required: "red",
            excluded: "RED",
            ..BooleanTextQuery::default()
        },
        BooleanTextQuery {
            required: "---",
            optional: "!!!",
            ..BooleanTextQuery::default()
        },
    ] {
        assert!(
            index
                .search_boolean(query, 10, |_| true, &mut unlimited())
                .unwrap()
                .is_empty()
        );
    }
}

#[test]
fn debug_does_not_disclose_clause_text() {
    let debug = format!(
        "{:?}",
        BooleanTextQuery {
            required: "private-required",
            optional: "private-optional",
            excluded: "private-excluded",
            minimum_optional: 2,
        }
    );
    assert!(!debug.contains("private"));
    assert!(debug.contains("minimum_optional: 2"));
}

#[test]
fn caller_scratch_refusal_and_cancellation_are_not_empty_successes() {
    struct RefuseScratch;
    impl WorkControl for RefuseScratch {
        fn charge(&mut self, _: usize) -> Result<(), BeaconError> {
            Ok(())
        }
        fn reserve_scratch(&mut self, _: usize) -> Result<(), BeaconError> {
            Err(BeaconError::ResourceLimit {
                resource: "test scratch",
                limit: 0,
            })
        }
    }
    struct Cancel;
    impl WorkControl for Cancel {
        fn charge(&mut self, _: usize) -> Result<(), BeaconError> {
            Err(BeaconError::Cancelled)
        }
    }
    let index = Bm25::build(Bm25Config::default(), documents(), &mut unlimited()).unwrap();
    let query = BooleanTextQuery {
        required: "red",
        ..BooleanTextQuery::default()
    };
    assert!(matches!(
        index.search_boolean(query, 3, |_| true, &mut RefuseScratch),
        Err(BeaconError::ResourceLimit {
            resource: "test scratch",
            limit: 0
        })
    ));
    assert_eq!(
        index.search_boolean(query, 0, |_| true, &mut Cancel),
        Err(BeaconError::Cancelled)
    );
}
