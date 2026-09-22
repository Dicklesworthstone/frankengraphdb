use std::collections::{BTreeMap, BinaryHeap};
use std::sync::Arc;

use fgdb_types::VId;

use crate::bm25::{AnalyzedText, CorpusStats, TextSegment};
use crate::ranking::{Ranked, retain_best};
use crate::{
    BeaconError, Bm25Config, Bm25Stats, Hnsw, HnswConfig, Neighbor, TextHit, TextMatch,
    VectorSearch, WorkControl,
};

#[derive(Clone, Debug, PartialEq)]
pub struct IndexConfig {
    pub vector: Option<HnswConfig>,
    pub text: Option<Bm25Config>,
    pub max_documents: usize,
    pub max_batch_operations: usize,
    /// Fixed deterministic merge policy, not a learned/adaptive controller.
    pub max_segments: usize,
    pub max_vector_values: usize,
    pub max_text_bytes: usize,
    pub max_document_terms: usize,
}

impl Default for IndexConfig {
    fn default() -> Self {
        Self {
            vector: None,
            text: Some(Bm25Config::default()),
            max_documents: 1_000_000,
            max_batch_operations: 100_000,
            max_segments: 8,
            max_vector_values: 64_000_000,
            max_text_bytes: 64 * 1024 * 1024,
            max_document_terms: 2_000_000,
        }
    }
}

impl IndexConfig {
    pub fn validate(&self) -> Result<(), BeaconError> {
        if self.vector.is_none() && self.text.is_none() {
            return Err(BeaconError::InvalidConfig("at least one index lane is required"));
        }
        if let Some(vector) = &self.vector {
            vector.validate()?;
        }
        if let Some(text) = &self.text {
            text.validate()?;
        }
        if self.max_documents == 0 || self.max_batch_operations == 0 || self.max_segments == 0
            || self.max_vector_values == 0 || self.max_text_bytes == 0 || self.max_document_terms == 0
        {
            return Err(BeaconError::InvalidConfig("index limits must be positive"));
        }
        Ok(())
    }
}

/// A projection of one already-visible vertex, never a new durable graph row.
#[derive(Clone)]
pub struct IndexDocument {
    pub id: VId,
    pub vector: Option<Vec<f32>>,
    pub text: Option<String>,
}

impl core::fmt::Debug for IndexDocument {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("IndexDocument")
            .field("id", &self.id)
            .field("dimensions", &self.vector.as_ref().map(Vec::len))
            .field("text_bytes", &self.text.as_ref().map(String::len))
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug)]
pub enum IndexMutation {
    Upsert(IndexDocument),
    Delete(VId),
}

#[derive(Clone)]
struct StoredDocument {
    vector: Option<Arc<[f32]>>,
    text: Option<AnalyzedText>,
}

impl StoredDocument {
    fn prepare(
        document: IndexDocument,
        config: &IndexConfig,
        work: &mut dyn WorkControl,
    ) -> Result<Self, BeaconError> {
        // Bound a single input before retaining analyzed terms or allocating
        // an Arc. Aggregate staging limits are enforced by apply_batch too.
        for (resource, actual, limit) in [
            ("staged vector values", document.vector.as_ref().map_or(0, Vec::len), config.max_vector_values),
            ("staged text bytes", document.text.as_ref().map_or(0, String::len), config.max_text_bytes),
        ] {
            if actual > limit {
                return Err(BeaconError::ResourceLimit { resource, limit });
            }
        }
        let vector = match document.vector {
            Some(vector) => {
                let vector_config = config.vector.as_ref().ok_or(BeaconError::Disabled("vector"))?;
                vector_config.validate_vector(&vector, work)?;
                Some(Arc::from(vector))
            }
            None => None,
        };
        let text = match document.text {
            Some(text) => Some(config.text.as_ref().ok_or(BeaconError::Disabled("text"))?
                .analyze_document(&text, work)?),
            None => None,
        };
        Ok(Self { vector, text })
    }
}

#[derive(Clone)]
struct LiveDocument {
    generation: u64,
    document: Arc<StoredDocument>,
}

#[derive(Default)]
struct StagedSize {
    vector_values: usize,
    text_bytes: usize,
    document_terms: usize,
}

