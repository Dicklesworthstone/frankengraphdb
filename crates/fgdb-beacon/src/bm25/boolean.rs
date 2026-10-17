//! Exact-token Boolean retrieval over one immutable segment generation.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap};
use std::sync::Arc;

use fgdb_types::VId;

use super::{
    AnalyzedText, Bm25, Bm25Config, CorpusStats, TextHit, TextSegment, analyze, saturation,
};
use crate::ranking::{Ranked, retain_best};
use crate::{BeaconError, WorkControl};

/// A bounded Boolean query using the ordinary SimpleUnicodeV1 analyzer.
///
/// Every distinct `required` term must occur, at least `minimum_optional`
/// distinct `optional` terms must occur, and no `excluded` term may occur.
/// When there are no required terms, at least one optional term must match
/// even when the minimum is zero. Empty and purely negative queries match
/// nothing; they never turn into a full-corpus scan.
///
/// Repetition within a clause does not change its meaning. A term present in
/// both positive clauses counts toward the optional minimum but scores only
/// once. Exclusion wins over either positive clause. An impossible minimum
/// yields no results, not a truncated or relaxed query.
///
/// All three clauses share `max_query_bytes` and `max_query_terms`: the latter
/// counts the SUM of their distinct-term counts, including cross-clause repeats.
/// Clauses contain text, not an operator language; quoting, fuzzy expansion and
/// phrases are not interpreted. Query text is redacted from Debug output.
#[derive(Clone, Copy, Default)]
pub struct BooleanTextQuery<'a> {
    pub required: &'a str,
    pub optional: &'a str,
    pub excluded: &'a str,
    pub minimum_optional: usize,
}

impl core::fmt::Debug for BooleanTextQuery<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("BooleanTextQuery")
            .field("required_bytes", &self.required.len())
            .field("optional_bytes", &self.optional.len())
            .field("excluded_bytes", &self.excluded.len())
            .field("minimum_optional", &self.minimum_optional)
            .finish()
    }
}

pub(crate) struct AnalyzedBooleanQuery {
    required: AnalyzedText,
    optional: AnalyzedText,
    excluded: AnalyzedText,
    minimum_optional: usize,
}

impl AnalyzedBooleanQuery {
    pub(crate) fn prepare(
        query: BooleanTextQuery<'_>,
        config: &Bm25Config,
        work: &mut dyn WorkControl,
    ) -> Result<Self, BeaconError> {
        work.charge(1)?;
        let bytes = query
            .required
            .len()
            .checked_add(query.optional.len())
            .and_then(|bytes| bytes.checked_add(query.excluded.len()));
        if bytes.is_none_or(|bytes| bytes > config.max_query_bytes) {
            return Err(BeaconError::ResourceLimit {
                resource: "text bytes",
                limit: config.max_query_bytes,
            });
        }
        let mut remaining = config.max_query_terms;
        let mut clause = |text: &str| -> Result<AnalyzedText, BeaconError> {
            let analyzed = analyze(text, config.max_query_bytes, remaining, work).map_err(
                |error| match error {
                    BeaconError::ResourceLimit {
                        resource: "distinct analyzed terms",
                        ..
                    } => BeaconError::ResourceLimit {
                        resource: "distinct analyzed terms",
                        limit: config.max_query_terms,
                    },
                    other => other,
                },
            )?;
            remaining -= analyzed.frequencies.len();
            Ok(analyzed)
        };
        // Validate every clause before early returns, even for k=0 or a
        // contradictory query. No clause receives a fresh resource allowance.
        let required = clause(query.required)?;
        let optional = clause(query.optional)?;
        let excluded = clause(query.excluded)?;
        let minimum_optional = if required.frequencies.is_empty() {
            query.minimum_optional.max(1)
        } else {
            query.minimum_optional
        };
        Ok(Self {
            required,
            optional,
            excluded,
            minimum_optional,
        })
    }
}

type PostingList = BTreeMap<VId, Arc<Vec<u64>>>;

struct PositivePosting<'a> {
    postings: &'a PostingList,
    idf: f64,
    required: bool,
    optional: bool,
}

// Logical logarithmic charges, not claims about exact B-tree comparisons,
// allocation bytes, physical I/O or wall-clock cost.
fn lookup_work(count: usize) -> usize {
    1 + count.checked_ilog2().unwrap_or(0) as usize
}

