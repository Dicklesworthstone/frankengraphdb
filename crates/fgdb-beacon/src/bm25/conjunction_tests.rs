use super::*;
use crate::{Bm25, TextHit, WorkBudget};
use std::cell::RefCell;

fn unlimited() -> WorkBudget {
    WorkBudget::new(usize::MAX)
}

fn bits(hits: &[TextHit]) -> Vec<(VId, u64)> {
    hits.iter()
        .map(|hit| (hit.id, hit.score.to_bits()))
        .collect()
}

// Reconstruct a token tape, then compare windows. This reference does NOT use
// posting-list intersection, the rare-term positional anchor, or a top-k heap.
fn tape(text: &AnalyzedText) -> Vec<&str> {
    let mut tokens = vec![""; usize::try_from(text.length).unwrap()];
    for (term, positions) in &text.frequencies {
        for &position in positions.iter() {
            tokens[usize::try_from(position).unwrap()] = term;
        }
    }
    tokens
}

fn reference(
    index: &Bm25,
    documents: &[(VId, String)],
    query: &str,
    mode: TextMatch,
    k: usize,
    eligible: impl Fn(VId) -> bool,
) -> Vec<TextHit> {
    let terms = index.config.query_terms(query, &mut unlimited()).unwrap();
    if terms.frequencies.is_empty() || index.corpus.total_length == 0 {
        return Vec::new();
    }
    let query_tape = tape(&terms);
    let average_length = index.corpus.total_length as f64 / index.corpus.documents as f64;
    let mut rows = Vec::new();
    for (id, text) in documents {
        if !eligible(*id) {
            continue;
        }
        let text = index
            .config
            .analyze_document(text, &mut unlimited())
            .unwrap();
        let common = terms
            .frequencies
            .keys()
            .filter(|term| text.frequencies.contains_key(*term))
            .count();
        let admitted = match mode {
            TextMatch::Any => common != 0,
            TextMatch::All => common == terms.frequencies.len(),
            TextMatch::Phrase => tape(&text)
                .windows(query_tape.len())
                .any(|window| window == query_tape.as_slice()),
            TextMatch::Fuzzy { .. } => unreachable!("reference is exact-token only"),
        };
        if !admitted {
            continue;
        }
        let length_norm =
            1.0 - index.config.b + index.config.b * text.length as f64 / average_length;
        let mut score = 0.0;
        for term in terms.frequencies.keys() {
            if let Some(posting) = text.frequencies.get(term) {
                let df = index.document_frequency(term);
                let ratio = ((index.corpus.documents - df) as f64 + 0.5) / (df as f64 + 0.5);
                // Reuse only the unchanged single-term arithmetic. Selection,
                // phrase admission and complete ordering are independent.
                score +=
                    ratio.ln_1p() * saturation(index.config.k1, length_norm, posting.len() as f64);
            }
        }
        rows.push(TextHit { id: *id, score });
    }
    rows.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.id.cmp(&b.id)));
    rows.truncate(k);
    rows
}

fn corpus() -> Vec<(VId, String)> {
    [
        (0, ""),
        (1, "red red blue café"),
        (2, "red blue red"),
        (3, "blue red red"),
        (4, "red filler red filler"),
        (5, "red red many many many extra extra words"),
        (1_u128 << 96, "CAFÉ red—blue red"),
        (u128::MAX - 1, "red red blue café"),
        (u128::MAX, "red blue"),
    ]
    .into_iter()
    .map(|(id, text)| (VId(id), text.to_owned()))
    .collect()
}

