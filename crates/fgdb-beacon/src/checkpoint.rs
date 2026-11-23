//! Canonical binary checkpoint codec (`FGBC` v1) for [`BeaconIndex`].
//!
//! Persists the exact materialized `BeaconIndex` generation — including live
//! analyzed documents, live-corpus BM25 statistics, materialized HNSW routing
//! segments, and BM25 positional posting segments — without `serde` (`Doctrine
//! #1, #7`). Decoding enforces every resource limit, coordinate finiteness
//! check, posting order invariant, and HNSW routing graph bound before
//! publishing an `Arc<Generation>`.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use fgdb_types::VId;

use crate::bm25::{AnalyzedText, CorpusStats, TextSegment};
use crate::fabric::{Generation, LiveDocument, Segment, StoredDocument};
use crate::hnsw::{MAX_LEVEL, Node};
use crate::{
    BeaconError, BeaconIndex, Bm25Config, DistanceMetric, Hnsw, HnswConfig, IndexConfig,
    WorkControl,
};

const MAGIC: [u8; 4] = *b"FGBC";
const VERSION_V1: u16 = 1;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BeaconCheckpointError {
    Malformed(&'static str),
    ConfigMismatch,
    Beacon(BeaconError),
}

impl From<BeaconError> for BeaconCheckpointError {
    fn from(error: BeaconError) -> Self {
        Self::Beacon(error)
    }
}

impl core::fmt::Display for BeaconCheckpointError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Malformed(why) => write!(f, "malformed Beacon checkpoint: {why}"),
            Self::ConfigMismatch => f.write_str("Beacon checkpoint configuration mismatch"),
            Self::Beacon(error) => error.fmt(f),
        }
    }
}

impl core::error::Error for BeaconCheckpointError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Beacon(error) => Some(error),
            _ => None,
        }
    }
}

struct Cursor<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], BeaconCheckpointError> {
        let end = self
            .pos
            .checked_add(len)
            .ok_or(BeaconCheckpointError::Malformed("truncated payload"))?;
        if end > self.bytes.len() {
            return Err(BeaconCheckpointError::Malformed("truncated payload"));
        }
        let out = &self.bytes[self.pos..end];
        self.pos = end;
        Ok(out)
    }

    // A configured ceiling is not evidence that a count can be present in this
    // frame. Check the minimum wire footprint before any count-sized reserve.
    fn require_items(
        &self,
        count: usize,
        minimum_bytes: usize,
    ) -> Result<(), BeaconCheckpointError> {
        if count > (self.bytes.len() - self.pos) / minimum_bytes {
            return Err(BeaconCheckpointError::Malformed("truncated payload"));
        }
        Ok(())
    }

    fn u8(&mut self) -> Result<u8, BeaconCheckpointError> {
        Ok(self.take(1)?[0])
    }

    fn bool(&mut self) -> Result<bool, BeaconCheckpointError> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(BeaconCheckpointError::Malformed("invalid boolean tag")),
        }
    }

    fn u16(&mut self) -> Result<u16, BeaconCheckpointError> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    fn u32(&mut self) -> Result<u32, BeaconCheckpointError> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn u64(&mut self) -> Result<u64, BeaconCheckpointError> {
        let b = self.take(8)?;
        let mut arr = [0u8; 8];
        arr.copy_from_slice(b);
        Ok(u64::from_le_bytes(arr))
    }

    fn usize_checked(&mut self) -> Result<usize, BeaconCheckpointError> {
        usize::try_from(self.u64()?)
            .map_err(|_| BeaconCheckpointError::Malformed("usize count overflow"))
    }

    fn u128(&mut self) -> Result<u128, BeaconCheckpointError> {
        let b = self.take(16)?;
        let mut arr = [0u8; 16];
        arr.copy_from_slice(b);
        Ok(u128::from_le_bytes(arr))
    }

    fn f32(&mut self) -> Result<f32, BeaconCheckpointError> {
        Ok(f32::from_bits(self.u32()?))
    }

    fn f64(&mut self) -> Result<f64, BeaconCheckpointError> {
        Ok(f64::from_bits(self.u64()?))
    }

    fn str_bounded(
        &mut self,
        max_bytes: usize,
        work: &mut dyn WorkControl,
    ) -> Result<String, BeaconCheckpointError> {
        let len = self.usize_checked()?;
        if len > max_bytes {
            return Err(BeaconCheckpointError::Malformed("string exceeds limit"));
        }
        let raw = self.take(len)?;
        work.charge(len.max(1))?;
        let text = core::str::from_utf8(raw)
            .map_err(|_| BeaconCheckpointError::Malformed("invalid UTF-8 string"))?;
        let mut owned = String::new();
        owned
            .try_reserve_exact(len)
            .map_err(|_| BeaconError::AllocationFailed { requested: len })?;
        owned.push_str(text);
        Ok(owned)
    }

    fn finish(self) -> Result<(), BeaconCheckpointError> {
        if self.pos != self.bytes.len() {
            return Err(BeaconCheckpointError::Malformed("trailing bytes"));
        }
        Ok(())
    }
}

fn decoded_vec<T>(count: usize) -> Result<Vec<T>, BeaconCheckpointError> {
    let requested = count
        .checked_mul(size_of::<T>())
        .ok_or(BeaconCheckpointError::Malformed(
            "decoded capacity overflow",
        ))?;
    let mut out = Vec::new();
    out.try_reserve_exact(count)
        .map_err(|_| BeaconError::AllocationFailed { requested })?;
    Ok(out)
}

// SimpleUnicodeV1 applies full Unicode lowercase. A source scalar occupies at
// least one byte and its mapping has at most three scalars of up to four bytes
// each. In particular, U+0130 grows from two input bytes to three analyzed bytes.
// The frame and work allowance still bound the actual allocation independently.
fn analyzed_term_bytes(source_bytes: usize) -> usize {
    source_bytes.saturating_mul(12)
}

/// All output growth passes this boundary, including nested vectors, postings
/// and term bytes. Reserve geometrically within the ceiling instead of letting
/// Vec double beyond admission. The limit bounds requested capacity, not the
/// allocator's bookkeeping or the source index retained by the caller.
struct CheckpointWriter<'a> {
    out: &'a mut Vec<u8>,
    limit: usize,
}

impl CheckpointWriter<'_> {
    fn bytes(&mut self, bytes: &[u8]) -> Result<(), BeaconError> {
        let required = self
            .out
            .len()
            .checked_add(bytes.len())
            .filter(|&len| len <= self.limit)
            .ok_or(BeaconError::ResourceLimit {
                resource: "checkpoint bytes",
                limit: self.limit,
            })?;
        if required > self.out.capacity() {
            let capacity = required.max(
                self.out
                    .capacity()
                    .max(128)
                    .saturating_mul(2)
                    .min(self.limit),
            );
            self.out
                .try_reserve_exact(capacity - self.out.len())
                .map_err(|_| BeaconError::AllocationFailed {
                    requested: capacity,
                })?;
        }
        self.out.extend_from_slice(bytes);
        Ok(())
    }

    fn byte(&mut self, byte: u8) -> Result<(), BeaconError> {
        self.bytes(&[byte])
    }

    fn count(&mut self, value: usize) -> Result<(), BeaconError> {
        self.bytes(&(value as u64).to_le_bytes())
    }

    fn text(&mut self, value: &str) -> Result<(), BeaconError> {
        self.count(value.len())?;
        self.bytes(value.as_bytes())
    }
}