impl TextSegment {
    // Inputs mirror the shared per-segment BM25 kernel and its global heap.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn search_boolean_into(
        &self,
        query: &AnalyzedBooleanQuery,
        config: &Bm25Config,
        corpus: &CorpusStats,
        limit: usize,
        eligible: &impl Fn(VId) -> bool,
        best: &mut BinaryHeap<Ranked>,
        work: &mut dyn WorkControl,
    ) -> Result<(), BeaconError> {
        work.charge(1)?;
        if limit == 0
            || corpus.documents == 0
            || corpus.total_length == 0
            || query.minimum_optional > query.optional.frequencies.len()
            || (query.required.frequencies.is_empty() && query.optional.frequencies.is_empty())
        {
            return Ok(());
        }
        let mut terms = BTreeMap::<&str, (bool, bool)>::new();
        for term in query.required.frequencies.keys() {
            work.charge(term.len().saturating_add(1))?;
            if query.excluded.frequencies.contains_key(term) {
                return Ok(());
            }
            work.reserve_scratch(1)?;
            terms.entry(term).or_default().0 = true;
        }
        for term in query.optional.frequencies.keys() {
            work.charge(term.len().saturating_add(1))?;
            if !terms.contains_key(term.as_str()) {
                work.reserve_scratch(1)?;
            }
            terms.entry(term).or_default().1 = true;
        }
        work.charge(terms.len())?;
        work.reserve_scratch(terms.len())?;
        let mut positive: Vec<PositivePosting<'_>> = Vec::new();
        positive
            .try_reserve_exact(terms.len())
            .map_err(|_| BeaconError::ResourceLimit {
                resource: "Boolean positive postings",
                limit: terms.len(),
            })?;
        let mut available_optional = 0;
        let mut anchor: Option<usize> = None;
        for (term, (required, optional)) in terms {
            work.charge(1)?;
            let df = corpus.frequencies.get(term).copied().unwrap_or(0);
            if df > corpus.documents {
                return Err(BeaconError::Invariant("df exceeds live document count"));
            }
            let postings = self
                .postings
                .get(term)
                .filter(|postings| df != 0 && !postings.is_empty());
            let Some(postings) = postings else {
                if required {
                    return Ok(());
                }
                continue;
            };
            if optional {
                available_optional += 1;
            }
            if required && anchor.is_none_or(|i| postings.len() < positive[i].postings.len()) {
                anchor = Some(positive.len());
            }
            let ratio = ((corpus.documents - df) as f64 + 0.5) / (df as f64 + 0.5);
            positive.push(PositivePosting {
                postings,
                idf: ratio.ln_1p(),
                required,
                optional,
            });
        }
        if positive.is_empty() || available_optional < query.minimum_optional {
            return Ok(());
        }
        work.charge(query.excluded.frequencies.len())?;
        work.reserve_scratch(query.excluded.frequencies.len())?;
        let mut excluded: Vec<&PostingList> = Vec::new();
        excluded
            .try_reserve_exact(query.excluded.frequencies.len())
            .map_err(|_| BeaconError::ResourceLimit {
                resource: "Boolean excluded postings",
                limit: query.excluded.frequencies.len(),
            })?;
        for term in query.excluded.frequencies.keys() {
            work.charge(1)?;
            if let Some(postings) = self.postings.get(term) {
                excluded.push(postings);
            }
        }
        let average_length = corpus.total_length as f64 / corpus.documents as f64;
        let consider = |id: VId,
                        best: &mut BinaryHeap<Ranked>,
                        work: &mut dyn WorkControl|
         -> Result<(), BeaconError> {
            work.charge(1)?;
            // In multi-segment hosts this includes generation visibility.
            // Never combine positive or negative facts from different versions.
            if !eligible(id) {
                return Ok(());
            }
            for postings in &excluded {
                work.charge(lookup_work(postings.len()))?;
                if postings.contains_key(&id) {
                    return Ok(());
                }
            }
            work.charge(lookup_work(self.lengths.len()))?;
            let length = *self
                .lengths
                .get(&id)
                .ok_or(BeaconError::Invariant("posting has no document length"))?;
            let length_norm = 1.0 - config.b + config.b * length as f64 / average_length;
            let mut score = 0.0;
            let mut optional_matches = 0;
            // The vector is in lexical term order, not driver order, so a
            // matching document retains ordinary distinct-term BM25 bits.
            for term in &positive {
                work.charge(lookup_work(term.postings.len()))?;
                if let Some(posting) = term.postings.get(&id) {
                    optional_matches += usize::from(term.optional);
                    score += term.idf * saturation(config.k1, length_norm, posting.len() as f64);
                } else if term.required {
                    return Ok(());
                }
            }
            if optional_matches >= query.minimum_optional {
                if !score.is_finite() {
                    return Err(BeaconError::Invariant("non-finite BM25 score"));
                }
                work.charge(lookup_work(limit))?;
                if best.len() < limit {
                    work.reserve_scratch(1)?;
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
            Ok(())
        };
        if let Some(anchor) = anchor {
            // Required queries enumerate only the rarest required posting
            // list, rather than the potentially enormous optional union.
            for &id in positive[anchor].postings.keys() {
                work.charge(1)?;
                consider(id, best, work)?;
            }
        } else {
            // Optional-only queries merge borrowed posting streams. Scratch
            // is O(distinct terms + k), never a set of every matching vertex.
            work.charge(positive.len().saturating_mul(2))?;
            work.reserve_scratch(positive.len().saturating_mul(2))?;
            let mut streams = Vec::new();
            streams
                .try_reserve_exact(positive.len())
                .map_err(|_| BeaconError::ResourceLimit {
                    resource: "Boolean posting streams",
                    limit: positive.len(),
                })?;
            let mut frontier = BinaryHeap::new();
            frontier
                .try_reserve(positive.len())
                .map_err(|_| BeaconError::ResourceLimit {
                    resource: "Boolean posting frontier",
                    limit: positive.len(),
                })?;
            for term in &positive {
                work.charge(lookup_work(positive.len()))?;
                let mut stream = term.postings.keys();
                if let Some(&id) = stream.next() {
                    frontier.push(Reverse((id, streams.len())));
                }
                streams.push(stream);
            }
            let mut previous = None;
            loop {
                work.charge(lookup_work(positive.len()).saturating_mul(2))?;
                let Some(Reverse((id, slot))) = frontier.pop() else {
                    break;
                };
                if previous != Some(id) {
                    consider(id, best, work)?;
                    previous = Some(id);
                }
                if let Some(&next) = streams[slot].next() {
                    frontier.push(Reverse((next, slot)));
                }
            }
        }
        work.charge(1)?;
        Ok(())
    }
}

pub(crate) fn finish_boolean_hits(
    best: BinaryHeap<Ranked>,
    work: &mut dyn WorkControl,
) -> Result<Vec<TextHit>, BeaconError> {
    work.charge(best.len().saturating_mul(lookup_work(best.len())))?;
    work.reserve_scratch(best.len())?;
    Ok(best
        .into_sorted_vec()
        .into_iter()
        .map(|hit| TextHit {
            id: hit.id,
            score: -hit.cost,
        })
        .collect())
}

impl Bm25 {
    /// Retrieve Boolean matches before selecting the top k. Eligibility is a
    /// result restriction, not an ACL or a change to the scoring corpus.
    /// Work exhaustion/cancellation returns an error, never partial success.
    /// Posting bookkeeping, ranking slots and output rows use reserve_scratch;
    /// the analyzer retains its independent shared query-byte/term bounds.
    pub fn search_boolean(
        &self,
        query: BooleanTextQuery<'_>,
        k: usize,
        eligible: impl Fn(VId) -> bool,
        work: &mut dyn WorkControl,
    ) -> Result<Vec<TextHit>, BeaconError> {
        let query = AnalyzedBooleanQuery::prepare(query, &self.config, work)?;
        let mut best = BinaryHeap::new();
        self.segment.search_boolean_into(
            &query,
            &self.config,
            &self.corpus,
            k.min(self.corpus.documents),
            &eligible,
            &mut best,
            work,
        )?;
        finish_boolean_hits(best, work)
    }
}

#[cfg(test)]
#[path = "boolean_tests.rs"]
mod tests;
