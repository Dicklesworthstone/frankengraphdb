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

    fn str_bounded(&mut self, max_bytes: usize) -> Result<String, BeaconCheckpointError> {
        let len = self.usize_checked()?;
        if len > max_bytes {
            return Err(BeaconCheckpointError::Malformed("string exceeds limit"));
        }
        let raw = self.take(len)?;
        let text = core::str::from_utf8(raw)
            .map_err(|_| BeaconCheckpointError::Malformed("invalid UTF-8 string"))?;
        Ok(text.to_owned())
    }

    fn finish(self) -> Result<(), BeaconCheckpointError> {
        if self.pos != self.bytes.len() {
            return Err(BeaconCheckpointError::Malformed("trailing bytes"));
        }
        Ok(())
    }
}

fn push_u64_usize(out: &mut Vec<u8>, value: usize) {
    out.extend_from_slice(&(value as u64).to_le_bytes());
}

fn push_str(out: &mut Vec<u8>, value: &str) {
    push_u64_usize(out, value.len());
    out.extend_from_slice(value.as_bytes());
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

fn encode_hnsw_config(out: &mut Vec<u8>, cfg: &HnswConfig) {
    push_u64_usize(out, cfg.dimensions);
    out.push(encode_metric(cfg.metric));
    push_u64_usize(out, cfg.m);
    push_u64_usize(out, cfg.ef_construction);
    out.extend_from_slice(&cfg.seed.to_le_bytes());
    push_u64_usize(out, cfg.max_vectors);
    out.push(u8::from(cfg.links));
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

fn encode_bm25_config(out: &mut Vec<u8>, cfg: &Bm25Config) {
    out.extend_from_slice(&cfg.k1.to_bits().to_le_bytes());
    out.extend_from_slice(&cfg.b.to_bits().to_le_bytes());
    push_u64_usize(out, cfg.max_documents);
    push_u64_usize(out, cfg.max_document_bytes);
    push_u64_usize(out, cfg.max_document_terms);
    push_u64_usize(out, cfg.max_query_bytes);
    push_u64_usize(out, cfg.max_query_terms);
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

fn encode_index_config(out: &mut Vec<u8>, cfg: &IndexConfig) {
    match &cfg.vector {
        Some(v) => {
            out.push(1);
            encode_hnsw_config(out, v);
        }
        None => out.push(0),
    }
    match &cfg.text {
        Some(t) => {
            out.push(1);
            encode_bm25_config(out, t);
        }
        None => out.push(0),
    }
    push_u64_usize(out, cfg.max_documents);
    push_u64_usize(out, cfg.max_batch_operations);
    push_u64_usize(out, cfg.max_segments);
    push_u64_usize(out, cfg.max_vector_values);
    push_u64_usize(out, cfg.max_text_bytes);
    push_u64_usize(out, cfg.max_document_terms);
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
    out: &mut Vec<u8>,
    values: &[f32],
    work: &mut dyn WorkControl,
) -> Result<(), BeaconError> {
    work.charge(values.len().max(1))?;
    push_u64_usize(out, values.len());
    for &v in values {
        out.extend_from_slice(&v.to_bits().to_le_bytes());
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
    work.charge(len)?;
    let mut vals = Vec::with_capacity(len);
    for _ in 0..len {
        vals.push(cur.f32()?);
    }
    let norm = cfg.validate_vector(&vals, work)?;
    Ok((Arc::from(vals), norm))
}

fn encode_analyzed_text(
    out: &mut Vec<u8>,
    text: &AnalyzedText,
    work: &mut dyn WorkControl,
) -> Result<(), BeaconError> {
    work.charge(1)?;
    out.extend_from_slice(&text.length.to_le_bytes());
    push_u64_usize(out, text.source_bytes);
    push_u64_usize(out, text.frequencies.len());
    for (term, positions) in &text.frequencies {
        work.charge(term.len().saturating_add(positions.len()).max(1))?;
        push_str(out, term);
        push_u64_usize(out, positions.len());
        for &pos in positions.iter() {
            out.extend_from_slice(&pos.to_le_bytes());
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
    let term_count = cur.usize_checked()?;
    if term_count > cfg.max_document_terms {
        return Err(BeaconError::ResourceLimit {
            resource: "distinct analyzed terms",
            limit: cfg.max_document_terms,
        }
        .into());
    }
    let mut frequencies = BTreeMap::new();
    let mut prev_term: Option<String> = None;
    let mut total_positions = 0u64;
    for _ in 0..term_count {
        work.charge(1)?;
        let term = cur.str_bounded(source_bytes)?;
        if term.is_empty() {
            return Err(BeaconCheckpointError::Malformed("empty analyzed term"));
        }
        if prev_term
            .as_deref()
            .is_some_and(|prev| term.as_str() <= prev)
        {
            return Err(BeaconCheckpointError::Malformed("unsorted analyzed terms"));
        }
        let pos_count = cur.usize_checked()?;
        if pos_count == 0 || pos_count > source_bytes {
            return Err(BeaconCheckpointError::Malformed(
                "invalid term position count",
            ));
        }
        work.charge(term.len().saturating_add(pos_count))?;
        let mut positions = Vec::with_capacity(pos_count);
        let mut prev_pos: Option<u64> = None;
        for _ in 0..pos_count {
            let pos = cur.u64()?;
            if pos >= length || prev_pos.is_some_and(|prev| pos <= prev) {
                return Err(BeaconCheckpointError::Malformed("invalid token position"));
            }
            prev_pos = Some(pos);
            positions.push(pos);
        }
        total_positions = total_positions
            .checked_add(pos_count as u64)
            .ok_or(BeaconCheckpointError::Malformed("position sum overflow"))?;
        prev_term = Some(term.clone());
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
        let generation = &self.current;
        work.charge(1)?;
        let mut out = Vec::new();
        out.extend_from_slice(&MAGIC);
        out.extend_from_slice(&VERSION_V1.to_le_bytes());
        encode_index_config(&mut out, &generation.config);
        out.extend_from_slice(&generation.sequence.to_le_bytes());

        push_u64_usize(&mut out, generation.live.len());
        for (&vid, live) in &generation.live {
            work.charge(1)?;
            out.extend_from_slice(&vid.0.to_le_bytes());
            out.extend_from_slice(&live.generation.to_le_bytes());
            match &live.document.vector {
                Some(vec) => {
                    out.push(1);
                    encode_vector_slice(&mut out, vec, work)?;
                }
                None => out.push(0),
            }
            match &live.document.text {
                Some(text) => {
                    out.push(1);
                    encode_analyzed_text(&mut out, text, work)?;
                }
                None => out.push(0),
            }
        }

        push_u64_usize(&mut out, generation.segments.len());
        for segment in &generation.segments {
            work.charge(1)?;
            out.extend_from_slice(&segment.generation.to_le_bytes());
            match &segment.vector {
                Some(hnsw) => {
                    out.push(1);
                    match hnsw.entry {
                        Some(entry) => {
                            out.push(1);
                            push_u64_usize(&mut out, entry);
                        }
                        None => out.push(0),
                    }
                    push_u64_usize(&mut out, hnsw.top_level);
                    push_u64_usize(&mut out, hnsw.nodes.len());
                    for node in &hnsw.nodes {
                        work.charge(1)?;
                        out.extend_from_slice(&node.id.0.to_le_bytes());
                        encode_vector_slice(&mut out, &node.vector, work)?;
                        push_u64_usize(&mut out, node.links.len());
                        for layer_links in &node.links {
                            work.charge(layer_links.len().max(1))?;
                            push_u64_usize(&mut out, layer_links.len());
                            for &slot in layer_links {
                                push_u64_usize(&mut out, slot);
                            }
                        }
                    }
                }
                None => out.push(0),
            }

            push_u64_usize(&mut out, segment.text.lengths.len());
            for (&vid, &length) in &segment.text.lengths {
                work.charge(1)?;
                out.extend_from_slice(&vid.0.to_le_bytes());
                out.extend_from_slice(&length.to_le_bytes());
            }
            push_u64_usize(&mut out, segment.text.postings.len());
            for (term, doc_map) in &segment.text.postings {
                work.charge(term.len().max(1))?;
                push_str(&mut out, term);
                push_u64_usize(&mut out, doc_map.len());
                for (&vid, positions) in doc_map {
                    work.charge(positions.len().max(1))?;
                    out.extend_from_slice(&vid.0.to_le_bytes());
                    push_u64_usize(&mut out, positions.len());
                    for &pos in positions.iter() {
                        out.extend_from_slice(&pos.to_le_bytes());
                    }
                }
            }
        }
        Ok(out)
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
        let mut live_found_in_segment = BTreeSet::new();
        for _ in 0..segment_count {
            work.charge(1)?;
            let seg_gen = cur.u64()?;
            if seg_gen > sequence {
                return Err(BeaconCheckpointError::Malformed(
                    "segment generation exceeds index sequence",
                ));
            }
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
                let mut nodes = Vec::with_capacity(node_count);
                let mut prev_node_id: Option<VId> = None;
                for slot in 0..node_count {
                    work.charge(1)?;
                    let id = VId(cur.u128()?);
                    if prev_node_id.is_some_and(|prev| id <= prev) {
                        return Err(BeaconCheckpointError::Malformed("unsorted HNSW node VIds"));
                    }
                    prev_node_id = Some(id);
                    let (mut vec_arc, norm) = decode_vector_arc(&mut cur, vcfg, work)?;
                    if let Some(live_doc) = next.live.get(&id)
                        && live_doc.generation == seg_gen
                        && let Some(live_vec) = &live_doc.document.vector
                        && same_f32_bits(live_vec, &vec_arc)
                    {
                        vec_arc = Arc::clone(live_vec);
                        live_found_in_segment.insert((id, true));
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
                    let mut links = Vec::with_capacity(layer_count);
                    for layer in 0..layer_count {
                        let deg = cur.usize_checked()?;
                        let max_deg = if layer == 0 { 2 * vcfg.m } else { vcfg.m };
                        if deg > max_deg {
                            return Err(BeaconCheckpointError::Malformed(
                                "HNSW layer degree exceeds configured bound",
                            ));
                        }
                        work.charge(deg.max(1))?;
                        let mut layer_links = Vec::with_capacity(deg);
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
                        for (layer, layer_links) in node.links.iter().enumerate() {
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
                if let Some(live_doc) = next.live.get(&vid)
                    && live_doc.generation == seg_gen
                    && let Some(live_text) = &live_doc.document.text
                    && live_text.length == doc_len
                {
                    live_found_in_segment.insert((vid, false));
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
                .map_or(0, |tcfg| tcfg.max_document_bytes);
            let mut postings: BTreeMap<String, BTreeMap<VId, Arc<Vec<u64>>>> = BTreeMap::new();
            let mut prev_term: Option<String> = None;
            for _ in 0..term_count {
                work.charge(1)?;
                let term = cur.str_bounded(max_term_bytes)?;
                if term.is_empty()
                    || prev_term
                        .as_deref()
                        .is_some_and(|prev| term.as_str() <= prev)
                {
                    return Err(BeaconCheckpointError::Malformed("invalid posting term"));
                }
                let doc_count = cur.usize_checked()?;
                if doc_count == 0 || doc_count > lengths.len() {
                    return Err(BeaconCheckpointError::Malformed(
                        "invalid posting document count",
                    ));
                }
                let mut doc_map = BTreeMap::new();
                let mut prev_doc_vid: Option<VId> = None;
                for _ in 0..doc_count {
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
                    work.charge(pos_count)?;
                    let mut positions = Vec::with_capacity(pos_count);
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
                        && let Some(live_text) = &live_doc.document.text
                        && let Some(live_pos) = live_text.frequencies.get(&term)
                        && live_pos.as_slice() == positions.as_slice()
                    {
                        Arc::clone(live_pos)
                    } else {
                        Arc::new(positions)
                    };
                    doc_map.insert(vid, pos_arc);
                }
                prev_term = Some(term.clone());
                postings.insert(term, doc_map);
            }
            next.segments.push(Arc::new(Segment {
                generation: seg_gen,
                vector,
                text: TextSegment { postings, lengths },
            }));
        }

        // Every live document lane must be represented in its named segment generation.
        for (&vid, live) in &next.live {
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

    #[test]
    fn beacon_checkpoint_roundtrip_preserves_multi_segment_hybrid_hits_and_rejects_tamper() {
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
}