fn encode_metric(metric: DistanceMetric) -> u8 {
    match metric {
        DistanceMetric::SquaredEuclidean => 1,
        DistanceMetric::Cosine => 2,
        DistanceMetric::NegativeDotProduct => 3,
    }
}

fn decode_metric(code: u8) -> Result<DistanceMetric, BeaconCheckpointError> {
    match code {
        1 => Ok(DistanceMetric::SquaredEuclidean),
        2 => Ok(DistanceMetric::Cosine),
        3 => Ok(DistanceMetric::NegativeDotProduct),
        _ => Err(BeaconCheckpointError::Malformed("unknown distance metric")),
    }
}

fn encode_hnsw_config(out: &mut CheckpointWriter<'_>, cfg: &HnswConfig) -> Result<(), BeaconError> {
    out.count(cfg.dimensions)?;
    out.byte(encode_metric(cfg.metric))?;
    out.count(cfg.m)?;
    out.count(cfg.ef_construction)?;
    out.bytes(&cfg.seed.to_le_bytes())?;
    out.count(cfg.max_vectors)?;
    out.byte(u8::from(cfg.links))
}

fn decode_hnsw_config(cur: &mut Cursor<'_>) -> Result<HnswConfig, BeaconCheckpointError> {
    let cfg = HnswConfig {
        dimensions: cur.usize_checked()?,
        metric: decode_metric(cur.u8()?)?,
        m: cur.usize_checked()?,
        ef_construction: cur.usize_checked()?,
        seed: cur.u64()?,
        max_vectors: cur.usize_checked()?,
        links: cur.bool()?,
    };
    cfg.validate()?;
    Ok(cfg)
}

fn encode_bm25_config(out: &mut CheckpointWriter<'_>, cfg: &Bm25Config) -> Result<(), BeaconError> {
    out.bytes(&cfg.k1.to_bits().to_le_bytes())?;
    out.bytes(&cfg.b.to_bits().to_le_bytes())?;
    out.count(cfg.max_documents)?;
    out.count(cfg.max_document_bytes)?;
    out.count(cfg.max_document_terms)?;
    out.count(cfg.max_query_bytes)?;
    out.count(cfg.max_query_terms)
}

fn decode_bm25_config(cur: &mut Cursor<'_>) -> Result<Bm25Config, BeaconCheckpointError> {
    let cfg = Bm25Config {
        k1: cur.f64()?,
        b: cur.f64()?,
        max_documents: cur.usize_checked()?,
        max_document_bytes: cur.usize_checked()?,
        max_document_terms: cur.usize_checked()?,
        max_query_bytes: cur.usize_checked()?,
        max_query_terms: cur.usize_checked()?,
    };
    cfg.validate()?;
    Ok(cfg)
}

fn encode_index_config(
    out: &mut CheckpointWriter<'_>,
    cfg: &IndexConfig,
) -> Result<(), BeaconError> {
    match &cfg.vector {
        Some(v) => {
            out.byte(1)?;
            encode_hnsw_config(out, v)?;
        }
        None => out.byte(0)?,
    }
    match &cfg.text {
        Some(t) => {
            out.byte(1)?;
            encode_bm25_config(out, t)?;
        }
        None => out.byte(0)?,
    }
    out.count(cfg.max_documents)?;
    out.count(cfg.max_batch_operations)?;
    out.count(cfg.max_segments)?;
    out.count(cfg.max_vector_values)?;
    out.count(cfg.max_text_bytes)?;
    out.count(cfg.max_document_terms)
}

fn decode_index_config(cur: &mut Cursor<'_>) -> Result<IndexConfig, BeaconCheckpointError> {
    let vector = if cur.bool()? {
        Some(decode_hnsw_config(cur)?)
    } else {
        None
    };
    let text = if cur.bool()? {
        Some(decode_bm25_config(cur)?)
    } else {
        None
    };
    let cfg = IndexConfig {
        vector,
        text,
        max_documents: cur.usize_checked()?,
        max_batch_operations: cur.usize_checked()?,
        max_segments: cur.usize_checked()?,
        max_vector_values: cur.usize_checked()?,
        max_text_bytes: cur.usize_checked()?,
        max_document_terms: cur.usize_checked()?,
    };
    cfg.validate()?;
    Ok(cfg)
}

fn same_f64_bits(a: f64, b: f64) -> bool {
    a.to_bits() == b.to_bits()
}

fn same_index_config(left: &IndexConfig, right: &IndexConfig) -> bool {
    left.vector == right.vector
        && left.max_documents == right.max_documents
        && left.max_batch_operations == right.max_batch_operations
        && left.max_segments == right.max_segments
        && left.max_vector_values == right.max_vector_values
        && left.max_text_bytes == right.max_text_bytes
        && left.max_document_terms == right.max_document_terms
        && match (&left.text, &right.text) {
            (None, None) => true,
            (Some(a), Some(b)) => {
                same_f64_bits(a.k1, b.k1)
                    && same_f64_bits(a.b, b.b)
                    && a.max_documents == b.max_documents
                    && a.max_document_bytes == b.max_document_bytes
                    && a.max_document_terms == b.max_document_terms
                    && a.max_query_bytes == b.max_query_bytes
                    && a.max_query_terms == b.max_query_terms
            }
            _ => false,
        }
}

fn encode_vector_slice(
    out: &mut CheckpointWriter<'_>,
    values: &[f32],
    work: &mut dyn WorkControl,
) -> Result<(), BeaconError> {
    work.charge(values.len().max(1))?;
    out.count(values.len())?;
    for &v in values {
        out.bytes(&v.to_bits().to_le_bytes())?;
    }
    Ok(())
}

fn decode_vector_arc(
    cur: &mut Cursor<'_>,
    cfg: &HnswConfig,
    work: &mut dyn WorkControl,
) -> Result<(Arc<[f32]>, f64), BeaconCheckpointError> {
    let len = cur.usize_checked()?;
    if len != cfg.dimensions {
        return Err(BeaconError::Dimension {
            expected: cfg.dimensions,
            actual: len,
        }
        .into());
    }
    cur.require_items(len, size_of::<f32>())?;
    work.charge(len)?;
    let mut vals = decoded_vec(len)?;
    for _ in 0..len {
        vals.push(cur.f32()?);
    }
    let norm = cfg.validate_vector(&vals, work)?;
    Ok((Arc::from(vals), norm))
}

fn encode_analyzed_text(
    out: &mut CheckpointWriter<'_>,
    text: &AnalyzedText,
    work: &mut dyn WorkControl,
) -> Result<(), BeaconError> {
    work.charge(1)?;
    out.bytes(&text.length.to_le_bytes())?;
    out.count(text.source_bytes)?;
    out.count(text.frequencies.len())?;
    for (term, positions) in &text.frequencies {
        work.charge(term.len().saturating_add(positions.len()).max(1))?;
        out.text(term)?;
        out.count(positions.len())?;
        for &pos in positions.iter() {
            out.bytes(&pos.to_le_bytes())?;
        }
    }
    Ok(())
}