impl StagedSize {
    fn update(&mut self, document: &StoredDocument, insert: bool) -> Result<(), BeaconError> {
        let values = document.vector.as_ref().map_or(0, |vector| vector.len());
        let bytes = document.text.as_ref().map_or(0, |text| text.source_bytes);
        let terms = document.text.as_ref().map_or(0, |text| text.frequencies.len());
        for (total, delta) in [
            (&mut self.vector_values, values),
            (&mut self.text_bytes, bytes),
            (&mut self.document_terms, terms),
        ] {
            let next = if insert { total.checked_add(delta) } else { total.checked_sub(delta) };
            *total = next.ok_or(BeaconError::Invariant("staged resource arithmetic"))?;
        }
        Ok(())
    }

    fn check(&self, config: &IndexConfig) -> Result<(), BeaconError> {
        for (resource, actual, limit) in [
            ("staged vector values", self.vector_values, config.max_vector_values),
            ("staged text bytes", self.text_bytes, config.max_text_bytes),
            ("staged document terms", self.document_terms, config.max_document_terms),
        ] {
            if actual > limit {
                return Err(BeaconError::ResourceLimit { resource, limit });
            }
        }
        Ok(())
    }
}

struct Segment {
    generation: u64,
    vector: Option<Hnsw>,
    text: TextSegment,
}

impl Segment {
    fn build(
        generation: u64,
        documents: &BTreeMap<VId, Arc<StoredDocument>>,
        config: &IndexConfig,
        work: &mut dyn WorkControl,
    ) -> Result<Self, BeaconError> {
        work.charge(documents.len())?;
        let vector = config.vector.as_ref().map(|vector_config| {
            Hnsw::build_shared(vector_config.clone(), documents.iter().filter_map(|(&id, document)| {
                document.vector.as_ref().map(|vector| (id, Arc::clone(vector)))
            }), work)
        }).transpose()?;
        let text = TextSegment::build(documents.iter().filter_map(|(&id, document)| {
            document.text.as_ref().map(|text| (id, text))
        }), work)?;
        Ok(Self { generation, vector, text })
    }
}

struct Generation {
    config: Arc<IndexConfig>,
    sequence: u64,
    live: BTreeMap<VId, LiveDocument>,
    segments: Vec<Arc<Segment>>,
    corpus: CorpusStats,
    vector_documents: usize,
    vector_values: usize,
    text_bytes: usize,
    document_terms: usize,
}

impl Generation {
    fn successor(&self, work: &mut dyn WorkControl) -> Result<Self, BeaconError> {
        work.charge(self.segments.len())?;
        let mut live = BTreeMap::new();
        for (&id, document) in &self.live {
            work.charge(1)?;
            live.insert(id, document.clone());
        }
        let corpus = self.corpus.clone_with_work(work)?;
        Ok(Self {
            config: Arc::clone(&self.config),
            sequence: self.sequence.checked_add(1).ok_or(BeaconError::GenerationExhausted)?,
            live,
            segments: self.segments.clone(),
            corpus,
            vector_documents: self.vector_documents,
            vector_values: self.vector_values,
            text_bytes: self.text_bytes,
            document_terms: self.document_terms,
        })
    }

    fn remove_stats(&mut self, document: &StoredDocument, work: &mut dyn WorkControl) -> Result<(), BeaconError> {
        if let Some(vector) = &document.vector {
            self.vector_documents = self.vector_documents.checked_sub(1).ok_or(BeaconError::Invariant("vector count underflow"))?;
            self.vector_values = self.vector_values.checked_sub(vector.len()).ok_or(BeaconError::Invariant("vector values underflow"))?;
        }
        if let Some(text) = &document.text {
            self.corpus.remove(text, work)?;
            self.text_bytes = self.text_bytes.checked_sub(text.source_bytes).ok_or(BeaconError::Invariant("text bytes underflow"))?;
            self.document_terms = self.document_terms.checked_sub(text.frequencies.len()).ok_or(BeaconError::Invariant("document terms underflow"))?;
        }
        Ok(())
    }

