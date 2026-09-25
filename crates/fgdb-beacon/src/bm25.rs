use std::collections::{BTreeMap, BinaryHeap};
use std::sync::Arc;

use fgdb_types::VId;

use crate::ranking::{Ranked, retain_best};
use crate::{BeaconError, WorkControl};

#[cfg(test)]
#[path = "bm25/phrase_tests.rs"]
mod phrase_tests;

/// SimpleUnicodeV1: maximal alphanumeric runs, Unicode lowercase, no stemming,
/// stop-word removal, or normalization. Unicode tables follow the pinned Rust
/// toolchain. This analyzer does not reinterpret a scalar's collation binding.
#[derive(Clone, Debug, PartialEq)]
pub struct Bm25Config {
    pub k1: f64,
    pub b: f64,
    pub max_documents: usize,
    pub max_document_bytes: usize,
    pub max_document_terms: usize,
    pub max_query_bytes: usize,
    pub max_query_terms: usize,
}

impl Default for Bm25Config {
    fn default() -> Self {
        Self {
            k1: 1.2,
            b: 0.75,
            max_documents: 1_000_000,
            max_document_bytes: 1_048_576,
            max_document_terms: 100_000,
            max_query_bytes: 16_384,
            max_query_terms: 64,
        }
    }
}

impl Bm25Config {
    pub fn validate(&self) -> Result<(), BeaconError> {
        if !self.k1.is_finite() || self.k1 <= 0.0 {
            return Err(BeaconError::InvalidConfig(
                "BM25 k1 must be finite and positive",
            ));
        }
        if !self.b.is_finite() || !(0.0..=1.0).contains(&self.b) {
            return Err(BeaconError::InvalidConfig("BM25 b must be in 0..=1"));
        }
        if self.max_documents == 0
            || self.max_document_bytes == 0
            || self.max_document_terms == 0
            || self.max_query_bytes == 0
            || self.max_query_terms == 0
        {
            return Err(BeaconError::InvalidConfig(
                "BM25 resource limits must be positive",
            ));
        }
        Ok(())
    }

    pub(crate) fn analyze_document(
        &self,
        text: &str,
        work: &mut dyn WorkControl,
    ) -> Result<AnalyzedText, BeaconError> {
        analyze(text, self.max_document_bytes, self.max_document_terms, work)
    }