fn decode_analyzed_text(
    cur: &mut Cursor<'_>,
    cfg: &Bm25Config,
    work: &mut dyn WorkControl,
) -> Result<AnalyzedText, BeaconCheckpointError> {
    work.charge(1)?;
    let length = cur.u64()?;
    let source_bytes = cur.usize_checked()?;
    if source_bytes > cfg.max_document_bytes {
        return Err(BeaconError::ResourceLimit {
            resource: "text bytes",
            limit: cfg.max_document_bytes,
        }
        .into());
    }
    if length > source_bytes as u64 {
        return Err(BeaconCheckpointError::Malformed(
            "analyzed token length exceeds source bytes",
        ));
    }
    let term_count = cur.usize_checked()?;
    if term_count > cfg.max_document_terms {
        return Err(BeaconError::ResourceLimit {
            resource: "distinct analyzed terms",
            limit: cfg.max_document_terms,
        }
        .into());
    }
    cur.require_items(term_count, 25)?;
    let mut frequencies: BTreeMap<String, Arc<Vec<u64>>> = BTreeMap::new();
    let mut seen_positions = BTreeSet::new();
    let mut total_positions = 0u64;
    for _ in 0..term_count {
        work.charge(1)?;
        let term = cur.str_bounded(analyzed_term_bytes(source_bytes), work)?;
        if term.is_empty() {
            return Err(BeaconCheckpointError::Malformed("empty analyzed term"));
        }
        if frequencies
            .last_key_value()
            .is_some_and(|(prev, _)| term.as_str() <= prev.as_str())
        {
            return Err(BeaconCheckpointError::Malformed("unsorted analyzed terms"));
        }
        let pos_count = cur.usize_checked()?;
        if pos_count == 0 || pos_count > source_bytes {
            return Err(BeaconCheckpointError::Malformed(
                "invalid term position count",
            ));
        }
        cur.require_items(pos_count, size_of::<u64>())?;
        work.charge(term.len().saturating_add(pos_count))?;
        let mut positions = decoded_vec(pos_count)?;
        let mut prev_pos: Option<u64> = None;
        for _ in 0..pos_count {
            let pos = cur.u64()?;
            if pos >= length
                || prev_pos.is_some_and(|prev| pos <= prev)
                || !seen_positions.insert(pos)
            {
                return Err(BeaconCheckpointError::Malformed("invalid token position"));
            }
            prev_pos = Some(pos);
            positions.push(pos);
        }
        total_positions = total_positions
            .checked_add(pos_count as u64)
            .ok_or(BeaconCheckpointError::Malformed("position sum overflow"))?;
        frequencies.insert(term, Arc::new(positions));
    }
    if total_positions != length {
        return Err(BeaconCheckpointError::Malformed(
            "analyzed token length disagrees with positions",
        ));
    }
    Ok(AnalyzedText {
        frequencies,
        length,
        source_bytes,
    })
}

fn same_f32_bits(left: &[f32], right: &[f32]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right.iter())
            .all(|(a, b)| a.to_bits() == b.to_bits())
}

impl BeaconIndex {
    /// Serialize this materialized `BeaconIndex` generation into canonical
    /// `FGBC` v1 checkpoint bytes under caller work accounting.
    pub fn encode_checkpoint(&self, work: &mut dyn WorkControl) -> Result<Vec<u8>, BeaconError> {
        self.encode_checkpoint_bounded(usize::MAX, work)
    }

    /// Serialize under a byte ceiling checked before every output allocation.
    /// The source index is borrowed; a failed encoding publishes no bytes and
    /// leaves its generation and search results unchanged.
    pub fn encode_checkpoint_bounded(
        &self,
        max_bytes: usize,
        work: &mut dyn WorkControl,
    ) -> Result<Vec<u8>, BeaconError> {
        let mut out = Vec::new();
        self.append_checkpoint(&mut out, max_bytes, work)?;
        Ok(out)
    }

    /// Append directly to a caller's container, avoiding a second complete
    /// checkpoint allocation. `max_bytes` includes the existing prefix. On any
    /// refusal the prefix bytes and length are unchanged; capacity already
    /// admitted during the failed attempt may remain available to the caller.
    pub fn append_checkpoint(
        &self,
        out: &mut Vec<u8>,
        max_bytes: usize,
        work: &mut dyn WorkControl,
    ) -> Result<usize, BeaconError> {
        let prefix = out.len();
        if prefix > max_bytes {
            return Err(BeaconError::ResourceLimit {
                resource: "checkpoint bytes",
                limit: max_bytes,
            });
        }
        let result = self.write_checkpoint(
            &mut CheckpointWriter {
                out,
                limit: max_bytes,
            },
            work,
        );
        match result {
            Ok(()) => Ok(out.len() - prefix),
            Err(error) => {
                out.truncate(prefix);
                Err(error)
            }
        }
    }

    fn write_checkpoint(
        &self,
        out: &mut CheckpointWriter<'_>,
        work: &mut dyn WorkControl,
    ) -> Result<(), BeaconError> {
        let generation = &self.current;
        work.charge(1)?;
        out.bytes(&MAGIC)?;
        out.bytes(&VERSION_V1.to_le_bytes())?;
        encode_index_config(out, &generation.config)?;
        out.bytes(&generation.sequence.to_le_bytes())?;

        out.count(generation.live.len())?;
        for (&vid, live) in &generation.live {
            work.charge(1)?;
            out.bytes(&vid.0.to_le_bytes())?;
            out.bytes(&live.generation.to_le_bytes())?;
            match &live.document.vector {
                Some(vec) => {
                    out.byte(1)?;
                    encode_vector_slice(out, vec, work)?;
                }
                None => out.byte(0)?,
            }
            match &live.document.text {
                Some(text) => {
                    out.byte(1)?;
                    encode_analyzed_text(out, text, work)?;
                }
                None => out.byte(0)?,
            }
        }

        out.count(generation.segments.len())?;
        for segment in &generation.segments {
            work.charge(1)?;
            out.bytes(&segment.generation.to_le_bytes())?;
            match &segment.vector {
                Some(hnsw) => {
                    out.byte(1)?;
                    match hnsw.entry {
                        Some(entry) => {
                            out.byte(1)?;
                            out.count(entry)?;
                        }
                        None => out.byte(0)?,
                    }
                    out.count(hnsw.top_level)?;
                    out.count(hnsw.nodes.len())?;
                    for node in &hnsw.nodes {
                        work.charge(1)?;
                        out.bytes(&node.id.0.to_le_bytes())?;
                        encode_vector_slice(out, &node.vector, work)?;
                        out.count(node.links.len())?;
                        for layer_links in &node.links {
                            work.charge(layer_links.len().max(1))?;
                            out.count(layer_links.len())?;
                            for &slot in layer_links {
                                out.count(slot)?;
                            }
                        }
                    }
                }
                None => out.byte(0)?,
            }

            out.count(segment.text.lengths.len())?;
            for (&vid, &length) in &segment.text.lengths {
                work.charge(1)?;
                out.bytes(&vid.0.to_le_bytes())?;
                out.bytes(&length.to_le_bytes())?;
            }
            out.count(segment.text.postings.len())?;
            for (term, doc_map) in &segment.text.postings {
                work.charge(term.len().max(1))?;
                out.text(term)?;
                out.count(doc_map.len())?;
                for (&vid, positions) in doc_map {
                    work.charge(positions.len().max(1))?;
                    out.bytes(&vid.0.to_le_bytes())?;
                    out.count(positions.len())?;
                    for &pos in positions.iter() {
                        out.bytes(&pos.to_le_bytes())?;
                    }
                }
            }
        }
        work.charge(1)?;
        Ok(())
    }

