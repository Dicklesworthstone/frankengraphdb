use fgdb_beacon::{BeaconError, Bm25, Bm25Config, TextMatch, WorkBudget};
use fgdb_types::VId;

fn budget() -> WorkBudget { WorkBudget::new(10_000_000) }

fn corpus() -> Bm25 {
    Bm25::build(Bm25Config::default(), [
        (VId(1), "a a b".to_owned()), (VId(2), "a c".to_owned()), (VId(3), "c".to_owned()),
    ], &mut budget()).unwrap()
}

#[test]
fn hand_calculated_bm25_scores() {
    let index = corpus();
    let hits = index.search("a", 10, TextMatch::Any, |_| true, &mut budget()).unwrap();
    assert_eq!(hits.iter().map(|hit| hit.id).collect::<Vec<_>>(), [VId(1), VId(2)]);
    let idf = (1.0f64 + 1.5 / 2.5).ln();
    let expected_first = idf * (2.0 * 2.2) / (2.0 + 1.2 * (0.25 + 0.75 * 3.0 / 2.0));
    assert!((hits[0].score - expected_first).abs() < 1e-14);
    assert!((hits[1].score - idf).abs() < 1e-14);
    assert_eq!(index.stats().documents, 3);
    assert_eq!(index.stats().total_tokens, 6);
    assert_eq!(index.document_frequency("a"), 2);
}

#[test]
fn and_or_unknown_and_repeated_terms() {
    let index = corpus();
    let both = index.search("a c", 10, TextMatch::All, |_| true, &mut budget()).unwrap();
    assert_eq!(both.iter().map(|hit| hit.id).collect::<Vec<_>>(), [VId(2)]);
    assert!(index.search("a missing", 10, TextMatch::All, |_| true, &mut budget()).unwrap().is_empty());
    let a = index.search("a", 10, TextMatch::Any, |_| true, &mut budget()).unwrap();
    assert_eq!(a, index.search("A a A", 10, TextMatch::Any, |_| true, &mut budget()).unwrap());
    assert_eq!(a, index.search("a missing", 10, TextMatch::Any, |_| true, &mut budget()).unwrap());
    assert!(index.search("...", 10, TextMatch::Any, |_| true, &mut budget()).unwrap().is_empty());
}

#[test]
fn analyzer_is_case_insensitive_and_splits_punctuation() {
    let index = Bm25::build(Bm25Config::default(), [(VId(1), "CAFÉ, Straße/東京42".to_owned())], &mut budget()).unwrap();
    assert_eq!(index.search("café straße 東京42", 10, TextMatch::All, |_| true, &mut budget()).unwrap().len(), 1);
    assert_eq!(index.stats().total_tokens, 3);
    assert!(!format!("{index:?}").contains("CAFÉ"));
}

#[test]
fn empty_text_counts_in_the_corpus_but_cannot_match() {
    let index = Bm25::build(Bm25Config::default(), [(VId(1), String::new()), (VId(2), "word".to_owned())], &mut budget()).unwrap();
    assert_eq!(index.stats().documents, 2);
    assert_eq!(index.stats().total_tokens, 1);
    let hits = index.search("word", 10, TextMatch::Any, |_| true, &mut budget()).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].id, VId(2));
    let all_empty = Bm25::build(Bm25Config::default(), [(VId(1), String::new())], &mut budget()).unwrap();
    assert!(all_empty.search("word", 10, TextMatch::Any, |_| true, &mut budget()).unwrap().is_empty());
}

#[test]
fn equal_scores_are_ordered_by_id_and_filters_do_not_change_idf() {
    let index = Bm25::build(Bm25Config::default(), [(VId(8), "x".to_owned()), (VId(2), "x".to_owned())], &mut budget()).unwrap();
    let all = index.search("x", 2, TextMatch::Any, |_| true, &mut budget()).unwrap();
    assert_eq!(all[0].id, VId(2));
    let filtered = index.search("x", 1, TextMatch::Any, |id| id == VId(8), &mut budget()).unwrap();
    assert_eq!(filtered[0], all[1]);
}

#[test]
fn length_normalization_can_be_disabled() {
    let config = Bm25Config { b: 0.0, ..Bm25Config::default() };
    let index = Bm25::build(config, [(VId(1), "x padding padding".to_owned()), (VId(2), "x".to_owned())], &mut budget()).unwrap();
    let hits = index.search("x", 2, TextMatch::Any, |_| true, &mut budget()).unwrap();
    assert_eq!(hits[0].score, hits[1].score);
    assert_eq!(hits[0].id, VId(1));
}

#[test]
fn invalid_bm25_parameters_and_resource_limits_fail() {
    for k1 in [0.0, -1.0, f64::NAN, f64::INFINITY] {
        assert!(Bm25Config { k1, ..Bm25Config::default() }.validate().is_err());
    }
    for b in [-0.1, 1.1, f64::NAN] {
        assert!(Bm25Config { b, ..Bm25Config::default() }.validate().is_err());
    }
    let config = Bm25Config { max_document_bytes: 3, ..Bm25Config::default() };
    assert!(matches!(Bm25::build(config, [(VId(1), "four".to_owned())], &mut budget()), Err(BeaconError::ResourceLimit { .. })));
    let config = Bm25Config { max_query_terms: 1, ..Bm25Config::default() };
    let index = Bm25::build(config, [(VId(1), "a b".to_owned())], &mut budget()).unwrap();
    assert!(matches!(index.search("a b", 0, TextMatch::Any, |_| true, &mut budget()), Err(BeaconError::ResourceLimit { .. })));
}

#[test]
fn duplicate_documents_and_exhausted_searches_are_refused() {
    assert!(matches!(Bm25::build(Bm25Config::default(), [(VId(1), "a".to_owned()), (VId(1), "b".to_owned())], &mut budget()), Err(BeaconError::DuplicateVertex(VId(1)))));
    assert_eq!(corpus().search("a", 10, TextMatch::Any, |_| true, &mut WorkBudget::new(1)).unwrap_err(), BeaconError::WorkBudgetExceeded);
}