    fn add_stats(&mut self, document: &StoredDocument, work: &mut dyn WorkControl) -> Result<(), BeaconError> {
        if let Some(vector) = &document.vector {
            self.vector_documents = self.vector_documents.checked_add(1).ok_or(BeaconError::Invariant("vector count overflow"))?;
            self.vector_values = self.vector_values.checked_add(vector.len()).ok_or(BeaconError::Invariant("vector values overflow"))?;
        }
        if let Some(text) = &document.text {
            self.corpus.add(text, work)?;
            self.text_bytes = self.text_bytes.checked_add(text.source_bytes).ok_or(BeaconError::Invariant("text bytes overflow"))?;
            self.document_terms = self.document_terms.checked_add(text.frequencies.len()).ok_or(BeaconError::Invariant("document terms overflow"))?;
        }
        Ok(())
    }

    fn check_limits(&self) -> Result<(), BeaconError> {
        for (resource, actual, limit) in [
            ("live documents", self.live.len(), self.config.max_documents),
            ("live vector values", self.vector_values, self.config.max_vector_values),
            ("live text bytes", self.text_bytes, self.config.max_text_bytes),
            ("live document terms", self.document_terms, self.config.max_document_terms),
        ] {
            if actual > limit {
                return Err(BeaconError::ResourceLimit { resource, limit });
            }
        }
        if let Some(vector) = &self.config.vector {
            if self.vector_documents > vector.max_vectors {
                return Err(BeaconError::ResourceLimit { resource: "live vector documents", limit: vector.max_vectors });
            }
        }
        if let Some(text) = &self.config.text {
            if self.corpus.documents > text.max_documents {
                return Err(BeaconError::ResourceLimit { resource: "live text documents", limit: text.max_documents });
            }
        }
        Ok(())
    }

