//! Unicode-scalar edit matching over Beacon's existing live ordered dictionary.
//!
//! This is not the region-owned, byte-keyed ART in fgdb-collections: byte edit
//! distance would miscount multibyte text. The bounded scalar automaton prunes
//! dead prefix ranges using the current dictionary without another stored trie.
//! It does not implement transpositions, grapheme edits, stemming or fuzzy
//! phrases. No dictionary, statistics or source outside the pinned corpus is
//! consulted. Expansion is repeated per segment under the SAME work allowance;
//! this is not an O(query length) lookup or the complete durable FTS/ART design.

use std::ops::Bound::{Excluded, Included, Unbounded};

use super::{AnalyzedText, Bm25Config, CorpusStats, EditDistance, TextSegment, saturation};
use crate::ranking::{Ranked, retain_best};
use crate::{BeaconError, WorkControl};
use fgdb_types::VId;
use std::collections::{BTreeMap, BinaryHeap, btree_map::Entry};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Match {
    Yes,
    No,
    /// All dictionary terms beginning with these UTF-8 bytes are impossible.
    DeadPrefix(usize),
}

fn cell(row: &[u8; 5], low: usize, high: usize, at: usize, dead: u8) -> u8 {
    if at < low || at > high {
        dead
    } else {
        row[at - low]
    }
}

/// A width <= 5 Levenshtein band: constant state, independent of token length.
/// Every scalar and dynamic-programming cell has a cancellation/work gate.
fn matches(
    pattern: &[char],
    candidate: &str,
    distance: EditDistance,
    work: &mut dyn WorkControl,
) -> Result<Match, BeaconError> {
    let edits = distance as usize;
    let dead = distance as u8 + 1;
    let (mut low, mut high) = (0, pattern.len().min(edits));
    let mut row = [dead; 5];
    for (index, value) in row.iter_mut().enumerate().take(high + 1) {
        *value = index as u8;
    }
    for (index, (byte, ch)) in candidate.char_indices().enumerate() {
        work.charge(ch.len_utf8())?;
        let depth = index + 1;
        let next_low = depth.saturating_sub(edits);
        let next_high = pattern.len().min(depth.saturating_add(edits));
        if next_low > next_high {
            return Ok(Match::DeadPrefix(byte + ch.len_utf8()));
        }
        let mut next = [dead; 5];
        let mut minimum = dead;
        for column in next_low..=next_high {
            work.charge(1)?;
            let delete = cell(&row, low, high, column, dead) + 1;
            let insert = if column > next_low {
                next[column - next_low - 1] + 1
            } else {
                dead
            };
            let replace = if column == 0 {
                dead
            } else {
                cell(&row, low, high, column - 1, dead) + u8::from(pattern[column - 1] != ch)
            };
            let value = dead.min(delete).min(insert).min(replace);
            next[column - next_low] = value;
            minimum = minimum.min(value);
        }
        if minimum == dead {
            return Ok(Match::DeadPrefix(byte + ch.len_utf8()));
        }
        row = next;
        low = next_low;
        high = next_high;
    }
    work.charge(1)?;
    Ok(if cell(&row, low, high, pattern.len(), dead) < dead {
        Match::Yes
    } else {
        Match::No
    })
}

/// Smallest valid UTF-8 lower bound beyond an entire prefix. UTF-8 byte order
/// agrees with scalar order; skip the surrogate gap rather than constructing
/// invalid UTF-8. All-maximum prefixes have no successor.
fn after_prefix(prefix: &str, work: &mut dyn WorkControl) -> Result<Option<String>, BeaconError> {
    for (offset, ch) in prefix.char_indices().rev() {
        work.charge(1)?;
        let code = u32::from(ch);
        if code == 0x10ffff {
            continue;
        }
        let next = if code == 0xd7ff { 0xe000 } else { code + 1 };
        let next = char::from_u32(next).ok_or(BeaconError::Invariant("fuzzy prefix successor"))?;
        let length = offset
            .checked_add(next.len_utf8())
            .ok_or(BeaconError::WorkBudgetExceeded)?;
        work.charge(length)?;
        let mut bound = String::new();
        bound
            .try_reserve_exact(length)
            .map_err(|_| BeaconError::ResourceLimit {
                resource: "fuzzy prefix bound",
                limit: length,
            })?;
        bound.push_str(&prefix[..offset]);
        bound.push(next);
        return Ok(Some(bound));
    }
    Ok(None)
}