    pub(crate) fn query_terms(
        &self,
        query: &str,
        work: &mut dyn WorkControl,
    ) -> Result<AnalyzedText, BeaconError> {
        analyze(query, self.max_query_bytes, self.max_query_terms, work)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextMatch {
    Any,
    All,
    /// An exact, contiguous sequence of SimpleUnicodeV1 tokens. Order and
    /// repeated terms matter; punctuation separates tokens, not phrases.
    /// Empty analyzed queries match nothing. Matching documents retain the
    /// ordinary distinct-term BM25 score: no phrase bonus or new scorer.
    Phrase,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TextHit {
    pub id: VId,
    pub score: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bm25Stats {
    /// Includes documents with an indexed but empty text value.
    pub documents: usize,
    pub total_tokens: u64,
    pub distinct_terms: usize,
}

#[derive(Clone)]
pub(crate) struct AnalyzedText {
    // One positional list per term; its length is the term frequency. Lists
    // are shared by the stored document and all segment/compaction views.
    pub frequencies: BTreeMap<String, Arc<Vec<u64>>>,
    pub length: u64,
    pub source_bytes: usize,
}

fn analyze(
    text: &str,
    byte_limit: usize,
    term_limit: usize,
    work: &mut dyn WorkControl,
) -> Result<AnalyzedText, BeaconError> {
    work.charge(1)?;
    if text.len() > byte_limit {
        return Err(BeaconError::ResourceLimit {
            resource: "text bytes",
            limit: byte_limit,
        });
    }
    let mut result = AnalyzedText {
        frequencies: BTreeMap::new(),
        length: 0,
        source_bytes: text.len(),
    };
    let mut token = String::new();
    for ch in text.chars() {
        work.charge(ch.len_utf8())?;
        if ch.is_alphanumeric() {
            for lower in ch.to_lowercase() {
                work.charge(lower.len_utf8())?;
                token.push(lower);
            }
        } else {
            finish_token(&mut result, &mut token, term_limit, work)?;
        }
    }
    finish_token(&mut result, &mut token, term_limit, work)?;
    Ok(result)
}

fn finish_token(
    result: &mut AnalyzedText,
    token: &mut String,
    limit: usize,
    work: &mut dyn WorkControl,
) -> Result<(), BeaconError> {
    if token.is_empty() {
        return Ok(());
    }
    if !result.frequencies.contains_key(token.as_str()) && result.frequencies.len() == limit {
        return Err(BeaconError::ResourceLimit {
            resource: "distinct analyzed terms",
            limit,
        });
    }
    work.charge(1)?;
    let next_length = result
        .length
        .checked_add(1)
        .ok_or(BeaconError::Invariant("token count overflow"))?;
    let positions = result.frequencies.entry(std::mem::take(token)).or_default();
    // Analysis owns these lists exclusively. Never copy a previously shared
    // posting behind a caller's work meter.
    let positions =
        Arc::get_mut(positions).ok_or(BeaconError::Invariant("shared posting during analysis"))?;
    if positions.len() == u32::MAX as usize {
        return Err(BeaconError::ResourceLimit {
            resource: "term frequency",
            limit: u32::MAX as usize,
        });
    }
    // There cannot be more tokens than input bytes. Existing per-document,
    // query and live/staged text-byte limits therefore also bound the number
    // of retained u64 positions, including repeated terms. They are NOT RSS
    // limits: positional storage adds up to one u64 per input token plus
    // vector capacity/metadata, and retained old generations remain live.
    positions
        .try_reserve(1)
        .map_err(|_| BeaconError::ResourceLimit {
            resource: "token position allocation",
            limit: result.source_bytes,
        })?;
    positions.push(result.length);
    result.length = next_length;
    Ok(())
}

/// These are LIVE-corpus statistics, not per-segment statistics. A segment
/// cannot compute a comparable BM25 score using its own local document count.
#[derive(Clone, Default)]
pub(crate) struct CorpusStats {
    pub documents: usize,
    pub total_length: u64,
    frequencies: BTreeMap<String, usize>,
    term_bytes: usize,
}

impl CorpusStats {
    pub fn add(
        &mut self,
        text: &AnalyzedText,
        work: &mut dyn WorkControl,
    ) -> Result<(), BeaconError> {
        work.charge(text.frequencies.len())?;
        self.documents = self
            .documents
            .checked_add(1)
            .ok_or(BeaconError::Invariant("corpus count overflow"))?;
        self.total_length = self
            .total_length
            .checked_add(text.length)
            .ok_or(BeaconError::Invariant("corpus length overflow"))?;
        for term in text.frequencies.keys() {
            work.charge(term.len())?;
            if !self.frequencies.contains_key(term) {
                self.term_bytes = self
                    .term_bytes
                    .checked_add(term.len())
                    .ok_or(BeaconError::Invariant("vocabulary bytes overflow"))?;
            }
            let frequency = self.frequencies.entry(term.clone()).or_default();
            *frequency = frequency
                .checked_add(1)
                .ok_or(BeaconError::Invariant("document frequency overflow"))?;
        }
        Ok(())
    }

    pub fn remove(
        &mut self,
        text: &AnalyzedText,
        work: &mut dyn WorkControl,
    ) -> Result<(), BeaconError> {
        work.charge(text.frequencies.len())?;
        self.documents = self
            .documents
            .checked_sub(1)
            .ok_or(BeaconError::Invariant("corpus count underflow"))?;
        self.total_length = self
            .total_length
            .checked_sub(text.length)
            .ok_or(BeaconError::Invariant("corpus length underflow"))?;
        for term in text.frequencies.keys() {
            work.charge(term.len())?;
            let frequency = self
                .frequencies
                .get_mut(term)
                .ok_or(BeaconError::Invariant("missing live term"))?;
            *frequency = frequency
                .checked_sub(1)
                .ok_or(BeaconError::Invariant("document frequency underflow"))?;
            if *frequency == 0 {
                self.frequencies.remove(term);
                self.term_bytes -= term.len();
            }
        }
        Ok(())
    }

    pub fn clone_with_work(&self, work: &mut dyn WorkControl) -> Result<Self, BeaconError> {
        let mut frequencies = BTreeMap::new();
        for (term, &frequency) in &self.frequencies {
            work.charge(1)?;
            work.charge(term.len())?;
            frequencies.insert(term.clone(), frequency);
        }
        Ok(Self {
            documents: self.documents,
            total_length: self.total_length,
            frequencies,
            term_bytes: self.term_bytes,
        })
    }

    pub fn stats(&self) -> Bm25Stats {
        Bm25Stats {
            documents: self.documents,
            total_tokens: self.total_length,
            distinct_terms: self.frequencies.len(),
        }
    }
}

#[derive(Clone, Default)]
pub(crate) struct TextSegment {
    postings: BTreeMap<String, BTreeMap<VId, Arc<Vec<u64>>>>,
    lengths: BTreeMap<VId, u64>,
}

impl TextSegment {
    pub fn build<'a>(
        documents: impl IntoIterator<Item = (VId, &'a AnalyzedText)>,
        work: &mut dyn WorkControl,
    ) -> Result<Self, BeaconError> {
        let mut segment = Self::default();
        for (id, text) in documents {
            work.charge(1)?;
            if segment.lengths.insert(id, text.length).is_some() {
                return Err(BeaconError::DuplicateVertex(id));
            }
            for (term, positions) in &text.frequencies {
                work.charge(term.len().saturating_add(1))?;
                segment
                    .postings
                    .entry(term.clone())
                    .or_default()
                    .insert(id, Arc::clone(positions));
            }
        }
        Ok(segment)
    }

    /// Document-at-a-time merge: auxiliary merge memory is O(unique terms + k),
    /// not O(matching documents); the analyzed query also retains its bounded
    /// token offsets. Only eligible postings contribute to the bounded heap,
    /// and each document's terms are accumulated in lexical term order.
    /// Phrase admission precedes top-k, so non-phrase hits cannot hide a lower
    /// scoring phrase. Position checks happen only for the selected generation.
    pub fn search_into(
        &self,
        query: &AnalyzedText,
        mode: TextMatch,
        config: &Bm25Config,
        corpus: &CorpusStats,
        limit: usize,
        eligible: &impl Fn(VId) -> bool,
        best: &mut BinaryHeap<Ranked>,
        work: &mut dyn WorkControl,
    ) -> Result<(), BeaconError> {
        let terms = &query.frequencies;
        if limit == 0 || terms.is_empty() || corpus.documents == 0 || corpus.total_length == 0 {
            return Ok(());
        }
        let mut streams = Vec::new();
        for term in terms.keys() {
            work.charge(1)?;
            let df = corpus.frequencies.get(term).copied().unwrap_or(0);
            if df > corpus.documents {
                return Err(BeaconError::Invariant("df exceeds live document count"));
            }
            match (self.postings.get(term), df) {
                (Some(postings), df) if df != 0 => {
                    let idf = ((corpus.documents - df) as f64 + 0.5) / (df as f64 + 0.5);
                    streams.push((idf.ln_1p(), postings.iter().peekable()));
                }
                _ if mode != TextMatch::Any => return Ok(()),
                _ => {}
            }
        }
        let average_length = corpus.total_length as f64 / corpus.documents as f64;
        let mut positions = Vec::new();
        if mode == TextMatch::Phrase {
            work.charge(terms.len())?;
            positions
                .try_reserve_exact(terms.len())
                .map_err(|_| BeaconError::ResourceLimit {
                    resource: "phrase posting references",
                    limit: terms.len(),
                })?;
        }
        loop {
            work.charge(streams.len().saturating_add(1))?;
            let Some(id) = streams
                .iter_mut()
                .filter_map(|(_, stream)| stream.peek().map(|(id, _)| **id))
                .min()
            else {
                break;
            };
            let visible = eligible(id);
            let length = *self
                .lengths
                .get(&id)
                .ok_or(BeaconError::Invariant("posting has no document length"))?;
            let length_norm = 1.0 - config.b + config.b * length as f64 / average_length;
            let mut matches = 0;
            let mut score = 0.0;
            positions.clear();
            for (idf, stream) in &mut streams {
                if stream.peek().is_some_and(|(next, _)| **next == id) {
                    let (_, posting) = stream
                        .next()
                        .ok_or(BeaconError::Invariant("posting cursor disappeared"))?;
                    matches += 1;
                    if visible {
                        // Analysis limits each frequency to u32::MAX, exactly
                        // representable in f64 as in the count-only kernel.
                        score += *idf * saturation(config.k1, length_norm, posting.len() as f64);
                        if mode == TextMatch::Phrase {
                            positions.push(posting.as_slice());
                        }
                    }
                }
            }
            if visible && (mode == TextMatch::Any || matches == terms.len()) {
                if mode == TextMatch::Phrase && !phrase_matches(query, &positions, length, work)? {
                    continue;
                }
                if !score.is_finite() {
                    return Err(BeaconError::Invariant("non-finite BM25 score"));
                }
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
        Ok(())
    }
}

/// Anchor on the rarest term in THIS document, not necessarily the first
/// phrase term. Every actual occurrence of that anchor supplies one possible
/// phrase start; all query offsets (including repetitions) must then exist.
/// This uses no candidate-start set and never rescans/tokenizes source text.
fn phrase_matches(
    query: &AnalyzedText,
    postings: &[&[u64]],
    document_length: u64,
    work: &mut dyn WorkControl,
) -> Result<bool, BeaconError> {
    work.charge(1)?;
    if query.length == 0 || query.length > document_length {
        return Ok(false);
    }
    if postings.len() != query.frequencies.len() {
        return Err(BeaconError::Invariant("phrase posting arity"));
    }
    let mut anchor = None;
    for (query_positions, &document_positions) in query.frequencies.values().zip(postings) {
        work.charge(1)?;
        let Some(&offset) = query_positions.first() else {
            return Err(BeaconError::Invariant("empty query posting"));
        };
        if anchor.is_none_or(|(_, best): (u64, &[u64])| document_positions.len() < best.len()) {
            anchor = Some((offset, document_positions));
        }
    }
    let Some((offset, candidates)) = anchor else {
        return Ok(false);
    };
    'candidate: for &position in candidates {
        work.charge(1)?;
        let Some(start) = position.checked_sub(offset) else {
            continue;
        };
        if start > document_length - query.length {
            continue;
        }
        for (query_positions, &document_positions) in query.frequencies.values().zip(postings) {
            for &offset in query_positions.iter() {
                // The prior length check and analyzer offsets prove addition
                // cannot overflow. Each binary-search comparison has a gate.
                let wanted = start + offset;
                let (mut low, mut high) = (0, document_positions.len());
                while low < high {
                    work.charge(1)?;
                    let middle = low + (high - low) / 2;
                    if document_positions[middle] < wanted {
                        low = middle + 1;
                    } else {
                        high = middle;
                    }
                }
                work.charge(1)?;
                if document_positions.get(low) != Some(&wanted) {
                    continue 'candidate;
                }
            }
        }
        return Ok(true);
    }
    Ok(false)
}

fn saturation(k1: f64, length_norm: f64, tf: f64) -> f64 {
    // Algebraically tf*(k1+1)/(tf+k1*length_norm), arranged to avoid
    // multiplying a large k1 by tf or length_norm. All finite k1 > 0 work.
    if k1 >= 1.0 {
        (1.0 + 1.0 / k1) / (1.0 / k1 + length_norm / tf)
    } else {
        (1.0 + k1) / (1.0 + (k1 / tf) * length_norm)
    }
}

/// A complete immutable BM25 corpus; the index fabric reuses these same
/// postings kernels against the LIVE statistics of its multi-segment view.
#[derive(Clone)]
pub struct Bm25 {
    config: Bm25Config,
    segment: TextSegment,
    corpus: CorpusStats,
}

impl core::fmt::Debug for Bm25 {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Bm25")
            .field("config", &self.config)
            .field("stats", &self.stats())
            .finish_non_exhaustive()
    }
}

impl Bm25 {
    pub fn build(
        config: Bm25Config,
        documents: impl IntoIterator<Item = (VId, String)>,
        work: &mut dyn WorkControl,
    ) -> Result<Self, BeaconError> {
        config.validate()?;
        let mut ordered = BTreeMap::new();
        let mut corpus = CorpusStats::default();
        for (id, text) in documents {
            let text = config.analyze_document(&text, work)?;
            if ordered.contains_key(&id) {
                return Err(BeaconError::DuplicateVertex(id));
            }
            if ordered.len() == config.max_documents {
                return Err(BeaconError::ResourceLimit {
                    resource: "BM25 documents",
                    limit: config.max_documents,
                });
            }
            corpus.add(&text, work)?;
            ordered.insert(id, text);
        }
        let segment = TextSegment::build(ordered.iter().map(|(&id, text)| (id, text)), work)?;
        Ok(Self {
            config,
            segment,
            corpus,
        })
    }

    #[must_use]
    pub fn stats(&self) -> Bm25Stats {
        self.corpus.stats()
    }

    #[must_use]
    pub fn document_frequency(&self, term: &str) -> usize {
        self.corpus.frequencies.get(term).copied().unwrap_or(0)
    }

    pub fn search(
        &self,
        query: &str,
        k: usize,
        mode: TextMatch,
        eligible: impl Fn(VId) -> bool,
        work: &mut dyn WorkControl,
    ) -> Result<Vec<TextHit>, BeaconError> {
        let terms = self.config.query_terms(query, work)?;
        let mut best = BinaryHeap::new();
        self.segment.search_into(
            &terms,
            mode,
            &self.config,
            &self.corpus,
            k.min(self.corpus.documents),
            &eligible,
            &mut best,
            work,
        )?;
        Ok(best
            .into_sorted_vec()
            .into_iter()
            .map(|hit| TextHit {
                id: hit.id,
                score: -hit.cost,
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::WorkBudget;

    #[test]
    fn saturation_is_stable_across_extreme_finite_k1() {
        for k1 in [f64::MIN_POSITIVE, 0.1, 1.2, 10.0, f64::MAX] {
            let result = saturation(k1, 1.0, 3.0);
            assert!(result.is_finite());
            assert!((1.0..=3.0).contains(&result));
        }
    }

    #[test]
    fn removing_last_occurrence_removes_vocabulary_entry() {
        let config = Bm25Config::default();
        let mut work = WorkBudget::new(1000);
        let text = config.analyze_document("one one two", &mut work).unwrap();
        let mut corpus = CorpusStats::default();
        corpus.add(&text, &mut work).unwrap();
        corpus.remove(&text, &mut work).unwrap();
        assert_eq!(corpus.documents, 0);
        assert_eq!(corpus.total_length, 0);
        assert!(corpus.frequencies.is_empty());
        assert_eq!(corpus.term_bytes, 0);
    }
}