    fn compact(&mut self, work: &mut dyn WorkControl) -> Result<(), BeaconError> {
        if self.live.is_empty() {
            self.segments.clear();
            return Ok(());
        }
        let mut documents = BTreeMap::new();
        for (&id, live) in &self.live {
            work.charge(1)?;
            documents.insert(id, Arc::clone(&live.document));
        }
        let segment = Arc::new(Segment::build(self.sequence, &documents, &self.config, work)?);
        for live in self.live.values_mut() {
            work.charge(1)?;
            live.generation = self.sequence;
        }
        self.segments = vec![segment];
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IndexStats {
    pub documents: usize,
    pub vector_documents: usize,
    pub vector_values: usize,
    pub text: Bm25Stats,
    pub text_bytes: usize,
    pub segments: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ApplyReport {
    pub operations: usize,
    pub distinct_vertices: usize,
    pub compacted: bool,
    pub segments: usize,
}

/// A writer for DERIVED state only. `apply_batch` does not commit graph writes:
/// the surface must first obtain a Chronicle-authenticated graph publication.
/// There is no public timestamp, marker token, or durability-success constructor.
///
/// The successor is assembled off-side, including both index lanes and live
/// BM25 statistics, then published with one Arc replacement. Errors leave the
/// entire previous generation intact. Keeping a snapshot is an O(1) Arc clone.
/// Metadata successor construction is O(live documents + vocabulary), not a
/// persistent-tree/O(delta) performance claim; index segments themselves share.
#[derive(Clone)]
pub struct BeaconIndex {
    current: Arc<Generation>,
}

impl core::fmt::Debug for BeaconIndex {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("BeaconIndex").field("stats", &self.snapshot().stats()).finish_non_exhaustive()
    }
}

impl BeaconIndex {
    pub fn new(config: IndexConfig) -> Result<Self, BeaconError> {
        config.validate()?;
        Ok(Self {
            current: Arc::new(Generation {
                config: Arc::new(config), sequence: 0, live: BTreeMap::new(), segments: Vec::new(),
                corpus: CorpusStats::default(), vector_documents: 0, vector_values: 0,
                text_bytes: 0, document_terms: 0,
            }),
        })
    }

    /// Build one base segment without repeatedly cloning an ever-growing live
    /// map between ingestion batches. Bulk input must contain each VId once.
    /// The batch-operation limit governs incremental writes, not bootstrap;
    /// all configured live-corpus limits and caller work limits still apply.
    pub fn build(
        config: IndexConfig,
        documents: impl IntoIterator<Item = IndexDocument>,
        work: &mut dyn WorkControl,
    ) -> Result<Self, BeaconError> {
        Self::try_build(config, documents.into_iter().map(Ok::<_, BeaconError>), work)
    }

    /// Fallible source variant: source errors are propagated unchanged, never
    /// converted into end-of-stream or a successfully indexed partial corpus.
    pub fn try_build<E: From<BeaconError>>(
        config: IndexConfig,
        documents: impl IntoIterator<Item = Result<IndexDocument, E>>,
        work: &mut dyn WorkControl,
    ) -> Result<Self, E> {
        let mut index = Self::new(config)?;
        index.try_replace_all(documents, work)?;
        Ok(index)
    }

    /// Atomically replace the complete derived corpus. Retained snapshots keep
    /// the preceding corpus. A failure leaves this writer unchanged as well.
    pub fn replace_all(
        &mut self,
        documents: impl IntoIterator<Item = IndexDocument>,
        work: &mut dyn WorkControl,
    ) -> Result<IndexStats, BeaconError> {
        self.try_replace_all(documents.into_iter().map(Ok::<_, BeaconError>), work)
    }

    /// Rebuild directly from a fallible source with one final publication.
    /// Source polling is preceded by a work/cancellation checkpoint. The
    /// iterator itself must enforce its own I/O and memory contracts; this
    /// method does not confer graph-read authority on arbitrary documents.
    ///
    /// Unlike replay through repeated apply_batch calls, no old live map or
    /// vocabulary is copied and no intermediate HNSW segments are constructed.
    /// Both modalities and corpus statistics are built from the same input.
    pub fn try_replace_all<E: From<BeaconError>>(
        &mut self,
        documents: impl IntoIterator<Item = Result<IndexDocument, E>>,
        work: &mut dyn WorkControl,
    ) -> Result<IndexStats, E> {
        work.charge(1)?;
        let sequence = self.current.sequence.checked_add(1)
            .ok_or(BeaconError::GenerationExhausted)?;
        let mut next = Generation {
            config: Arc::clone(&self.current.config), sequence,
            live: BTreeMap::new(), segments: Vec::new(),
            corpus: CorpusStats::default(), vector_documents: 0,
            vector_values: 0, text_bytes: 0, document_terms: 0,
        };
        let mut documents = documents.into_iter();
        loop {
            work.charge(1)?;
            let Some(document) = documents.next() else { break; };
            let document = document?;
            let id = document.id;
            if next.live.contains_key(&id) {
                return Err(BeaconError::DuplicateVertex(id).into());
            }
            if next.live.len() == next.config.max_documents {
                return Err(BeaconError::ResourceLimit {
                    resource: "live documents", limit: next.config.max_documents,
                }.into());
            }
            let document = Arc::new(StoredDocument::prepare(document, &next.config, work)?);
            next.add_stats(&document, work)?;
            next.live.insert(id, LiveDocument { generation: sequence, document });
            // Check each prefix before fetching another row; an oversized
            // stream must not be completely buffered before it is refused.
            next.check_limits()?;
        }
        next.compact(work)?;
        work.charge(1)?;
        self.current = Arc::new(next);
        Ok(self.snapshot().stats())
    }

    #[must_use]
    pub fn config(&self) -> &IndexConfig {
        &self.current.config
    }

    #[must_use]
    pub fn snapshot(&self) -> IndexSnapshot {
        IndexSnapshot { generation: Arc::clone(&self.current) }
    }

    /// Last operation for a vertex wins, but EVERY supplied operation is
    /// validated: an invalid vector cannot be hidden by a subsequent delete.
    pub fn apply_batch(
        &mut self,
        mutations: impl IntoIterator<Item = IndexMutation>,
        work: &mut dyn WorkControl,
    ) -> Result<ApplyReport, BeaconError> {
        work.charge(1)?;
        let mut normalized: BTreeMap<VId, Option<Arc<StoredDocument>>> = BTreeMap::new();
        let mut staged = StagedSize::default();
        let mut operations = 0;
        for mutation in mutations {
            work.charge(1)?;
            if operations == self.current.config.max_batch_operations {
                return Err(BeaconError::ResourceLimit { resource: "batch operations", limit: self.current.config.max_batch_operations });
            }
            operations += 1;
            match mutation {
                IndexMutation::Upsert(document) => {
                    let id = document.id;
                    let document = StoredDocument::prepare(document, &self.current.config, work)?;
                    if let Some(Some(previous)) = normalized.get(&id) {
                        staged.update(previous, false)?;
                    }
                    staged.update(&document, true)?;
                    staged.check(&self.current.config)?;
                    normalized.insert(id, Some(Arc::new(document)));
                }
                IndexMutation::Delete(id) => {
                    if let Some(Some(previous)) = normalized.get(&id) {
                        staged.update(previous, false)?;
                    }
                    normalized.insert(id, None);
                }
            }
        }
        let distinct_vertices = normalized.len();
        if distinct_vertices == 0 {
            return Ok(ApplyReport { operations, distinct_vertices, compacted: false, segments: self.current.segments.len() });
        }
        let mut next = self.current.successor(work)?;
        let mut documents = BTreeMap::new();
        for (id, document) in normalized {
            work.charge(1)?;
            if let Some(old) = next.live.remove(&id) {
                next.remove_stats(&old.document, work)?;
            }
            if let Some(document) = document {
                next.add_stats(&document, work)?;
                next.live.insert(id, LiveDocument { generation: next.sequence, document: Arc::clone(&document) });
                documents.insert(id, document);
            }
        }
        next.check_limits()?;
        // Decide the fixed merge before building a delta that would immediately
        // be thrown away. Empty-live cleanup removes all obsolete routing nodes.
        let projected_segments = next.segments.len() + usize::from(!documents.is_empty());
        let compacted = next.live.is_empty() || projected_segments > next.config.max_segments;
        if compacted {
            next.compact(work)?;
        } else if !documents.is_empty() {
            next.segments.push(Arc::new(Segment::build(next.sequence, &documents, &next.config, work)?));
        }
        let report = ApplyReport { operations, distinct_vertices, compacted, segments: next.segments.len() };
        self.current = Arc::new(next);
        Ok(report)
    }

    pub fn compact(&mut self, work: &mut dyn WorkControl) -> Result<(), BeaconError> {
        let mut next = self.current.successor(work)?;
        next.compact(work)?;
        self.current = Arc::new(next);
        Ok(())
    }
}

#[derive(Clone)]
pub struct IndexSnapshot {
    generation: Arc<Generation>,
}

impl core::fmt::Debug for IndexSnapshot {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("IndexSnapshot").field("stats", &self.stats()).finish_non_exhaustive()
    }
}

impl IndexSnapshot {
    #[must_use]
    pub fn stats(&self) -> IndexStats {
        IndexStats {
            documents: self.generation.live.len(), vector_documents: self.generation.vector_documents,
            vector_values: self.generation.vector_values, text: self.generation.corpus.stats(),
            text_bytes: self.generation.text_bytes, segments: self.generation.segments.len(),
        }
    }