    /// Rehydrate a `BeaconIndex` from `FGBC` v1 checkpoint bytes, validating
    /// exact configuration match, corpus invariants, posting invariants, and
    /// HNSW routing-graph topology before returning.
    pub fn decode_checkpoint(
        bytes: &[u8],
        expected_config: &IndexConfig,
        work: &mut dyn WorkControl,
    ) -> Result<Self, BeaconCheckpointError> {
        work.charge(1)?;
        expected_config.validate()?;
        let mut cur = Cursor::new(bytes);
        if cur.take(4)? != MAGIC {
            return Err(BeaconCheckpointError::Malformed("bad magic"));
        }
        if cur.u16()? != VERSION_V1 {
            return Err(BeaconCheckpointError::Malformed("unsupported version"));
        }
        let stored_config = decode_index_config(&mut cur)?;
        if !same_index_config(&stored_config, expected_config) {
            return Err(BeaconCheckpointError::ConfigMismatch);
        }
        let sequence = cur.u64()?;
        let mut next = Generation {
            config: Arc::new(stored_config),
            sequence,
            live: BTreeMap::new(),
            segments: Vec::new(),
            corpus: CorpusStats::default(),
            vector_documents: 0,
            vector_values: 0,
            text_bytes: 0,
            document_terms: 0,
        };

        let live_count = cur.usize_checked()?;
        if live_count > next.config.max_documents {
            return Err(BeaconError::ResourceLimit {
                resource: "live documents",
                limit: next.config.max_documents,
            }
            .into());
        }
        cur.require_items(live_count, 26)?;
        let mut prev_vid: Option<VId> = None;
        for _ in 0..live_count {
            work.charge(1)?;
            let vid = VId(cur.u128()?);
            if prev_vid.is_some_and(|prev| vid <= prev) {
                return Err(BeaconCheckpointError::Malformed("unsorted live VIds"));
            }
            prev_vid = Some(vid);
            let doc_gen = cur.u64()?;
            if doc_gen > sequence {
                return Err(BeaconCheckpointError::Malformed(
                    "live document generation exceeds index sequence",
                ));
            }
            let vector = if cur.bool()? {
                let vcfg = next
                    .config
                    .vector
                    .as_ref()
                    .ok_or(BeaconCheckpointError::Malformed(
                        "vector present when vector lane disabled",
                    ))?;
                let (vec_arc, _) = decode_vector_arc(&mut cur, vcfg, work)?;
                Some(vec_arc)
            } else {
                None
            };
            let text = if cur.bool()? {
                let tcfg = next
                    .config
                    .text
                    .as_ref()
                    .ok_or(BeaconCheckpointError::Malformed(
                        "text present when text lane disabled",
                    ))?;
                Some(decode_analyzed_text(&mut cur, tcfg, work)?)
            } else {
                None
            };
            let doc = Arc::new(StoredDocument { vector, text });
            next.add_stats(&doc, work)?;
            next.live.insert(
                vid,
                LiveDocument {
                    generation: doc_gen,
                    document: doc,
                },
            );
            next.check_limits()?;
        }

        let segment_count = cur.usize_checked()?;
        if segment_count > next.config.max_segments {
            return Err(BeaconError::ResourceLimit {
                resource: "segments",
                limit: next.config.max_segments,
            }
            .into());
        }
        cur.require_items(segment_count, 25)?;
        next.segments = decoded_vec(segment_count)?;
        let mut live_found_in_segment = BTreeSet::new();
        let mut previous_segment_generation = None;
        for _ in 0..segment_count {
            work.charge(1)?;
            let seg_gen = cur.u64()?;
            if seg_gen > sequence
                || previous_segment_generation.is_some_and(|previous| seg_gen <= previous)
            {
                return Err(BeaconCheckpointError::Malformed(
                    "segment generations are not strictly increasing within index sequence",
                ));
            }
            previous_segment_generation = Some(seg_gen);
            let has_vector = cur.bool()?;
            if has_vector != next.config.vector.is_some() {
                return Err(BeaconCheckpointError::Malformed(
                    "segment vector lane presence disagrees with config",
                ));
            }
            let vector = if has_vector {
                let vcfg = next.config.vector.as_ref().expect("checked");
                let entry = if cur.bool()? {
                    Some(cur.usize_checked()?)
                } else {
                    None
                };
                let top_level = cur.usize_checked()?;
                if top_level > MAX_LEVEL {
                    return Err(BeaconCheckpointError::Malformed("HNSW top_level overflow"));
                }
                let node_count = cur.usize_checked()?;
                if node_count > vcfg.max_vectors {
                    return Err(BeaconError::ResourceLimit {
                        resource: "vectors per HNSW segment",
                        limit: vcfg.max_vectors,
                    }
                    .into());
                }
                cur.require_items(node_count, 32)?;
                let mut nodes = decoded_vec::<Node>(node_count)?;
                let mut prev_node_id: Option<VId> = None;
                for slot in 0..node_count {
                    work.charge(1)?;
                    let id = VId(cur.u128()?);
                    if prev_node_id.is_some_and(|prev| id <= prev) {
                        return Err(BeaconCheckpointError::Malformed("unsorted HNSW node VIds"));
                    }
                    prev_node_id = Some(id);
                    let (mut vec_arc, norm) = decode_vector_arc(&mut cur, vcfg, work)?;
                    if let Some(live_doc) = next.live.get(&id) {
                        if live_doc.generation < seg_gen {
                            return Err(BeaconCheckpointError::Malformed(
                                "HNSW node is newer than its live document",
                            ));
                        }
                        if live_doc.generation == seg_gen {
                            work.charge(vec_arc.len().max(1))?;
                            let Some(live_vec) = &live_doc.document.vector else {
                                return Err(BeaconCheckpointError::Malformed(
                                    "live HNSW node has no vector document",
                                ));
                            };
                            if !same_f32_bits(live_vec, &vec_arc) {
                                return Err(BeaconCheckpointError::Malformed(
                                    "live HNSW node disagrees with vector document",
                                ));
                            }
                            vec_arc = Arc::clone(live_vec);
                            live_found_in_segment.insert((id, true));
                        }
                    }
                    let layer_count = cur.usize_checked()?;
                    if !vcfg.links {
                        if layer_count != 0 {
                            return Err(BeaconCheckpointError::Malformed(
                                "exact-only HNSW segment contains links",
                            ));
                        }
                    } else if layer_count == 0 || layer_count > top_level + 1 {
                        return Err(BeaconCheckpointError::Malformed(
                            "invalid HNSW node layer count",
                        ));
                    }
                    cur.require_items(layer_count, size_of::<u64>())?;
                    let mut links = decoded_vec(layer_count)?;
                    for layer in 0..layer_count {
                        let deg = cur.usize_checked()?;
                        let max_deg = if layer == 0 { 2 * vcfg.m } else { vcfg.m };
                        if deg > max_deg {
                            return Err(BeaconCheckpointError::Malformed(
                                "HNSW layer degree exceeds configured bound",
                            ));
                        }
                        cur.require_items(deg, size_of::<u64>())?;
                        work.charge(deg.max(1))?;
                        let mut layer_links = decoded_vec(deg)?;
                        let mut seen = BTreeSet::new();
                        for _ in 0..deg {
                            let nb = cur.usize_checked()?;
                            if nb >= node_count || nb == slot || !seen.insert(nb) {
                                return Err(BeaconCheckpointError::Malformed(
                                    "invalid HNSW neighbor slot",
                                ));
                            }
                            layer_links.push(nb);
                        }
                        links.push(layer_links);
                    }
                    nodes.push(Node {
                        id,
                        vector: vec_arc,
                        norm,
                        links,
                    });
                }
                if !vcfg.links || nodes.is_empty() {
                    if entry.is_some() || top_level != 0 {
                        return Err(BeaconCheckpointError::Malformed(
                            "unexpected HNSW entry for empty or linkless segment",
                        ));
                    }
                } else {
                    let Some(entry_slot) = entry else {
                        return Err(BeaconCheckpointError::Malformed(
                            "missing HNSW entry for linked segment",
                        ));
                    };
                    if entry_slot >= nodes.len() || nodes[entry_slot].links.len() != top_level + 1 {
                        return Err(BeaconCheckpointError::Malformed(
                            "HNSW entry slot does not match top_level",
                        ));
                    }
                    for node in &nodes {
                        work.charge(1)?;
                        for (layer, layer_links) in node.links.iter().enumerate() {
                            work.charge(layer_links.len().max(1))?;
                            for &nb in layer_links {
                                if nodes[nb].links.len() <= layer {
                                    return Err(BeaconCheckpointError::Malformed(
                                        "HNSW link targets node absent from layer",
                                    ));
                                }
                            }
                        }
                    }
                }
                Some(Hnsw {
                    config: vcfg.clone(),
                    nodes,
                    entry,
                    top_level,
                })
            } else {
                None
            };

            let len_count = cur.usize_checked()?;
            if next.config.text.is_none() && len_count != 0 {
                return Err(BeaconCheckpointError::Malformed(
                    "text segment non-empty when text lane disabled",
                ));
            }
            if let Some(tcfg) = &next.config.text
                && len_count > tcfg.max_documents
            {
                return Err(BeaconError::ResourceLimit {
                    resource: "live text documents",
                    limit: tcfg.max_documents,
                }
                .into());
            }
            cur.require_items(len_count, 24)?;
            let mut lengths = BTreeMap::new();
            let mut prev_len_vid: Option<VId> = None;
            for _ in 0..len_count {
                work.charge(1)?;
                let vid = VId(cur.u128()?);
                if prev_len_vid.is_some_and(|prev| vid <= prev) {
                    return Err(BeaconCheckpointError::Malformed(
                        "unsorted text segment lengths",
                    ));
                }
                prev_len_vid = Some(vid);
                let doc_len = cur.u64()?;
                if let Some(live_doc) = next.live.get(&vid) {
                    if live_doc.generation < seg_gen {
                        return Err(BeaconCheckpointError::Malformed(
                            "text segment is newer than its live document",
                        ));
                    }
                    if live_doc.generation == seg_gen
                        && live_doc
                            .document
                            .text
                            .as_ref()
                            .is_none_or(|text| text.length != doc_len)
                    {
                        return Err(BeaconCheckpointError::Malformed(
                            "live segment length disagrees with text document",
                        ));
                    }
                }
                lengths.insert(vid, doc_len);
            }

            let term_count = cur.usize_checked()?;
            if len_count == 0 && term_count != 0 {
                return Err(BeaconCheckpointError::Malformed(
                    "text postings without segment document lengths",
                ));
            }
            let max_term_bytes = next
                .config
                .text
                .as_ref()
                .map_or(0, |tcfg| analyzed_term_bytes(tcfg.max_document_bytes));
            cur.require_items(term_count, 49)?;
            let mut postings: BTreeMap<String, BTreeMap<VId, Arc<Vec<u64>>>> = BTreeMap::new();
            for _ in 0..term_count {
                work.charge(1)?;
                let term = cur.str_bounded(max_term_bytes, work)?;
                if term.is_empty()
                    || postings
                        .last_key_value()
                        .is_some_and(|(prev, _)| term.as_str() <= prev.as_str())
                {
                    return Err(BeaconCheckpointError::Malformed("invalid posting term"));
                }
                let doc_count = cur.usize_checked()?;
                if doc_count == 0 || doc_count > lengths.len() {
                    return Err(BeaconCheckpointError::Malformed(
                        "invalid posting document count",
                    ));
                }
                cur.require_items(doc_count, 32)?;
                let mut doc_map = BTreeMap::new();
                let mut prev_doc_vid: Option<VId> = None;
                for _ in 0..doc_count {
                    work.charge(1)?;
                    let vid = VId(cur.u128()?);
                    if prev_doc_vid.is_some_and(|prev| vid <= prev) {
                        return Err(BeaconCheckpointError::Malformed(
                            "unsorted posting document VIds",
                        ));
                    }
                    prev_doc_vid = Some(vid);
                    let Some(&doc_len) = lengths.get(&vid) else {
                        return Err(BeaconCheckpointError::Malformed(
                            "posting references document absent from segment lengths",
                        ));
                    };
                    let pos_count = cur.usize_checked()?;
                    if pos_count == 0 || (pos_count as u64) > doc_len {
                        return Err(BeaconCheckpointError::Malformed(
                            "invalid segment posting position count",
                        ));
                    }
                    cur.require_items(pos_count, size_of::<u64>())?;
                    work.charge(pos_count)?;
                    let mut positions = decoded_vec(pos_count)?;
                    let mut prev_pos: Option<u64> = None;
                    for _ in 0..pos_count {
                        let pos = cur.u64()?;
                        if pos >= doc_len || prev_pos.is_some_and(|prev| pos <= prev) {
                            return Err(BeaconCheckpointError::Malformed(
                                "invalid segment posting position",
                            ));
                        }
                        prev_pos = Some(pos);
                        positions.push(pos);
                    }
                    let pos_arc = if let Some(live_doc) = next.live.get(&vid)
                        && live_doc.generation == seg_gen
                    {
                        work.charge(term.len().saturating_add(pos_count).max(1))?;
                        let live_pos = live_doc
                            .document
                            .text
                            .as_ref()
                            .and_then(|text| text.frequencies.get(&term))
                            .filter(|expected| expected.as_slice() == positions.as_slice())
                            .ok_or(BeaconCheckpointError::Malformed(
                                "live posting disagrees with analyzed document",
                            ))?;
                        Arc::clone(live_pos)
                    } else {
                        Arc::new(positions)
                    };
                    doc_map.insert(vid, pos_arc);
                }
                postings.insert(term, doc_map);
            }
            // Length equality proves neither term coverage nor phrase/rank
            // equivalence. Every live term must have its exact positional list
            // in this same generation; the per-posting check above rejects
            // additional live terms. Older segments may legitimately retain
            // superseded documents and are not compared to the new live value.
            for &vid in lengths.keys() {
                work.charge(1)?;
                let Some(live_doc) = next.live.get(&vid) else {
                    continue;
                };
                if live_doc.generation != seg_gen {
                    continue;
                }
                let live_text = live_doc.document.text.as_ref().expect("length admitted");
                for (term, expected) in &live_text.frequencies {
                    work.charge(term.len().saturating_add(expected.len()).max(1))?;
                    if postings
                        .get(term)
                        .and_then(|documents| documents.get(&vid))
                        .is_none_or(|positions| positions.as_slice() != expected.as_slice())
                    {
                        return Err(BeaconCheckpointError::Malformed(
                            "live text term missing from its segment generation",
                        ));
                    }
                }
                live_found_in_segment.insert((vid, false));
            }
            next.segments.push(Arc::new(Segment {
                generation: seg_gen,
                vector,
                text: TextSegment { postings, lengths },
            }));
        }

        // Every live document lane must be represented in its named segment generation.
        for (&vid, live) in &next.live {
            work.charge(1)?;
            if live.document.vector.is_some() && !live_found_in_segment.contains(&(vid, true)) {
                return Err(BeaconCheckpointError::Malformed(
                    "live vector document missing from its segment generation",
                ));
            }
            if live.document.text.is_some() && !live_found_in_segment.contains(&(vid, false)) {
                return Err(BeaconCheckpointError::Malformed(
                    "live text document missing from its segment generation",
                ));
            }
        }

        cur.finish()?;
        work.charge(1)?;
        Ok(Self {
            current: Arc::new(next),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{HybridQuery, IndexDocument, IndexMutation, TextMatch, VectorSearch, WorkBudget};

    fn checkpoint_fixture() -> (IndexConfig, BeaconIndex) {
        let cfg = IndexConfig {
            vector: Some(HnswConfig::new(3, DistanceMetric::Cosine)),
            max_segments: 4,
            ..IndexConfig::default()
        };

        let mut work = WorkBudget::new(10_000_000);
        let mut index = BeaconIndex::build(
            cfg.clone(),
            [
                IndexDocument {
                    id: VId(1),
                    vector: Some(vec![1.0, 0.0, 0.0]),
                    text: Some("alpha beta graph".to_owned()),
                },
                IndexDocument {
                    id: VId(2),
                    vector: Some(vec![0.0, 1.0, 0.0]),
                    text: Some("beta delta search".to_owned()),
                },
            ],
            &mut work,
        )
        .unwrap();
        index
            .apply_batch(
                [
                    IndexMutation::Upsert(IndexDocument {
                        id: VId(3),
                        vector: Some(vec![0.7, 0.7, 0.0]),
                        text: Some("alpha hybrid search".to_owned()),
                    }),
                    IndexMutation::Upsert(IndexDocument {
                        id: VId(1),
                        vector: Some(vec![0.9, 0.1, 0.0]),
                        text: Some("alpha updated graph".to_owned()),
                    }),
                ],
                &mut work,
            )
            .unwrap();

        (cfg, index)
    }

    #[test]
    fn beacon_checkpoint_roundtrip_preserves_multi_segment_hybrid_hits_and_rejects_tamper() {
        let (cfg, index) = checkpoint_fixture();
        let mut work = WorkBudget::new(10_000_000);
        let bytes = index.encode_checkpoint(&mut work).unwrap();
        let decoded = BeaconIndex::decode_checkpoint(&bytes, &cfg, &mut work).unwrap();
        assert_eq!(decoded.snapshot().stats(), index.snapshot().stats());

        let query = HybridQuery {
            vector: &[0.8, 0.2, 0.0],
            text: "alpha search",
            k: 3,
            candidates: 3,
            vector_mode: VectorSearch::Approximate { ef_search: 8 },
            text_mode: TextMatch::Any,
            rank_constant: 60.0,
            vector_weight: 0.5,
            text_weight: 0.5,
        };
        assert_eq!(
            decoded
                .snapshot()
                .hybrid_search(query, |_| true, &mut work)
                .unwrap(),
            index
                .snapshot()
                .hybrid_search(query, |_| true, &mut work)
                .unwrap(),
        );

        let mut wrong_cfg = cfg.clone();
        wrong_cfg.max_segments = 2;
        assert_eq!(
            BeaconIndex::decode_checkpoint(&bytes, &wrong_cfg, &mut work).unwrap_err(),
            BeaconCheckpointError::ConfigMismatch,
        );
        assert!(matches!(
            BeaconIndex::decode_checkpoint(&bytes[..bytes.len() - 3], &cfg, &mut work).unwrap_err(),
            BeaconCheckpointError::Malformed(_),
        ));
    }

    fn rejects_changed_generation(name: &str, edit: impl FnOnce(&mut Generation)) {
        let (cfg, mut index) = checkpoint_fixture();
        edit(Arc::get_mut(&mut index.current).unwrap());
        // Use the real encoder so framing, ordering and counts remain valid.
        // Only the relationship between live documents and segments is wrong.
        let bytes = index
            .encode_checkpoint(&mut WorkBudget::new(10_000_000))
            .unwrap();
        assert!(
            matches!(
                BeaconIndex::decode_checkpoint(&bytes, &cfg, &mut WorkBudget::new(10_000_000)),
                Err(BeaconCheckpointError::Malformed(_))
            ),
            "accepted inconsistent checkpoint: {name}",
        );
    }

    #[test]
    fn live_text_postings_must_exactly_represent_the_analyzed_document() {
        rejects_changed_generation("missing term", |generation| {
            Arc::get_mut(&mut generation.segments[1])
                .unwrap()
                .text
                .postings
                .remove("hybrid");
        });
        rejects_changed_generation("missing every term of one live document", |generation| {
            let postings = &mut Arc::get_mut(&mut generation.segments[1])
                .unwrap()
                .text
                .postings;
            for documents in postings.values_mut() {
                documents.remove(&VId(3));
            }
            postings.retain(|_, documents| !documents.is_empty());
        });
        rejects_changed_generation("additional live term", |generation| {
            Arc::get_mut(&mut generation.segments[1])
                .unwrap()
                .text
                .postings
                .insert(
                    "unexpected".to_owned(),
                    BTreeMap::from([(VId(3), Arc::new(vec![0]))]),
                );
        });
        rejects_changed_generation("wrong term frequency", |generation| {
            Arc::get_mut(&mut generation.segments[1])
                .unwrap()
                .text
                .postings
                .get_mut("alpha")
                .unwrap()
                .insert(VId(3), Arc::new(vec![0, 1]));
        });
        rejects_changed_generation("wrong phrase position", |generation| {
            Arc::get_mut(&mut generation.segments[1])
                .unwrap()
                .text
                .postings
                .get_mut("alpha")
                .unwrap()
                .insert(VId(3), Arc::new(vec![1]));
        });
        rejects_changed_generation("two analyzed terms own one token position", |generation| {
            let document = &mut generation.live.get_mut(&VId(3)).unwrap().document;
            Arc::get_mut(document)
                .unwrap()
                .text
                .as_mut()
                .unwrap()
                .frequencies
                .insert("hybrid".to_owned(), Arc::new(vec![0]));
        });
    }

    #[test]
    fn segment_generations_and_live_lane_ownership_cannot_be_forged() {
        rejects_changed_generation("duplicate generation", |generation| {
            generation
                .segments
                .push(Arc::clone(&generation.segments[1]));
        });
        rejects_changed_generation("reversed generations", |generation| {
            generation.segments.swap(0, 1);
        });
        rejects_changed_generation("extra current text lane", |generation| {
            let document = &mut generation.live.get_mut(&VId(3)).unwrap().document;
            Arc::get_mut(document).unwrap().text = None;
        });
        rejects_changed_generation("extra current vector lane", |generation| {
            let document = &mut generation.live.get_mut(&VId(3)).unwrap().document;
            Arc::get_mut(document).unwrap().vector = None;
        });
        rejects_changed_generation("segment newer than live document", |generation| {
            generation.live.get_mut(&VId(1)).unwrap().generation = 1;
        });
    }

    #[test]
    fn stale_segments_restore_without_changing_text_scores_phrases_or_later_updates() {
        let (cfg, mut index) = checkpoint_fixture();
        let mut work = WorkBudget::new(10_000_000);
        let bytes = index.encode_checkpoint(&mut work).unwrap();
        let mut restored = BeaconIndex::decode_checkpoint(&bytes, &cfg, &mut work).unwrap();
        // Vertex 1 has different text/vector values in the older segment.
        // Those authentic stale values must remain legal and invisible.
        for (query, mode, expected_ids) in [
            ("beta", TextMatch::Any, vec![VId(2)]),
            ("updated", TextMatch::Any, vec![VId(1)]),
            ("alpha updated graph", TextMatch::Phrase, vec![VId(1)]),
            ("alpha hybrid search", TextMatch::Phrase, vec![VId(3)]),
            ("alpha beta graph", TextMatch::Phrase, Vec::new()),
        ] {
            let expected = index
                .snapshot()
                .text_search(query, 10, mode, |_| true, &mut work)
                .unwrap();
            assert_eq!(
                expected.iter().map(|hit| hit.id).collect::<Vec<_>>(),
                expected_ids
            );
            assert_eq!(
                restored
                    .snapshot()
                    .text_search(query, 10, mode, |_| true, &mut work)
                    .unwrap(),
                expected,
            );
        }
        assert_eq!(restored.encode_checkpoint(&mut work).unwrap(), bytes);

        let mutations = [
            IndexMutation::Delete(VId(1)),
            IndexMutation::Upsert(IndexDocument {
                id: VId(3),
                vector: None,
                text: Some(String::new()),
            }),
            IndexMutation::Upsert(IndexDocument {
                id: VId(4),
                vector: Some(vec![0.0, 0.0, 1.0]),
                text: Some("beta new graph".to_owned()),
            }),
        ];
        index.apply_batch(mutations.clone(), &mut work).unwrap();
        restored.apply_batch(mutations, &mut work).unwrap();
        index.compact(&mut work).unwrap();
        restored.compact(&mut work).unwrap();
        assert_eq!(restored.snapshot().stats(), index.snapshot().stats());
        assert_eq!(
            restored.encode_checkpoint(&mut work).unwrap(),
            index.encode_checkpoint(&mut work).unwrap(),
        );
        let final_bytes = restored.encode_checkpoint(&mut work).unwrap();
        let final_index = BeaconIndex::decode_checkpoint(&final_bytes, &cfg, &mut work).unwrap();
        assert_eq!(
            final_index
                .snapshot()
                .text_search("beta new graph", 10, TextMatch::Phrase, |_| true, &mut work)
                .unwrap(),
            index
                .snapshot()
                .text_search("beta new graph", 10, TextMatch::Phrase, |_| true, &mut work)
                .unwrap(),
        );
    }

    #[test]
    fn unicode_lowercase_expansion_roundtrips_at_the_exact_source_byte_limit() {
        let cfg = IndexConfig {
            text: Some(Bm25Config {
                max_document_bytes: "İ".len(),
                ..Bm25Config::default()
            }),
            ..IndexConfig::default()
        };
        let mut work = WorkBudget::new(10_000_000);
        let index = BeaconIndex::build(
            cfg.clone(),
            [IndexDocument {
                id: VId(7),
                vector: None,
                text: Some("İ".to_owned()),
            }],
            &mut work,
        )
        .unwrap();
        let bytes = index.encode_checkpoint(&mut work).unwrap();
        let restored = BeaconIndex::decode_checkpoint(&bytes, &cfg, &mut work).unwrap();
        assert_eq!(restored.encode_checkpoint(&mut work).unwrap(), bytes);
        let expected = index
            .snapshot()
            .text_search("İ", 1, TextMatch::Any, |_| true, &mut work)
            .unwrap();
        assert_eq!(expected.len(), 1);
        assert_eq!(expected[0].id, VId(7));
        assert_eq!(
            restored
                .snapshot()
                .text_search("İ", 1, TextMatch::Any, |_| true, &mut work)
                .unwrap(),
            expected,
        );
    }

    #[test]
    fn forged_counts_are_rejected_before_count_sized_allocation() {
        let mut vector_bytes = 65_536_u64.to_le_bytes().to_vec();
        vector_bytes.extend_from_slice(&1_f32.to_le_bytes());
        let error = decode_vector_arc(
            &mut Cursor::new(&vector_bytes),
            &HnswConfig::new(65_536, DistanceMetric::SquaredEuclidean),
            &mut WorkBudget::new(0),
        )
        .unwrap_err();
        assert_eq!(error, BeaconCheckpointError::Malformed("truncated payload"));

        let mut vector_cfg = HnswConfig::new(1, DistanceMetric::SquaredEuclidean).exact_only();
        vector_cfg.max_vectors = usize::MAX;
        let cfg = IndexConfig {
            vector: Some(vector_cfg),
            text: None,
            ..IndexConfig::default()
        };
        let mut work = WorkBudget::new(10_000_000);
        let index = BeaconIndex::build(
            cfg.clone(),
            [IndexDocument {
                id: VId(1),
                vector: None,
                text: None,
            }],
            &mut work,
        )
        .unwrap();
        let mut bytes = index.encode_checkpoint(&mut work).unwrap();
        let mut cursor = Cursor::new(&bytes);
        cursor.take(6).unwrap();
        decode_index_config(&mut cursor).unwrap();
        assert_eq!(cursor.u64().unwrap(), 1); // index sequence
        assert_eq!(cursor.u64().unwrap(), 1); // live document count
        cursor.take(24).unwrap(); // live identity and generation
        assert!(!cursor.bool().unwrap());
        assert!(!cursor.bool().unwrap());
        assert_eq!(cursor.u64().unwrap(), 1); // segment count
        assert_eq!(cursor.u64().unwrap(), 1); // segment generation
        assert!(cursor.bool().unwrap()); // HNSW lane
        assert!(!cursor.bool().unwrap()); // no entry in the empty HNSW
        assert_eq!(cursor.u64().unwrap(), 0); // top level
        let node_count_at = cursor.pos;
        assert_eq!(cursor.u64().unwrap(), 0);
        bytes[node_count_at..node_count_at + 8].copy_from_slice(&(usize::MAX as u64).to_le_bytes());
        assert_eq!(
            BeaconIndex::decode_checkpoint(&bytes, &cfg, &mut work).unwrap_err(),
            BeaconCheckpointError::Malformed("truncated payload"),
        );
    }

    #[test]
    fn every_decode_cancellation_and_truncated_prefix_refuses_without_a_generation() {
        struct Stop {
            calls: usize,
            at: usize,
        }
        impl WorkControl for Stop {
            fn charge(&mut self, _: usize) -> Result<(), BeaconError> {
                self.calls += 1;
                if self.calls == self.at {
                    Err(BeaconError::Cancelled)
                } else {
                    Ok(())
                }
            }
        }
        let (cfg, index) = checkpoint_fixture();
        let mut work = WorkBudget::new(100_000_000);
        let bytes = index.encode_checkpoint(&mut work).unwrap();
        let mut complete = Stop {
            calls: 0,
            at: usize::MAX,
        };
        BeaconIndex::decode_checkpoint(&bytes, &cfg, &mut complete).unwrap();
        assert!(complete.calls > 100);
        for at in 1..=complete.calls {
            let mut stop = Stop { calls: 0, at };
            assert_eq!(
                BeaconIndex::decode_checkpoint(&bytes, &cfg, &mut stop).unwrap_err(),
                BeaconCheckpointError::Beacon(BeaconError::Cancelled),
            );
            assert_eq!(stop.calls, at);
        }
        for end in 0..bytes.len() {
            assert!(
                matches!(
                    BeaconIndex::decode_checkpoint(&bytes[..end], &cfg, &mut work),
                    Err(BeaconCheckpointError::Malformed(_))
                ),
                "accepted truncated checkpoint at {end}",
            );
        }
        assert_eq!(index.encode_checkpoint(&mut work).unwrap(), bytes);
        assert_eq!(
            BeaconIndex::decode_checkpoint(&bytes, &cfg, &mut work)
                .unwrap()
                .encode_checkpoint(&mut work)
                .unwrap(),
            bytes,
        );
    }

    #[test]
    fn bounded_empty_checkpoint_keeps_the_v1_golden_bytes() {
        let cfg = IndexConfig {
            vector: None,
            text: Some(Bm25Config {
                k1: 1.0,
                b: 0.5,
                max_documents: 1,
                max_document_bytes: 2,
                max_document_terms: 3,
                max_query_bytes: 4,
                max_query_terms: 5,
            }),
            max_documents: 6,
            max_batch_operations: 7,
            max_segments: 8,
            max_vector_values: 9,
            max_text_bytes: 10,
            max_document_terms: 11,
        };
        let index = BeaconIndex::new(cfg).unwrap();
        // v1 magic/version, absent vector lane and present text lane; IEEE-754
        // k1/b, fixed resource fields, generation zero and two empty counts.
        let mut expected = b"FGBC\x01\x00\x00\x01".to_vec();
        for word in [
            0x3ff0_0000_0000_0000_u64,
            0x3fe0_0000_0000_0000,
            1,
            2,
            3,
            4,
            5,
            6,
            7,
            8,
            9,
            10,
            11,
            0,
            0,
            0,
        ] {
            expected.extend_from_slice(&word.to_le_bytes());
        }
        assert_eq!(expected.len(), 136);
        assert_eq!(
            index
                .encode_checkpoint_bounded(136, &mut WorkBudget::new(10))
                .unwrap(),
            expected
        );
    }

    #[test]
    fn every_nested_checkpoint_byte_is_bounded_and_failed_append_preserves_prefix() {
        let (cfg, index) = checkpoint_fixture();
        let mut work = WorkBudget::new(100_000_000);
        let bytes = index.encode_checkpoint(&mut work).unwrap();
        let stats = index.snapshot().stats();
        let prefix = b"native-container";
        for bytes_allowed in 0..bytes.len() {
            let mut out = prefix.to_vec();
            let limit = prefix.len() + bytes_allowed;
            assert_eq!(
                index.append_checkpoint(&mut out, limit, &mut work),
                Err(BeaconError::ResourceLimit {
                    resource: "checkpoint bytes",
                    limit
                }),
                "accepted a truncated nested checkpoint at {bytes_allowed}",
            );
            assert_eq!(out, prefix);
            assert!(out.capacity() <= limit);
        }
        let exact = index
            .encode_checkpoint_bounded(bytes.len(), &mut work)
            .unwrap();
        assert_eq!(exact, bytes);
        assert!(exact.capacity() <= bytes.len());
        let mut prefixed = prefix.to_vec();
        assert_eq!(
            index.append_checkpoint(&mut prefixed, prefix.len() + bytes.len(), &mut work),
            Ok(bytes.len())
        );
        assert_eq!(&prefixed[..prefix.len()], prefix);
        assert_eq!(&prefixed[prefix.len()..], bytes);
        let decoded =
            BeaconIndex::decode_checkpoint(&prefixed[prefix.len()..], &cfg, &mut work).unwrap();
        assert_eq!(decoded.snapshot().stats(), stats);
        assert_eq!(index.snapshot().stats(), stats);
        assert_eq!(index.encode_checkpoint(&mut work).unwrap(), bytes);
    }

    #[test]
    fn checkpoint_capacity_growth_is_geometric_and_ceiling_checked_before_growth() {
        let mut out = Vec::new();
        let limit = 65_537;
        let mut writer = CheckpointWriter {
            out: &mut out,
            limit,
        };
        let mut allocations = 0;
        for _ in 0..limit {
            let before = writer.out.capacity();
            writer.byte(0x5a).unwrap();
            allocations += usize::from(writer.out.capacity() != before);
            assert!(writer.out.capacity() <= limit);
        }
        assert!(
            allocations <= 11,
            "per-field exact reservations would be quadratic"
        );
        let capacity = writer.out.capacity();
        assert_eq!(
            writer.byte(0),
            Err(BeaconError::ResourceLimit {
                resource: "checkpoint bytes",
                limit
            })
        );
        assert_eq!(writer.out.capacity(), capacity);
        assert_eq!(writer.out.len(), limit);
        assert!(writer.out.iter().all(|&byte| byte == 0x5a));
    }

    #[test]
    fn every_checkpoint_cancellation_restores_prefix_and_leaves_index_usable() {
        struct Stop {
            calls: usize,
            at: usize,
        }
        impl WorkControl for Stop {
            fn charge(&mut self, _: usize) -> Result<(), BeaconError> {
                self.calls += 1;
                if self.calls == self.at {
                    Err(BeaconError::Cancelled)
                } else {
                    Ok(())
                }
            }
        }
        let (_, index) = checkpoint_fixture();
        let mut count = Stop {
            calls: 0,
            at: usize::MAX,
        };
        let bytes = index.encode_checkpoint(&mut count).unwrap();
        assert!(count.calls > 10);
        for at in 1..=count.calls {
            let mut out = b"prefix".to_vec();
            let mut stop = Stop { calls: 0, at };
            assert_eq!(
                index.append_checkpoint(&mut out, 6 + bytes.len(), &mut stop),
                Err(BeaconError::Cancelled)
            );
            assert_eq!(out, b"prefix");
            assert_eq!(stop.calls, at);
        }
        assert_eq!(
            index
                .encode_checkpoint(&mut WorkBudget::new(10_000_000))
                .unwrap(),
            bytes
        );
    }
}
