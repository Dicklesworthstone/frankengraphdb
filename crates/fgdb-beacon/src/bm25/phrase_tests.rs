use super::*;
use crate::{BeaconIndex, IndexConfig, IndexDocument, IndexMutation, WorkBudget};

fn budget() -> WorkBudget {
    WorkBudget::new(10_000_000)
}

fn build(documents: &[&str]) -> Bm25 {
    Bm25::build(
        Bm25Config::default(),
        documents.iter().enumerate().map(|(id, text)| (VId(id as u128), (*text).into())),
        &mut budget(),
    ).unwrap()
}

fn ids(index: &Bm25, query: &str, mode: TextMatch) -> Vec<VId> {
    let mut ids: Vec<_> = index.search(query, 1000, mode, |_| true, &mut budget())
        .unwrap().into_iter().map(|hit| hit.id).collect();
    ids.sort();
    ids
}

#[test]
fn phrases_keep_order_repetitions_boundaries_and_existing_tokenization() {
    let index = build(&[
        "red blue", "blue red", "red x blue", "red red blue", "red blue red",
        "red", "", "RED,\nBLUE!", "cafÉ Σ42", "red blue blue",
    ]);
    assert_eq!(ids(&index, "red blue", TextMatch::Phrase),
        [VId(0), VId(3), VId(4), VId(7), VId(9)]);
    assert_eq!(ids(&index, "blue red", TextMatch::Phrase), [VId(1), VId(4)]);
    assert_eq!(ids(&index, "red red blue", TextMatch::Phrase), [VId(3)]);
    assert_eq!(ids(&index, "blue blue", TextMatch::Phrase), [VId(9)]);
    assert_eq!(ids(&index, "red blue red", TextMatch::Phrase), [VId(4)]);
    assert!(ids(&index, "red red red", TextMatch::Phrase).is_empty());
    assert!(ids(&index, "red absent", TextMatch::Phrase).is_empty());
    assert!(ids(&index, "?!", TextMatch::Phrase).is_empty());
    assert_eq!(ids(&index, "café σ42", TextMatch::Phrase), [VId(8)]);
    // No stemming, accent stripping, normalization or substring matching.
    assert!(ids(&index, "cafe σ42", TextMatch::Phrase).is_empty());
    assert!(ids(&index, "re blue", TextMatch::Phrase).is_empty());
    assert_eq!(ids(&index, "red", TextMatch::Phrase), ids(&index, "red", TextMatch::All));
    assert_eq!(ids(&index, "red red blue", TextMatch::All),
        ids(&index, "blue red", TextMatch::All));
}

#[test]
fn phrase_admission_precedes_top_k_and_does_not_change_bm25_scores() {
    // Every reversed occurrence outranks the longer phrase on plain BM25.
    let index = build(&["blue red", "blue red", "padding red blue padding padding"]);
    let plain = index.search("red blue", 3, TextMatch::All, |_| true, &mut budget()).unwrap();
    assert_eq!(plain[0].id, VId(0));
    let phrase = index.search("red blue", 1, TextMatch::Phrase, |_| true, &mut budget()).unwrap();
    assert_eq!(phrase.len(), 1);
    assert_eq!(phrase[0].id, VId(2));
    assert_eq!(phrase[0].score, plain.iter().find(|hit| hit.id == VId(2)).unwrap().score);
    assert!(index.search("red blue", 1, TextMatch::Phrase, |id| id != VId(2), &mut budget())
        .unwrap().is_empty());
    let repeated = index.search("red red", 1, TextMatch::Phrase, |_| true, &mut budget()).unwrap();
    assert!(repeated.is_empty(), "a query repetition is not a deduplicated term filter");
}

fn words(max_length: usize) -> Vec<Vec<&'static str>> {
    let mut rows = Vec::new();
    for length in 0..=max_length {
        for bits in 0..(1usize << length) {
            rows.push((0..length).map(|position| {
                if bits & (1 << position) == 0 { "a" } else { "b" }
            }).collect());
        }
    }
    rows
}

#[test]
fn exhaustive_small_corpus_matches_independent_token_windows_and_legacy_scores() {
    let documents = words(5);
    let index = Bm25::build(Bm25Config::default(), documents.iter().enumerate()
        .map(|(id, words)| (VId(id as u128), words.join(" "))), &mut budget()).unwrap();
    for query in words(4) {
        let text = query.join(" ");
        // Independent reference: direct sequence windows, not production
        // analysis, postings, phrase helpers, or a copy of anchor alignment.
        let expected: Vec<_> = documents.iter().enumerate().filter_map(|(id, words)| {
            (!query.is_empty() && words.windows(query.len().max(1)).any(|w| w == query))
                .then_some(VId(id as u128))
        }).collect();
        assert_eq!(ids(&index, &text, TextMatch::Phrase), expected, "{text:?}");
        let all = index.search(&text, 1000, TextMatch::All, |_| true, &mut budget()).unwrap();
        let expected_hits: Vec<_> = all.into_iter().filter(|hit| expected.contains(&hit.id)).collect();
        for k in [0, 1, 3, 1000] {
            let actual = index.search(&text, k, TextMatch::Phrase, |_| true, &mut budget()).unwrap();
            assert_eq!(actual, expected_hits.iter().take(k).copied().collect::<Vec<_>>(), "{text:?}, k={k}");
        }
    }
}