    pub fn knn(
        &self,
        query: &[f32],
        k: usize,
        mode: VectorSearch,
        eligible: impl Fn(VId) -> bool,
        work: &mut dyn WorkControl,
    ) -> Result<Vec<Neighbor>, BeaconError> {
        let config = self.generation.config.vector.as_ref().ok_or(BeaconError::Disabled("vector"))?;
        config.validate_vector(query, work)?;
        if matches!(mode, VectorSearch::Approximate { ef_search: 0 }) {
            return Err(BeaconError::InvalidQuery("ef_search must be positive"));
        }
        let limit = k.min(self.generation.vector_documents);
        let mut best = BinaryHeap::new();
        if limit != 0 {
            for segment in &self.generation.segments {
                work.charge(1)?;
                if let Some(vector) = &segment.vector {
                    let visible = |id| self.visible_in(id, segment.generation) && eligible(id);
                    for hit in vector.search(query, limit, mode, visible, work)? {
                        retain_best(&mut best, Ranked { cost: hit.distance, id: hit.id, slot: 0 }, limit);
                    }
                }
            }
        }
        Ok(best.into_sorted_vec().into_iter().map(|hit| Neighbor { id: hit.id, distance: hit.cost }).collect())
    }

    pub fn text_search(
        &self,
        query: &str,
        k: usize,
        mode: TextMatch,
        eligible: impl Fn(VId) -> bool,
        work: &mut dyn WorkControl,
    ) -> Result<Vec<TextHit>, BeaconError> {
        let config = self.generation.config.text.as_ref().ok_or(BeaconError::Disabled("text"))?;
        let terms = config.query_terms(query, work)?;
        let limit = k.min(self.generation.corpus.documents);
        let mut best = BinaryHeap::new();
        for segment in &self.generation.segments {
            work.charge(1)?;
            let visible = |id| self.visible_in(id, segment.generation) && eligible(id);
            segment.text.search_into(&terms, mode, config, &self.generation.corpus, limit, &visible, &mut best, work)?;
        }
        Ok(best.into_sorted_vec().into_iter().map(|hit| TextHit { id: hit.id, score: -hit.cost }).collect())
    }

