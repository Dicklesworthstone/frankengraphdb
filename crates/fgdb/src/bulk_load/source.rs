//! Private, per-invocation source replay seals. These are not durable import
//! identities, checkpoint authority, or a proof of an external file's origin.
//! Row framing preserves source order and every field; scalar bytes are owned
//! by the existing canonical encoder. Only digests survive whole-source preflight.

use super::{BulkLoadCheckpoint, BulkLoadErrorKind, BulkLoadPolicy, BulkRow};
use fgdb_crypto::{Digest, Hasher};
use fgdb_types::QueryCx;

pub(super) fn checkpoint(cx: &QueryCx) -> Result<(), BulkLoadErrorKind> {
    cx.checkpoint()
        .map_err(|error| BulkLoadErrorKind::Write(super::WriteTxnError::Interrupted(error)))
}

pub(super) fn limit(
    row: usize,
    dimension: &'static str,
    maximum: usize,
    observed: usize,
) -> Result<(), BulkLoadErrorKind> {
    if observed > maximum {
        return Err(BulkLoadErrorKind::SourceLimit {
            row,
            dimension,
            limit: maximum,
            observed,
        });
    }
    Ok(())
}

pub(super) fn admit_keys(
    row: usize,
    value: &BulkRow,
    policy: &BulkLoadPolicy,
) -> Result<(), BulkLoadErrorKind> {
    match value {
        BulkRow::Vertex(v) => limit(row, "key_bytes", policy.max_key_bytes, v.key.len())?,
        BulkRow::Edge(e) => {
            for key in [&e.key, &e.source, &e.destination] {
                limit(row, "key_bytes", policy.max_key_bytes, key.len())?;
            }
        }
    }
    Ok(())
}

pub(super) fn admit_checkpoint(
    cx: &QueryCx,
    value: &BulkLoadCheckpoint,
    policy: &BulkLoadPolicy,
) -> Result<(), BulkLoadErrorKind> {
    let count = value
        .vertices
        .len()
        .checked_add(value.edges.len())
        .ok_or(BulkLoadErrorKind::CounterOverflow)?;
    limit(
        0,
        "source_rows",
        policy.max_source_rows,
        count.max(value.next_row),
    )?;
    let mut bytes = 0usize;
    for key in value.vertices.keys().chain(value.edges.keys()) {
        checkpoint(cx)?;
        limit(0, "key_bytes", policy.max_key_bytes, key.len())?;
        bytes = bytes
            .checked_add(key.len())
            .ok_or(BulkLoadErrorKind::CounterOverflow)?;
        limit(0, "total_key_bytes", policy.max_total_key_bytes, bytes)?;
    }
    Ok(())
}

pub(super) fn next_row<E: core::error::Error + Send + Sync + 'static>(
    cx: &QueryCx,
    source: &mut impl Iterator<Item = Result<BulkRow, E>>,
    row: usize,
) -> Result<Option<BulkRow>, BulkLoadErrorKind> {
    checkpoint(cx)?;
    let value = source.next();
    checkpoint(cx)?;
    value
        .transpose()
        .map_err(|source| BulkLoadErrorKind::Source {
            row,
            source: Box::new(source),
        })
}

pub(super) fn admit_row(row: &BulkRow) -> Result<(), BulkLoadErrorKind> {
    match row {
        BulkRow::Vertex(vertex) => {
            fgdb_strata::vertex::admit_row_content(&vertex.labels, &vertex.props).map_err(
                |source| BulkLoadErrorKind::InvalidVertex {
                    key: vertex.key.clone(),
                    source,
                },
            )?;
        }
        BulkRow::Edge(edge) => {
            fgdb_strata::edge_props::admitted_row_bytes(&edge.props).map_err(|source| {
                BulkLoadErrorKind::InvalidEdge {
                    key: edge.key.clone(),
                    source,
                }
            })?;
        }
    }
    Ok(())
}

pub(super) struct ChunkSeal {
    pub start: usize,
    pub rows: usize,
    digest: Digest,
}
impl ChunkSeal {
    pub fn end(&self) -> usize {
        // Constructed only after ChunkHasher::push checked this addition.
        self.start + self.rows
    }
}

pub(super) struct ChunkHasher {
    start: usize,
    rows: usize,
    bytes: usize,
    byte_limit: usize,
    hasher: Hasher,
}
impl ChunkHasher {
    pub fn new(start: usize, policy: &BulkLoadPolicy) -> Self {
        let mut hasher = Hasher::new();
        hasher.update(b"fgdb:bulk-source-chunk:v1\0");
        hasher.update(&(start as u64).to_be_bytes());
        hasher.update(&(policy.rows_per_chunk as u64).to_be_bytes());
        hasher.update(&policy.vertex_relation.0.to_be_bytes());
        Self {
            start,
            rows: 0,
            bytes: 0,
            byte_limit: policy.max_chunk_bytes,
            hasher,
        }
    }