/// Borrow vocabulary keys. A term appears once even if it matches several
/// input terms, but its ordered input-group list preserves all-term semantics.
struct Expansion<'a> {
    terms: BTreeMap<&'a str, Vec<usize>>,
    groups: usize,
}

impl<'a> Expansion<'a> {
    fn build(
        query: &AnalyzedText,
        corpus: &'a CorpusStats,
        distance: EditDistance,
        maximum: usize,
        work: &mut dyn WorkControl,
    ) -> Result<Self, BeaconError> {
        let mut result = Self {
            terms: BTreeMap::new(),
            groups: query.frequencies.len(),
        };
        for (group, term) in query.frequencies.keys().enumerate() {
            work.charge(term.len())?;
            let mut pattern = Vec::new();
            pattern
                .try_reserve_exact(term.len())
                .map_err(|_| BeaconError::ResourceLimit {
                    resource: "fuzzy pattern scalars",
                    limit: term.len(),
                })?;
            pattern.extend(term.chars());
            let mut after: Option<&str> = None;
            let mut skip: Option<String> = None;
            loop {
                // Bound comparison work before the seek; no full vocabulary
                // copy, result-sized candidate-start set, or unmetered tail.
                let bound_bytes = skip
                    .as_ref()
                    .map_or_else(|| after.map_or(1, str::len), String::len);
                let steps = 1 + corpus.frequencies.len().checked_ilog2().unwrap_or(0) as usize;
                work.charge(steps.saturating_mul(bound_bytes.max(1)))?;
                let entry = if let Some(bound) = skip.as_deref() {
                    corpus
                        .frequencies
                        .range::<str, _>((Included(bound), Unbounded))
                        .next()
                } else if let Some(bound) = after {
                    corpus
                        .frequencies
                        .range::<str, _>((Excluded(bound), Unbounded))
                        .next()
                } else {
                    corpus.frequencies.first_key_value()
                };
                let Some((candidate, &frequency)) = entry else {
                    break;
                };
                if frequency == 0 || frequency > corpus.documents {
                    return Err(BeaconError::Invariant(
                        "invalid fuzzy live document frequency",
                    ));
                }
                after = Some(candidate.as_str());
                skip = None;
                match matches(&pattern, candidate, distance, work)? {
                    Match::No => {}
                    Match::DeadPrefix(end) => {
                        skip = after_prefix(&candidate[..end], work)?;
                        if skip.is_none() {
                            break;
                        }
                    }
                    Match::Yes => {
                        work.charge(1)?;
                        let count = result.terms.len();
                        let groups = match result.terms.entry(candidate.as_str()) {
                            Entry::Occupied(entry) => entry.into_mut(),
                            Entry::Vacant(entry) => {
                                if count == maximum {
                                    return Err(BeaconError::ResourceLimit {
                                        resource: "fuzzy expanded terms",
                                        limit: maximum,
                                    });
                                }
                                entry.insert(Vec::new())
                            }
                        };
                        work.charge(1)?;
                        groups
                            .try_reserve(1)
                            .map_err(|_| BeaconError::ResourceLimit {
                                resource: "fuzzy term groups",
                                limit: result.groups,
                            })?;
                        groups.push(group);
                    }
                }
            }
        }
        Ok(result)
    }
}

