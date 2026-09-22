//! Private, per-invocation source replay seals. These are not durable import
//! identities, checkpoint authority, or a proof of an external file's origin.
//! Row framing preserves source order and every field; scalar bytes are owned
//! by the existing canonical encoder. Only digests survive whole-source preflight.

use super::{BulkLoadErrorKind, BulkLoadPolicy, BulkRow};
use fgdb_crypto::{Digest, Hasher};
use fgdb_types::QueryCx;

pub(super) fn checkpoint(cx: &QueryCx) -> Result<(), BulkLoadErrorKind> {
    cx.checkpoint()
        .map_err(|error| BulkLoadErrorKind::Write(super::WriteTxnError::Interrupted(error)))
}

pub(super) fn admit_row(row: &BulkRow) -> Result<(), BulkLoadErrorKind> {
    match row {
        BulkRow::Vertex(vertex) => {
            fgdb_strata::vertex::admit_row_content(&vertex.labels, &vertex.props)
                .map_err(|source| BulkLoadErrorKind::InvalidVertex {
                    key: vertex.key.clone(),
                    source,
                })?;
        }
        BulkRow::Edge(edge) => {
            fgdb_strata::edge_props::admitted_row_bytes(&edge.props)
                .map_err(|source| BulkLoadErrorKind::InvalidEdge {
                    key: edge.key.clone(),
                    source,
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
    hasher: Hasher,
}
impl ChunkHasher {
    pub fn new(start: usize, policy: &BulkLoadPolicy) -> Self {
        let mut hasher = Hasher::new();
        hasher.update(b"fgdb:bulk-source-chunk:v1\0");
        hasher.update(&(start as u64).to_be_bytes());
        hasher.update(&(policy.rows_per_chunk as u64).to_be_bytes());
        hasher.update(&policy.vertex_relation.0.to_be_bytes());
        Self { start, rows: 0, hasher }
    }

    fn bytes(&mut self, cx: &QueryCx, bytes: &[u8]) -> Result<(), BulkLoadErrorKind> {
        self.hasher.update(&(bytes.len() as u64).to_be_bytes());
        for part in bytes.chunks(4096) {
            checkpoint(cx)?;
            self.hasher.update(part);
        }
        Ok(())
    }

    pub fn push(&mut self, cx: &QueryCx, row: &BulkRow) -> Result<(), BulkLoadErrorKind> {
        checkpoint(cx)?;
        let ordinal = self.start.checked_add(self.rows)
            .ok_or(BulkLoadErrorKind::CounterOverflow)?;
        ordinal.checked_add(1).ok_or(BulkLoadErrorKind::CounterOverflow)?;
        let props = match row {
            BulkRow::Vertex(vertex) => {
                self.hasher.update(&[0]);
                self.bytes(cx, vertex.key.as_bytes())?;
                self.hasher.update(&(vertex.labels.len() as u64).to_be_bytes());
                for label in &vertex.labels {
                    checkpoint(cx)?;
                    self.hasher.update(&label.0.to_be_bytes());
                }
                &vertex.props
            }
            BulkRow::Edge(edge) => {
                self.hasher.update(&[1]);
                self.bytes(cx, edge.key.as_bytes())?;
                self.bytes(cx, edge.source.as_bytes())?;
                self.bytes(cx, edge.destination.as_bytes())?;
                self.hasher.update(&edge.relation.0.to_be_bytes());
                &edge.props
            }
        };
        self.hasher.update(&(props.len() as u64).to_be_bytes());
        for (key, value) in props {
            checkpoint(cx)?;
            self.hasher.update(&key.0.to_be_bytes());
            let bytes = value.encode().map_err(|source| BulkLoadErrorKind::SourceEncoding {
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
        ChunkSeal { start: self.start, rows: self.rows, digest: self.hasher.finalize() }
    }
}

pub(super) fn expect_end(
    cx: &QueryCx,
    source: &mut impl Iterator<Item = BulkRow>,
    row: usize,
) -> Result<(), BulkLoadErrorKind> {
    checkpoint(cx)?;
    let extra = source.next();
    checkpoint(cx)?;
    if extra.is_some() {
        return Err(BulkLoadErrorKind::SourceChanged { row });
    }
    Ok(())
}

/// Verify a complete chunk BEFORE returning any rows for ID allocation or
/// preparation. Prefix chunks on resume are also verified, without buffering
/// their payloads. An appended suffix is refused before the final publication.
pub(super) fn read_verified(
    cx: &QueryCx,
    source: &mut impl Iterator<Item = BulkRow>,
    seal: &ChunkSeal,
    policy: &BulkLoadPolicy,
    retain: bool,
    last: bool,
) -> Result<Vec<BulkRow>, BulkLoadErrorKind> {
    let mut actual = ChunkHasher::new(seal.start, policy);
    let mut chunk = Vec::new();
    for at in 0..seal.rows {
        checkpoint(cx)?;
        let row = source.next();
        checkpoint(cx)?;
        let row = row.ok_or(BulkLoadErrorKind::SourceChanged { row: seal.start + at })?;
        // Replay is an untrusted second observation, not an admission bypass.
        admit_row(&row)?;
        actual.push(cx, &row)?;
        if retain {
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