#[test]
fn conjunctions_match_document_scan_bits_for_repetition_unicode_filters_and_ties() {
    let documents = corpus();
    for k1 in [0.1, 1.2, f64::MAX] {
        for b in [0.0, 0.75, 1.0] {
            let config = Bm25Config {
                k1,
                b,
                ..Bm25Config::default()
            };
            let index = Bm25::build(config, documents.clone(), &mut unlimited()).unwrap();
            for query in [
                "",
                "!!!",
                "red",
                "red red",
                "red blue",
                "blue red",
                "CAFÉ red",
                "red missing",
            ] {
                for mode in [TextMatch::Any, TextMatch::All, TextMatch::Phrase] {
                    for k in [0, 1, 4, 20] {
                        for filter in 0..3 {
                            let eligible = |id: VId| match filter {
                                0 => true,
                                1 => id.0.is_multiple_of(2),
                                _ => false,
                            };
                            let actual = index
                                .search(query, k, mode, eligible, &mut unlimited())
                                .unwrap();
                            let expected = reference(&index, &documents, query, mode, k, eligible);
                            assert_eq!(
                                bits(&actual),
                                bits(&expected),
                                "{query:?} {mode:?} k={k} filter={filter} k1={k1} b={b}",
                            );
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn cursor_seeks_forward_without_identity_arithmetic_or_revisiting_exhaustion() {
    let postings: PostingList = (0..64_u128)
        .chain([1_u128 << 96, u128::MAX])
        .map(|id| (VId(id), Arc::new(vec![0])))
        .collect();
    let mut cursor = PostingCursor::new(&postings, 1.0, &mut unlimited()).unwrap();
    SEEKS.with(|count| count.set(0));
    for (target, expected) in [
        (1, 1),
        (40, 40),
        (2, 40),
        (1_u128 << 95, 1_u128 << 96),
        (u128::MAX, u128::MAX),
    ] {
        cursor.seek(VId(target), &mut unlimited()).unwrap();
        assert_eq!(cursor.head.map(|(id, _)| id), Some(VId(expected)));
    }
    assert_eq!(SEEKS.with(std::cell::Cell::get), 2);
    cursor.advance(&mut unlimited()).unwrap();
    cursor.seek(VId(0), &mut unlimited()).unwrap();
    assert!(cursor.head.is_none());
}

#[test]
fn selective_queries_seek_past_common_postings_and_dense_queries_do_not_reseek() {
    let documents: Vec<_> = (0..4096_u128)
        .map(|id| {
            let text = if matches!(id, 1000 | 3000) {
                "common rare"
            } else {
                "common"
            };
            (VId(id), text.to_owned())
        })
        .collect();
    let index = Bm25::build(Bm25Config::default(), documents, &mut unlimited()).unwrap();
    for mode in [TextMatch::All, TextMatch::Phrase] {
        NEXTS.with(|count| count.set(0));
        SEEKS.with(|count| count.set(0));
        let seen = RefCell::new(Vec::new());
        let mut work = WorkBudget::new(1000);
        let rows = index
            .search(
                "common rare",
                10,
                mode,
                |id| {
                    seen.borrow_mut().push(id);
                    true
                },
                &mut work,
            )
            .unwrap();
        assert_eq!(
            rows.iter().map(|row| row.id).collect::<Vec<_>>(),
            [VId(1000), VId(3000)],
        );
        assert_eq!(seen.into_inner(), [VId(1000), VId(3000)]);
        assert!(NEXTS.with(std::cell::Cell::get) < 16);
        assert_eq!(SEEKS.with(std::cell::Cell::get), 2);
        assert!(
            work.remaining() > 500,
            "query must not bill a full union scan"
        );
    }
    let dense = Bm25::build(
        Bm25Config::default(),
        (0..256_u128).map(|id| (VId(id), "common rare".to_owned())),
        &mut unlimited(),
    )
    .unwrap();
    SEEKS.with(|count| count.set(0));
    let rows = dense
        .search("common rare", 3, TextMatch::All, |_| true, &mut unlimited())
        .unwrap();
    assert_eq!(
        rows.iter().map(|row| row.id).collect::<Vec<_>>(),
        [VId(0), VId(1), VId(2)],
    );
    assert_eq!(SEEKS.with(std::cell::Cell::get), 0);
}

#[test]
fn phrase_and_endpoint_admission_precede_one_candidate_limit() {
    let documents = vec![
        (VId(1), "red blue red blue".to_owned()),
        (VId(2), "red red many many many many many".to_owned()),
        (VId(3), "red red many many many many many many".to_owned()),
    ];
    let index = Bm25::build(Bm25Config::default(), documents.clone(), &mut unlimited()).unwrap();
    let all = index
        .search("red red", 1, TextMatch::All, |_| true, &mut unlimited())
        .unwrap();
    assert_eq!(
        all[0].id,
        VId(1),
        "negative control must favor a non-phrase"
    );
    let phrase = index
        .search(
            "red red",
            1,
            TextMatch::Phrase,
            |id| id != VId(2),
            &mut unlimited(),
        )
        .unwrap();
    assert_eq!(phrase[0].id, VId(3));
    assert_eq!(
        bits(&phrase),
        bits(&reference(
            &index,
            &documents,
            "red red",
            TextMatch::Phrase,
            1,
            |id| id != VId(2),
        )),
    );
}

#[derive(Default)]
struct Trace {
    charges: Vec<usize>,
    stop: Option<(usize, BeaconError)>,
}
impl WorkControl for Trace {
    fn charge(&mut self, units: usize) -> Result<(), BeaconError> {
        let at = self.charges.len();
        self.charges.push(units);
        if let Some((stop, error)) = &self.stop
            && *stop == at
        {
            return Err(error.clone());
        }
        Ok(())
    }
}

#[test]
fn every_seek_scoring_and_phrase_refusal_returns_no_prefix_and_leaves_index_reusable() {
    let index = Bm25::build(Bm25Config::default(), corpus(), &mut unlimited()).unwrap();
    let stats = index.stats();
    for mode in [TextMatch::All, TextMatch::Phrase] {
        let mut original = Trace::default();
        let expected = index
            .search("red red blue", 4, mode, |_| true, &mut original)
            .unwrap();
        assert!(!expected.is_empty());
        for reason in [BeaconError::Cancelled, BeaconError::WorkBudgetExceeded] {
            for cutoff in 0..original.charges.len() {
                let mut work = Trace {
                    stop: Some((cutoff, reason.clone())),
                    ..Trace::default()
                };
                let result = index.search("red red blue", 4, mode, |_| true, &mut work);
                assert_eq!(result, Err(reason.clone()), "cutoff={cutoff} mode={mode:?}");
                assert_eq!(work.charges, original.charges[..=cutoff]);
                assert_eq!(index.stats(), stats);
            }
        }
        let units: usize = original.charges.iter().sum();
        assert_eq!(
            index.search(
                "red red blue",
                4,
                mode,
                |_| true,
                &mut WorkBudget::new(units - 1),
            ),
            Err(BeaconError::WorkBudgetExceeded),
        );
        let actual = index
            .search(
                "red red blue",
                4,
                mode,
                |_| true,
                &mut WorkBudget::new(units),
            )
            .unwrap();
        assert_eq!(bits(&actual), bits(&expected));
    }
}

#[test]
fn absent_terms_empty_queries_and_zero_k_keep_complete_query_validation() {
    let config = Bm25Config {
        max_query_terms: 2,
        ..Bm25Config::default()
    };
    let index = Bm25::build(config, corpus(), &mut unlimited()).unwrap();
    for mode in [TextMatch::All, TextMatch::Phrase] {
        for query in ["", "!!!", "absent", "red absent"] {
            assert!(
                index
                    .search(
                        query,
                        10,
                        mode,
                        |_| panic!("no matching candidate"),
                        &mut unlimited(),
                    )
                    .unwrap()
                    .is_empty()
            );
        }
        for k in [0, 10] {
            assert_eq!(
                index.search("red blue third", k, mode, |_| false, &mut unlimited()),
                Err(BeaconError::ResourceLimit {
                    resource: "distinct analyzed terms",
                    limit: 2,
                }),
            );
        }
    }
}