impl TextSegment {
    /// Merge expanded postings before top-k. Matching vocabulary terms add
    /// their ordinary live-corpus BM25 score ONCE in lexical order; no score
    /// bonus, edit penalty, or duplicated contribution from overlapping groups.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn search_fuzzy_into(
        &self,
        query: &AnalyzedText,
        distance: EditDistance,
        require_all: bool,
        maximum: usize,
        config: &Bm25Config,
        corpus: &CorpusStats,
        limit: usize,
        eligible: &impl Fn(VId) -> bool,
        best: &mut BinaryHeap<Ranked>,
        work: &mut dyn WorkControl,
    ) -> Result<(), BeaconError> {
        work.charge(1)?;
        if limit == 0 || query.frequencies.is_empty() || corpus.total_length == 0 {
            return Ok(());
        }
        // Expand against the LIVE vocabulary, not obsolete per-segment words.
        // The cap is global to that vocabulary, not a per-segment truncation.
        let expansion = Expansion::build(query, corpus, distance, maximum, work)?;
        let mut streams = Vec::new();
        work.charge(expansion.terms.len())?;
        streams
            .try_reserve_exact(expansion.terms.len())
            .map_err(|_| BeaconError::ResourceLimit {
                resource: "fuzzy posting streams",
                limit: maximum,
            })?;
        let mut covered = Vec::new();
        work.charge(expansion.groups)?;
        covered
            .try_reserve_exact(expansion.groups)
            .map_err(|_| BeaconError::ResourceLimit {
                resource: "fuzzy group coverage",
                limit: expansion.groups,
            })?;
        covered.resize(expansion.groups, false);
        for (&term, groups) in &expansion.terms {
            work.charge(1)?;
            if let Some(postings) = self.postings.get(term) {
                let df = corpus.frequencies[term];
                let idf = ((corpus.documents - df) as f64 + 0.5) / (df as f64 + 0.5);
                streams.push((groups.as_slice(), idf.ln_1p(), postings.iter().peekable()));
            }
        }
        let average = corpus.total_length as f64 / corpus.documents as f64;
        loop {
            work.charge(streams.len().saturating_add(1))?;
            let Some(id) = streams
                .iter_mut()
                .filter_map(|(_, _, stream)| stream.peek().map(|(id, _)| **id))
                .min()
            else {
                break;
            };
            let visible = eligible(id);
            work.charge(covered.len())?;
            covered.fill(false);
            let length = *self
                .lengths
                .get(&id)
                .ok_or(BeaconError::Invariant("fuzzy posting length"))?;
            let norm = 1.0 - config.b + config.b * length as f64 / average;
            let mut score = 0.0;
            for (groups, idf, stream) in &mut streams {
                work.charge(1)?;
                if stream.peek().is_some_and(|(next, _)| **next == id) {
                    let (_, posting) = stream
                        .next()
                        .ok_or(BeaconError::Invariant("fuzzy posting cursor"))?;
                    if visible {
                        score += *idf * saturation(config.k1, norm, posting.len() as f64);
                        for &group in *groups {
                            work.charge(1)?;
                            covered[group] = true;
                        }
                    }
                }
            }
            work.charge(covered.len())?;
            if visible && (!require_all || covered.iter().all(|found| *found)) {
                if !score.is_finite() {
                    return Err(BeaconError::Invariant("non-finite fuzzy BM25 score"));
                }
                work.charge(1 + best.len().checked_ilog2().unwrap_or(0) as usize)?;
                retain_best(
                    best,
                    Ranked {
                        cost: -score,
                        id,
                        slot: 0,
                    },
                    limit,
                );
            }
        }
        work.charge(1)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        BeaconIndex, Bm25, IndexConfig, IndexDocument, IndexMutation, TextMatch, WorkBudget,
    };

    fn mode(distance: EditDistance, require_all: bool, max_expansions: usize) -> TextMatch {
        TextMatch::Fuzzy {
            distance,
            require_all,
            max_expansions,
        }
    }

    fn words() -> Vec<String> {
        let mut all = vec![String::new()];
        let mut layer = all.clone();
        for _ in 0..4 {
            layer = layer
                .into_iter()
                .flat_map(|word| {
                    ['a', 'é', '界']
                        .into_iter()
                        .map(move |ch| format!("{word}{ch}"))
                })
                .collect();
            all.extend(layer.iter().cloned());
        }
        all
    }

    // Independent full matrix with usize cells: no band, prefix pruning,
    // saturation, shared transition, or production matcher.
    fn reference(left: &str, right: &str) -> usize {
        let a: Vec<_> = left.chars().collect();
        let b: Vec<_> = right.chars().collect();
        let mut matrix = vec![vec![0usize; b.len() + 1]; a.len() + 1];
        for (i, row) in matrix.iter_mut().enumerate() {
            row[0] = i;
        }
        for j in 0..=b.len() {
            matrix[0][j] = j;
        }
        for i in 1..=a.len() {
            for j in 1..=b.len() {
                matrix[i][j] = (matrix[i - 1][j] + 1)
                    .min(matrix[i][j - 1] + 1)
                    .min(matrix[i - 1][j - 1] + usize::from(a[i - 1] != b[j - 1]));
            }
        }
        matrix[a.len()][b.len()]
    }

    #[test]
    fn scalar_band_and_dictionary_pruning_match_an_independent_unicode_matrix() {
        let words = words();
        let mut work = WorkBudget::new(usize::MAX);
        let corpus = CorpusStats {
            documents: 1,
            total_length: words.len() as u64,
            frequencies: words
                .iter()
                .filter(|word| !word.is_empty())
                .map(|word| (word.clone(), 1))
                .collect(),
            term_bytes: words.iter().map(String::len).sum(),
        };
        let config = Bm25Config {
            max_query_terms: 1000,
            ..Bm25Config::default()
        };
        for pattern in &words {
            for distance in [EditDistance::Zero, EditDistance::One, EditDistance::Two] {
                let chars: Vec<_> = pattern.chars().collect();
                for candidate in &words {
                    let expected = reference(pattern, candidate) <= distance as usize;
                    assert_eq!(
                        matches(&chars, candidate, distance, &mut work).unwrap() == Match::Yes,
                        expected,
                        "{pattern:?} {candidate:?} {distance:?}"
                    );
                }
                if !pattern.is_empty() {
                    let query = config.query_terms(pattern, &mut work).unwrap();
                    let expanded =
                        Expansion::build(&query, &corpus, distance, words.len(), &mut work)
                            .unwrap();
                    let mut expected: Vec<_> = words
                        .iter()
                        .filter(|word| {
                            !word.is_empty() && reference(pattern, word) <= distance as usize
                        })
                        .map(String::as_str)
                        .collect();
                    expected.sort();
                    assert_eq!(expanded.terms.keys().copied().collect::<Vec<_>>(), expected);
                }
            }
        }
        assert_eq!(
            after_prefix("x\u{d7ff}", &mut work).unwrap().as_deref(),
            Some("x\u{e000}")
        );
        assert_eq!(
            after_prefix("x\u{10ffff}", &mut work).unwrap().as_deref(),
            Some("y")
        );
        assert_eq!(after_prefix("\u{10ffff}", &mut work).unwrap(), None);
    }

    fn corpus() -> Bm25 {
        Bm25::build(
            Bm25Config::default(),
            [
                (VId(1), "cat cot".to_owned()),
                (VId(2), "coat dog".to_owned()),
                (VId(3), "cat dog".to_owned()),
                (VId(4), "cat catalog".to_owned()),
                (VId(5), "é 界".to_owned()),
            ],
            &mut WorkBudget::new(1_000_000),
        )
        .unwrap()
    }

    #[test]
    fn all_covers_input_groups_and_overlapping_expansions_score_each_term_once() {
        let corpus = corpus();
        let mut work = WorkBudget::new(1_000_000);
        let any = mode(EditDistance::One, false, 3);
        let all = mode(EditDistance::One, true, 4);
        let actual = corpus
            .search("cat coat", 10, any, |_| true, &mut work)
            .unwrap();
        let expected = corpus
            .search("cat cot coat", 10, TextMatch::Any, |_| true, &mut work)
            .unwrap();
        assert_eq!(
            actual, expected,
            "overlapping alternatives must not duplicate scores"
        );
        assert_eq!(
            corpus
                .search("cat cat coat", 10, any, |_| true, &mut work)
                .unwrap(),
            actual
        );
        let actual = corpus
            .search("cat dog", 10, all, |_| true, &mut work)
            .unwrap();
        let expected = corpus
            .search(
                "cat cot coat dog",
                10,
                TextMatch::Any,
                |id| id == VId(2) || id == VId(3),
                &mut work,
            )
            .unwrap();
        assert_eq!(actual, expected);
        assert!(
            actual.iter().all(|hit| hit.id != VId(1)),
            "two alternatives are not two groups"
        );
        assert_eq!(
            corpus
                .search("cat dog", 1, all, |_| true, &mut work)
                .unwrap(),
            actual[..1]
        );
        // Each alternative covers both input terms; do not require different
        // document occurrences merely because their expansion sets overlap.
        assert_eq!(
            corpus
                .search(
                    "cat coat",
                    10,
                    mode(EditDistance::One, true, 3),
                    |_| true,
                    &mut work
                )
                .unwrap(),
            corpus
                .search("cat cot coat", 10, TextMatch::Any, |_| true, &mut work)
                .unwrap()
        );
        assert!(
            corpus
                .search("cat absent", 10, all, |_| true, &mut work)
                .unwrap()
                .is_empty()
        );
        assert!(matches!(
            corpus.search(
                "cat coat",
                1,
                mode(EditDistance::One, false, 2),
                |_| true,
                &mut work
            ),
            Err(BeaconError::ResourceLimit {
                resource: "fuzzy expanded terms",
                limit: 2
            })
        ));
    }

    #[test]
    fn zero_distance_preserves_exact_scores_and_unicode_edits_are_not_bytes() {
        let corpus = corpus();
        let mut work = WorkBudget::new(1_000_000);
        for (all, exact) in [(false, TextMatch::Any), (true, TextMatch::All)] {
            for query in ["cat dog", "coat cat", "é 界", "cat cat", "missing", "--"] {
                assert_eq!(
                    corpus
                        .search(
                            query,
                            10,
                            mode(EditDistance::Zero, all, 100),
                            |_| true,
                            &mut work
                        )
                        .unwrap(),
                    corpus
                        .search(query, 10, exact, |_| true, &mut work)
                        .unwrap()
                );
            }
        }
        let hits = corpus
            .search(
                "a",
                10,
                mode(EditDistance::One, false, 2),
                |_| true,
                &mut work,
            )
            .unwrap();
        assert_eq!(hits.iter().map(|hit| hit.id).collect::<Vec<_>>(), [VId(5)]);
        assert_eq!(
            hits,
            corpus
                .search("é 界", 10, TextMatch::Any, |_| true, &mut work)
                .unwrap()
        );
        // Substitution/insertion/deletion yes; no Damerau single-edit swap.
        assert!(
            corpus
                .search(
                    "cta",
                    10,
                    mode(EditDistance::One, false, 10),
                    |_| true,
                    &mut work
                )
                .unwrap()
                .is_empty()
        );
        assert!(
            !corpus
                .search(
                    "cta",
                    10,
                    mode(EditDistance::Two, false, 10),
                    |_| true,
                    &mut work
                )
                .unwrap()
                .is_empty()
        );
        assert!(
            corpus
                .search(
                    "anything",
                    0,
                    mode(EditDistance::Two, false, 0),
                    |_| true,
                    &mut work
                )
                .unwrap()
                .is_empty()
        );
    }

    fn document(id: u128, text: &str) -> IndexDocument {
        IndexDocument {
            id: VId(id),
            vector: None,
            text: Some(text.to_owned()),
        }
    }

    #[test]
    fn live_vocabulary_caps_ignore_retired_terms_and_segment_layout() {
        let mut work = WorkBudget::new(10_000_000);
        let mut index = BeaconIndex::build(
            IndexConfig::default(),
            [document(1, "cat"), document(2, "bat")],
            &mut work,
        )
        .unwrap();
        let old = index.snapshot();
        let bounded = mode(EditDistance::One, false, 1);
        assert!(matches!(
            old.text_search("cat", 1, bounded, |_| true, &mut work),
            Err(BeaconError::ResourceLimit {
                resource: "fuzzy expanded terms",
                limit: 1
            })
        ));
        index
            .apply_batch([IndexMutation::Delete(VId(2))], &mut work)
            .unwrap();
        assert_eq!(
            index
                .snapshot()
                .text_search("cat", 10, bounded, |_| true, &mut work)
                .unwrap()
                .iter()
                .map(|hit| hit.id)
                .collect::<Vec<_>>(),
            [VId(1)]
        );
        index
            .apply_batch([IndexMutation::Upsert(document(3, "hat"))], &mut work)
            .unwrap();
        assert!(matches!(
            index
                .snapshot()
                .text_search("cat", 1, bounded, |_| true, &mut work),
            Err(BeaconError::ResourceLimit {
                resource: "fuzzy expanded terms",
                limit: 1
            })
        ));
        let old_rows = old
            .text_search(
                "cat",
                10,
                mode(EditDistance::One, false, 2),
                |_| true,
                &mut work,
            )
            .unwrap();
        let replacement = [document(1, "cat dog"), document(3, "coat dog")];
        index
            .apply_batch(replacement.clone().map(IndexMutation::Upsert), &mut work)
            .unwrap();
        let rebuilt = BeaconIndex::build(IndexConfig::default(), replacement, &mut work).unwrap();
        for all in [false, true] {
            let query = mode(EditDistance::One, all, 3);
            assert_eq!(
                index
                    .snapshot()
                    .text_search("cat dog", 10, query, |_| true, &mut work)
                    .unwrap(),
                rebuilt
                    .snapshot()
                    .text_search("cat dog", 10, query, |_| true, &mut work)
                    .unwrap()
            );
        }
        assert_eq!(
            old.text_search(
                "cat",
                10,
                mode(EditDistance::One, false, 2),
                |_| true,
                &mut work
            )
            .unwrap(),
            old_rows
        );
    }

    #[test]
    fn every_expansion_and_posting_control_cut_refuses_without_a_partial_answer() {
        #[derive(Default)]
        struct Cut {
            calls: usize,
            stop: Option<usize>,
        }
        impl WorkControl for Cut {
            fn charge(&mut self, _: usize) -> Result<(), BeaconError> {
                self.calls += 1;
                if self.stop == Some(self.calls) {
                    Err(BeaconError::Cancelled)
                } else {
                    Ok(())
                }
            }
        }
        let corpus = corpus();
        let mode = mode(EditDistance::Two, true, 20);
        let mut trace = Cut::default();
        let expected = corpus
            .search("cat dog", 3, mode, |_| true, &mut trace)
            .unwrap();
        assert!(!expected.is_empty());
        for stop in 1..=trace.calls {
            let mut cut = Cut {
                stop: Some(stop),
                ..Cut::default()
            };
            assert!(
                matches!(
                    corpus.search("cat dog", 3, mode, |_| true, &mut cut),
                    Err(BeaconError::Cancelled)
                ),
                "cut {stop}"
            );
            assert_eq!(cut.calls, stop);
            assert_eq!(
                corpus
                    .search("cat dog", 3, mode, |_| true, &mut cut)
                    .unwrap(),
                expected
            );
        }
    }
}