    fn raw(&mut self, cx: &QueryCx, bytes: &[u8]) -> Result<(), BulkLoadErrorKind> {
        let next = self
            .bytes
            .checked_add(bytes.len())
            .ok_or(BulkLoadErrorKind::CounterOverflow)?;
        limit(self.start + self.rows, "chunk_bytes", self.byte_limit, next)?;
        for part in bytes.chunks(4096) {
            checkpoint(cx)?;
            self.hasher.update(part);
        }
        self.bytes = next;
        Ok(())
    }

    fn bytes(&mut self, cx: &QueryCx, bytes: &[u8]) -> Result<(), BulkLoadErrorKind> {
        self.raw(cx, &(bytes.len() as u64).to_be_bytes())?;
        self.raw(cx, bytes)
    }

    pub fn push(&mut self, cx: &QueryCx, row: &BulkRow) -> Result<(), BulkLoadErrorKind> {
        checkpoint(cx)?;
        let ordinal = self
            .start
            .checked_add(self.rows)
            .ok_or(BulkLoadErrorKind::CounterOverflow)?;
        ordinal
            .checked_add(1)
            .ok_or(BulkLoadErrorKind::CounterOverflow)?;
        let props = match row {
            BulkRow::Vertex(vertex) => {
                self.raw(cx, &[0])?;
                self.bytes(cx, vertex.key.as_bytes())?;
                self.raw(cx, &(vertex.labels.len() as u64).to_be_bytes())?;
                for label in &vertex.labels {
                    checkpoint(cx)?;
                    self.raw(cx, &label.0.to_be_bytes())?;
                }
                &vertex.props
            }
            BulkRow::Edge(edge) => {
                self.raw(cx, &[1])?;
                self.bytes(cx, edge.key.as_bytes())?;
                self.bytes(cx, edge.source.as_bytes())?;
                self.bytes(cx, edge.destination.as_bytes())?;
                self.raw(cx, &edge.relation.0.to_be_bytes())?;
                &edge.props
            }
        };
        self.raw(cx, &(props.len() as u64).to_be_bytes())?;
        for (key, value) in props {
            checkpoint(cx)?;
            self.raw(cx, &key.0.to_be_bytes())?;
            let bytes = value
                .encode()
                .map_err(|source| BulkLoadErrorKind::SourceEncoding {
                    row: ordinal,
                    source,
                })?;
            self.bytes(cx, &bytes)?;
        }
        self.rows += 1;
        Ok(())
    }

    pub fn finish(mut self) -> ChunkSeal {
        self.hasher.update(&(self.rows as u64).to_be_bytes());
        ChunkSeal {
            start: self.start,
            rows: self.rows,
            digest: self.hasher.finalize(),
        }
    }
}

pub(super) fn expect_end<E: core::error::Error + Send + Sync + 'static>(
    cx: &QueryCx,
    source: &mut impl Iterator<Item = Result<BulkRow, E>>,
    row: usize,
) -> Result<(), BulkLoadErrorKind> {
    let extra = next_row(cx, source, row)?;
    if extra.is_some() {
        return Err(BulkLoadErrorKind::SourceChanged { row });
    }
    Ok(())
}

/// Verify a complete chunk BEFORE returning any rows for ID allocation or
/// preparation. Prefix chunks on resume are also verified, without buffering
/// their payloads. An appended suffix is refused before the final publication.
pub(super) fn read_verified<E: core::error::Error + Send + Sync + 'static>(
    cx: &QueryCx,
    source: &mut impl Iterator<Item = Result<BulkRow, E>>,
    seal: &ChunkSeal,
    policy: &BulkLoadPolicy,
    retain: bool,
    last: bool,
) -> Result<Vec<BulkRow>, BulkLoadErrorKind> {
    let mut actual = ChunkHasher::new(seal.start, policy);
    let mut chunk = Vec::new();
    for at in 0..seal.rows {
        let row = next_row(cx, source, seal.start + at)?;
        let row = row.ok_or(BulkLoadErrorKind::SourceChanged {
            row: seal.start + at,
        })?;
        // Replay is an untrusted second observation, not an admission bypass.
        admit_keys(seal.start + at, &row, policy)?;
        admit_row(&row)?;
        actual.push(cx, &row)?;
        if retain {
            chunk
                .try_reserve(1)
                .map_err(|_| BulkLoadErrorKind::SourceAllocation {
                    row: seal.start + at,
                })?;
            chunk.push(row);
        }
    }
    if actual.finish().digest != seal.digest {
        return Err(BulkLoadErrorKind::SourceChanged { row: seal.start });
    }
    if last {
        expect_end(cx, source, seal.end())?;
    }
    Ok(chunk)
}