    fn visible_in(&self, id: VId, generation: u64) -> bool {
        self.generation.live.get(&id).is_some_and(|live| live.generation == generation)
    }

    /// Weighted reciprocal-rank fusion of two explicitly bounded candidate
    /// sets. This is NOT an exhaustive hybrid-top-k certificate. Both lanes
    /// read this exact immutable generation; raw distance and BM25 scales are
    /// never added to one another.
    pub fn hybrid_search(
        &self,
        query: HybridQuery<'_>,
        eligible: impl Fn(VId) -> bool,
        work: &mut dyn WorkControl,
    ) -> Result<Vec<HybridHit>, BeaconError> {
        query.validate()?;
        let vector = self.knn(query.vector, query.candidates, query.vector_mode, &eligible, work)?;
        let text = self.text_search(query.text, query.candidates, query.text_mode, &eligible, work)?;
        let total_weight = query.vector_weight + query.text_weight;
        let mut fused = BTreeMap::<VId, HybridHit>::new();
        for (rank, hit) in vector.into_iter().enumerate() {
            work.charge(1)?;
            let row = fused.entry(hit.id).or_insert(HybridHit { id: hit.id, score: 0.0, vector_distance: None, text_score: None });
            row.vector_distance = Some(hit.distance);
            row.score += (query.vector_weight / total_weight) / (query.rank_constant + rank as f64 + 1.0);
        }
        for (rank, hit) in text.into_iter().enumerate() {
            work.charge(1)?;
            let row = fused.entry(hit.id).or_insert(HybridHit { id: hit.id, score: 0.0, vector_distance: None, text_score: None });
            row.text_score = Some(hit.score);
            row.score += (query.text_weight / total_weight) / (query.rank_constant + rank as f64 + 1.0);
        }
        let mut best = BinaryHeap::new();
        for hit in fused.values() {
            work.charge(1)?;
            if hit.score > 0.0 {
                retain_best(&mut best, Ranked { cost: -hit.score, id: hit.id, slot: 0 }, query.k);
            }
        }
        best.into_sorted_vec().into_iter().map(|hit| {
            fused.remove(&hit.id).ok_or(BeaconError::Invariant("fused result disappeared"))
        }).collect()
    }
}

#[derive(Clone, Copy, Debug)]
pub struct HybridQuery<'a> {
    pub vector: &'a [f32],
    pub text: &'a str,
    pub k: usize,
    pub candidates: usize,
    pub vector_mode: VectorSearch,
    pub text_mode: TextMatch,
    pub rank_constant: f64,
    pub vector_weight: f64,
    pub text_weight: f64,
}

impl HybridQuery<'_> {
    fn validate(&self) -> Result<(), BeaconError> {
        if self.candidates < self.k {
            return Err(BeaconError::InvalidQuery("hybrid candidates must be at least k"));
        }
        if !self.rank_constant.is_finite() || self.rank_constant < 1.0 {
            return Err(BeaconError::InvalidQuery("RRF rank constant must be finite and at least one"));
        }
        let sum = self.vector_weight + self.text_weight;
        if !self.vector_weight.is_finite() || !self.text_weight.is_finite()
            || self.vector_weight < 0.0 || self.text_weight < 0.0 || !sum.is_finite() || sum <= 0.0
        {
            return Err(BeaconError::InvalidQuery("RRF weights must be finite, nonnegative, with a finite positive sum"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct HybridHit {
    pub id: VId,
    pub score: f64,
    pub vector_distance: Option<f64>,
    pub text_score: Option<f64>,
}
