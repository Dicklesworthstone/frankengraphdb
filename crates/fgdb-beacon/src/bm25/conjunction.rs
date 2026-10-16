//! Seek the intersection of required posting lists, not their entire union.
//! Cursors borrow the immutable positional index; no candidate set is built.

use super::{
    AnalyzedText, Bm25Config, CorpusStats, TextMatch, TextSegment, phrase_matches, saturation,
};
use crate::ranking::{Ranked, retain_best};
use crate::{BeaconError, WorkControl};
use fgdb_types::VId;
use std::collections::{BTreeMap, BinaryHeap, btree_map};
use std::sync::Arc;

type PostingList = BTreeMap<VId, Arc<Vec<u64>>>;

#[cfg(test)]
thread_local! {
    // Physical cursor operations, separate from the logical work allowance.
    static NEXTS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static SEEKS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

struct PostingCursor<'a> {
    postings: &'a PostingList,
    remaining: btree_map::Range<'a, VId, Arc<Vec<u64>>>,
    head: Option<(VId, &'a [u64])>,
    idf: f64,
}

impl<'a> PostingCursor<'a> {
    fn new(
        postings: &'a PostingList,
        idf: f64,
        work: &mut dyn WorkControl,
    ) -> Result<Self, BeaconError> {
        work.charge(1)?;
        let mut cursor = Self {
            postings,
            remaining: postings.range(..),
            head: None,
            idf,
        };
        cursor.advance(work)?;
        Ok(cursor)
    }

    fn advance(&mut self, work: &mut dyn WorkControl) -> Result<(), BeaconError> {
        work.charge(1)?;
        #[cfg(test)]
        NEXTS.with(|count| count.set(count.get() + 1));
        self.head = self
            .remaining
            .next()
            .map(|(id, positions)| (*id, positions.as_slice()));
        Ok(())
    }

    fn seek(&mut self, target: VId, work: &mut dyn WorkControl) -> Result<(), BeaconError> {
        work.charge(1)?;
        if self.head.is_none_or(|(id, _)| id >= target) {
            return Ok(());
        }
        // Dense intersections advance in O(1) iterator steps. Only a genuine
        // gap needs a tree seek; never restart at the beginning of a list.
        self.advance(work)?;
        if self.head.is_some_and(|(id, _)| id < target) {
            // A deterministic logarithmic logical seek charge, not a claim
            // about allocator bytes, exact tree comparisons or wall time.
            work.charge(1 + self.postings.len().checked_ilog2().unwrap_or(0) as usize)?;
            #[cfg(test)]
            SEEKS.with(|count| count.set(count.get() + 1));
            self.remaining = self.postings.range(target..);
            self.advance(work)?;
        }
        Ok(())
    }
}

impl TextSegment {
    /// All required terms must occur in the SAME document generation. A rare
    /// list drives ascending candidates and other lists seek forward to it.
    /// If another head passes the target, repeat at that larger identity.
    /// Every successful iteration has all heads equal; an exhausted list
    /// proves that no later conjunction exists. No VId increment is needed.
    ///
    /// Scoring still accumulates distinct terms in lexical order, with global
    /// live-corpus statistics. Eligibility and exact phrase admission precede
    /// the shared top-k heap. Scratch is O(distinct query terms + k), excluding
    /// the already-resident postings and bounded analyzed-query positions.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn search_conjunction_into(
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
        debug_assert!(matches!(mode, TextMatch::All | TextMatch::Phrase));
        if limit == 0
            || query.frequencies.is_empty()
            || corpus.documents == 0
            || corpus.total_length == 0
        {
            return Ok(());
        }
        work.charge(query.frequencies.len())?;
        let mut cursors: Vec<PostingCursor<'_>> = Vec::new();
        cursors
            .try_reserve_exact(query.frequencies.len())
            .map_err(|_| BeaconError::ResourceLimit {
                resource: "conjunctive posting cursors",
                limit: query.frequencies.len(),
            })?;
        let mut anchor = 0;
        for term in query.frequencies.keys() {
            work.charge(1)?;
            let df = corpus.frequencies.get(term).copied().unwrap_or(0);
            if df > corpus.documents {
                return Err(BeaconError::Invariant("df exceeds live document count"));
            }
            let Some(postings) = self
                .postings
                .get(term)
                .filter(|postings| !postings.is_empty())
            else {
                return Ok(());
            };
            if df == 0 {
                return Ok(());
            }
            let ratio = ((corpus.documents - df) as f64 + 0.5) / (df as f64 + 0.5);
            let cursor = PostingCursor::new(postings, ratio.ln_1p(), work)?;
            // Keep the vector in LEXICAL term order, regardless of which list
            // drives candidate discovery. This preserves every score's bits.
            if cursors.is_empty() || postings.len() < cursors[anchor].postings.len() {
                anchor = cursors.len();
            }
            cursors.push(cursor);
        }
        let average_length = corpus.total_length as f64 / corpus.documents as f64;
        let mut positions = Vec::new();
        if mode == TextMatch::Phrase {
            work.charge(cursors.len())?;
            positions
                .try_reserve_exact(cursors.len())
                .map_err(|_| BeaconError::ResourceLimit {
                    resource: "phrase posting references",
                    limit: cursors.len(),
                })?;
        }
        while let Some((mut target, _)) = cursors[anchor].head {
            loop {
                let previous = target;
                for cursor in &mut cursors {
                    cursor.seek(target, work)?;
                    let Some((id, _)) = cursor.head else {
                        work.charge(1)?;
                        return Ok(());
                    };
                    target = target.max(id);
                }
                if previous == target {
                    break;
                }
            }
            work.charge(1)?;
            if eligible(target) {
                let length = *self
                    .lengths
                    .get(&target)
                    .ok_or(BeaconError::Invariant("posting has no document length"))?;
                let length_norm = 1.0 - config.b + config.b * length as f64 / average_length;
                let mut score = 0.0;
                positions.clear();
                for cursor in &cursors {
                    work.charge(1)?;
                    let (id, posting) = cursor.head.ok_or(BeaconError::Invariant(
                        "conjunctive posting cursor disappeared",
                    ))?;
                    debug_assert_eq!(id, target);
                    score += cursor.idf * saturation(config.k1, length_norm, posting.len() as f64);
                    if mode == TextMatch::Phrase {
                        positions.push(posting);
                    }
                }
                if mode != TextMatch::Phrase || phrase_matches(query, &positions, length, work)? {
                    if !score.is_finite() {
                        return Err(BeaconError::Invariant("non-finite BM25 score"));
                    }
                    retain_best(
                        best,
                        Ranked {
                            cost: -score,
                            id: target,
                            slot: 0,
                        },
                        limit,
                    );
                }
            }
            cursors[anchor].advance(work)?;
        }
        work.charge(1)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    include!("conjunction_tests.rs");
}
