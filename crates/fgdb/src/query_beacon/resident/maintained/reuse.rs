//! Definition-based reuse is confined to privileged, exact-cut database reads.
//! Query-only pins may belong to a retained read view's older frontier.
//! Authorized readers must build their masked corpus: an endpoint predicate
//! cannot remove hidden documents from BM25 statistics or ANN routing.

use super::{Options, QueryIndexes};
use fgdb_beacon::read::Search;
use fgdb_beacon::{BeaconError, IndexConfig, IndexSnapshot, VectorSearch, WorkControl};
use fgdb_types::CommitSeq;

impl QueryIndexes {
    /// Select the first healthy, compatible current generation in bounded slot
    /// order. A miss precedes execution and chooses the ordinary native build;
    /// a refused lookup/search is never retried with a fresh work allowance.
    ///
    /// The Database or read view owns both these immutable pins and the
    /// supplied source frontier. A historical selector must still match that
    /// pinned frontier; staged overlays never receive these pins.
    /// Incremental ANN topology is not equivalent to a rebuild,
    /// even with equal HNSW settings, so approximate requests always miss.
    pub(crate) fn lookup_exact(
        &self,
        at: CommitSeq,
        current: CommitSeq,
        options: &Options,
        config: &IndexConfig,
        query: Search<'_>,
        work: &mut impl WorkControl,
    ) -> Result<Option<IndexSnapshot>, BeaconError> {
        if at != current || !exact_request(query) {
            return Ok(None);
        }
        for entry in self.entries.iter() {
            work.charge(1)?;
            if entry.source_sequence != at {
                continue;
            }
            let definition = &entry.definition;
            if definition.vertex_label != options.vertex_label
                || (config.text.is_some() && definition.projection.text != options.projection.text)
            {
                continue;
            }

            // Only requested lanes influence this search. The full live text
            // corpus remains unchanged, even when an endpoint is ineligible.
            let mut stored = definition.index.clone();
            let mut requested = config.clone();
            if requested.text.is_none() {
                stored.text = None;
            }
            if requested.vector.is_none() {
                stored.vector = None;
            }
            // Exact HNSW search reads validated coordinates, never links.
            // Normalize this one construction-only difference; metric, seed,
            // dimensions, all other settings and every index bound must match.
            if let Some(vector) = &mut stored.vector {
                vector.links = false;
            }
            if let Some(vector) = &mut requested.vector {
                vector.links = false;
            }
            if stored != requested {
                continue;
            }
            if config.vector.is_some() {
                if definition.projection.encoding != options.projection.encoding
                    || definition.projection.vector.len() != options.projection.vector.len()
                {
                    continue;
                }
                let mut equal = true;
                for (stored, requested) in definition
                    .projection
                    .vector
                    .iter()
                    .zip(&options.projection.vector)
                {
                    work.charge(1)?;
                    if stored != requested {
                        equal = false;
                        break;
                    }
                }
                if !equal {
                    continue;
                }
            }
            return Ok(Some(entry.index.clone()));
        }
        Ok(None)
    }
}

fn exact_request(query: Search<'_>) -> bool {
    match query {
        Search::Text { .. } => true,
        Search::Vector { mode, .. } => mode == VectorSearch::Exact,
        Search::Hybrid(query) => {
            query.profile.vector_weight() == 0 || query.vector_mode == VectorSearch::Exact
        }
    }
}