fn document(id: u128, text: &str) -> IndexDocument {
    IndexDocument { id: VId(id), text: Some(text.into()), vector: None }
}

#[test]
fn updates_deletes_compaction_and_failed_apply_preserve_positional_generations() {
    for max_segments in [1, 8] {
        let config = IndexConfig { max_segments, ..IndexConfig::default() };
        let mut index = BeaconIndex::build(config, [document(1, "red blue"), document(2, "blue red")], &mut budget()).unwrap();
        let old = index.snapshot();
        let before = old.text_search("red blue", 10, TextMatch::Phrase, |_| true, &mut budget()).unwrap();
        assert_eq!(before.iter().map(|hit| hit.id).collect::<Vec<_>>(), [VId(1)]);
        index.apply_batch([
            IndexMutation::Upsert(document(1, "blue red")),
            IndexMutation::Upsert(document(2, "red blue")),
            IndexMutation::Upsert(document(3, "red red blue")),
        ], &mut budget()).unwrap();
        let rebuilt = BeaconIndex::build(index.config().clone(), [document(1, "blue red"),
            document(2, "red blue"), document(3, "red red blue")], &mut budget()).unwrap();
        for phrase in ["red blue", "blue red", "red red blue"] {
            assert_eq!(index.snapshot().text_search(phrase, 10, TextMatch::Phrase, |_| true, &mut budget()).unwrap(),
                rebuilt.snapshot().text_search(phrase, 10, TextMatch::Phrase, |_| true, &mut budget()).unwrap());
        }
        let prior = index.snapshot();
        assert!(index.apply_batch([IndexMutation::Upsert(document(2, "red red"))], &mut WorkBudget::new(0)).is_err());
        assert_eq!(prior.text_search("red blue", 10, TextMatch::Phrase, |_| true, &mut budget()).unwrap(),
            index.snapshot().text_search("red blue", 10, TextMatch::Phrase, |_| true, &mut budget()).unwrap());
        index.apply_batch([IndexMutation::Delete(VId(2)), IndexMutation::Delete(VId(3))], &mut budget()).unwrap();
        assert!(index.snapshot().text_search("red blue", 10, TextMatch::Phrase, |_| true, &mut budget()).unwrap().is_empty());
        assert_eq!(old.text_search("red blue", 10, TextMatch::Phrase, |_| true, &mut budget()).unwrap(), before);
    }
}

#[test]
fn positional_payloads_are_shared_and_repeated_terms_remain_byte_bounded() {
    let config = Bm25Config { max_document_bytes: 5, max_document_terms: 1,
        max_query_bytes: 5, max_query_terms: 1, ..Bm25Config::default() };
    let text = config.analyze_document("a a a", &mut budget()).unwrap();
    assert_eq!(text.frequencies["a"].as_slice(), &[0, 1, 2]);
    assert_eq!(text.length, 3);
    let segment = TextSegment::build([(VId(1), &text)], &mut budget()).unwrap();
    assert!(Arc::ptr_eq(&text.frequencies["a"], &segment.postings["a"][&VId(1)]));
    assert!(matches!(config.analyze_document("a a a a", &mut budget()),
        Err(BeaconError::ResourceLimit { resource: "text bytes", limit: 5 })));
    assert!(matches!(config.query_terms("a a a a", &mut budget()),
        Err(BeaconError::ResourceLimit { resource: "text bytes", limit: 5 })));
    let index = Bm25::build(config, [(VId(1), "a a a".into())], &mut budget()).unwrap();
    assert_eq!(ids(&index, "a a a", TextMatch::Phrase), [VId(1)]);
}

#[derive(Default)]
struct Cut { calls: usize, stop: Option<usize> }
impl WorkControl for Cut {
    fn charge(&mut self, _: usize) -> Result<(), BeaconError> {
        self.calls += 1;
        if self.stop == Some(self.calls) { Err(BeaconError::Cancelled) } else { Ok(()) }
    }
}

#[test]
fn every_phrase_checkpoint_refuses_without_partial_results_or_mutating_the_index() {
    // The rare anchor lies late in the phrase. Earlier occurrences underflow
    // its alignment; later candidates test repetition and suffix bounds.
    let index = build(&["b a a a b a a b", "a a b", "a b a", "b"]);
    for query in ["a a b", "b a a", "a a a", ""] {
        let mut trace = Cut::default();
        let expected = index.search(query, 10, TextMatch::Phrase, |_| true, &mut trace).unwrap();
        assert!(trace.calls > 0);
        for stop in 1..=trace.calls {
            let mut cut = Cut { stop: Some(stop), calls: 0 };
            assert!(matches!(index.search(query, 10, TextMatch::Phrase, |_| true, &mut cut),
                Err(BeaconError::Cancelled)), "{query:?}, cut={stop}");
            assert_eq!(cut.calls, stop);
            assert_eq!(index.search(query, 10, TextMatch::Phrase, |_| true, &mut budget()).unwrap(), expected);
        }
    }
}
