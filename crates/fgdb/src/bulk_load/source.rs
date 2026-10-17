//! Private, per-invocation source replay seals. These are not durable import
//! identities, checkpoint authority, or a proof of an external file's origin.
//! Row framing preserves source order and every field; scalar bytes are owned
//! by the existing canonical encoder. Only digests survive whole-source preflight.

use super::{BulkEndpoint, BulkLoadCheckpoint, BulkLoadErrorKind, BulkLoadPolicy, BulkRow};
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
            limit(row, "key_bytes", policy.max_key_bytes, e.key.len())?;
            for endpoint in [&e.source, &e.destination] {
                // A match endpoint's encoded value is bounded like a key.
                let len = match endpoint {
                    BulkEndpoint::Key(key) => key.len(),
                    BulkEndpoint::Match(matched) => matched
                        .value
                        .encode()
                        .map_err(|source| BulkLoadErrorKind::SourceEncoding { row, source })?
                        .len(),
                };
                limit(row, "key_bytes", policy.max_key_bytes, len)?;
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

    pub fn push(&mut self, cx: &QueryCx, row: &BulkRow) -> Result<(), BulkLoadErrorKind> {
        checkpoint(cx)?;
        let ordinal = self
            .start
            .checked_add(self.rows)
            .ok_or(BulkLoadErrorKind::CounterOverflow)?;
        ordinal
            .checked_add(1)
            .ok_or(BulkLoadErrorKind::CounterOverflow)?;
        transcript_row(cx, ordinal, row, &mut |bytes| self.raw(cx, bytes))?;
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

/// Feed `row`'s source-transcript bytes to `emit` in the order a chunk seal
/// hashes and counts them against `max_chunk_bytes`.
fn transcript_row(
    cx: &QueryCx,
    ordinal: usize,
    row: &BulkRow,
    emit: &mut impl FnMut(&[u8]) -> Result<(), BulkLoadErrorKind>,
) -> Result<(), BulkLoadErrorKind> {
    fn framed(
        emit: &mut impl FnMut(&[u8]) -> Result<(), BulkLoadErrorKind>,
        bytes: &[u8],
    ) -> Result<(), BulkLoadErrorKind> {
        emit(&(bytes.len() as u64).to_be_bytes())?;
        emit(bytes)
    }
    let props = match row {
        BulkRow::Vertex(vertex) => {
            emit(&[0])?;
            framed(emit, vertex.key.as_bytes())?;
            emit(&(vertex.labels.len() as u64).to_be_bytes())?;
            for label in &vertex.labels {
                checkpoint(cx)?;
                emit(&label.0.to_be_bytes())?;
            }
            &vertex.props
        }
        BulkRow::Edge(edge) => {
            emit(&[1])?;
            framed(emit, edge.key.as_bytes())?;
            for endpoint in [&edge.source, &edge.destination] {
                match endpoint {
                    // Byte-identical to the key-only framing.
                    BulkEndpoint::Key(key) => framed(emit, key.as_bytes())?,
                    // No key length is u64::MAX, so this sentinel cannot
                    // collide with a framed key.
                    BulkEndpoint::Match(matched) => {
                        emit(&u64::MAX.to_be_bytes())?;
                        match matched.label {
                            None => emit(&[0])?,
                            Some(label) => {
                                emit(&[1])?;
                                emit(&label.0.to_be_bytes())?;
                            }
                        }
                        emit(&matched.key.0.to_be_bytes())?;
                        let bytes = matched.value.encode().map_err(|source| {
                            BulkLoadErrorKind::SourceEncoding {
                                row: ordinal,
                                source,
                            }
                        })?;
                        framed(emit, &bytes)?;
                    }
                }
            }
            emit(&edge.relation.0.to_be_bytes())?;
            &edge.props
        }
    };
    emit(&(props.len() as u64).to_be_bytes())?;
    for (key, value) in props {
        checkpoint(cx)?;
        emit(&key.0.to_be_bytes())?;
        let bytes = value
            .encode()
            .map_err(|source| BulkLoadErrorKind::SourceEncoding {
                row: ordinal,
                source,
            })?;
        framed(emit, &bytes)?;
    }
    Ok(())
}

/// The bytes `row` adds to its chunk's capsule: the canonical `CreateVertex`
/// or `CreateEdge` row its template carries, as fgdb-delta-types encodes it.
/// That is a tag, the identity, birth ordinal (and an edge's endpoints and
/// relation) at fixed widths, a u32 count before the labels (u64 each) and
/// the properties (u64 key, u32-counted scalar encoding each), and absent
/// key and valid-time tags. Sorting can only drop a duplicate label, so this
/// bounds the row from above.
pub(super) fn template_len(ordinal: usize, row: &BulkRow) -> Result<usize, BulkLoadErrorKind> {
    let props = |props: &[(fgdb_delta_types::PropertyKeyId, fgdb_types::CanonicalScalar)]| {
        props.iter().try_fold(4_usize, |len, (_, value)| {
            let encoded = value
                .encode()
                .map_err(|source| BulkLoadErrorKind::SourceEncoding {
                    row: ordinal,
                    source,
                })?;
            Ok(len.saturating_add(8 + 4 + encoded.len()))
        })
    };
    // Tag, identity, ordinal, label count and valid-time tag; an edge adds
    // its endpoints and relation and the canonical-key tag but no labels.
    const VERTEX: usize = 1 + 16 + 8 + 4 + 1;
    const EDGE: usize = 1 + 16 + 8 + 16 + 8 + 16 + 1 + 1;
    Ok(match row {
        BulkRow::Vertex(vertex) => VERTEX
            .saturating_add(vertex.labels.len().saturating_mul(8))
            .saturating_add(props(&vertex.props)?),
        BulkRow::Edge(edge) => EDGE.saturating_add(props(&edge.props)?),
    })
}

/// The source-transcript bytes `row` adds to its chunk.
pub(super) fn transcript_len(
    cx: &QueryCx,
    ordinal: usize,
    row: &BulkRow,
) -> Result<usize, BulkLoadErrorKind> {
    let mut len = 0usize;
    transcript_row(cx, ordinal, row, &mut |bytes| {
        len = len
            .checked_add(bytes.len())
            .ok_or(BulkLoadErrorKind::CounterOverflow)?;
        Ok(())
    })?;
    Ok(len)
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

#[cfg(test)]
mod template_len_tests {
    use super::super::{BulkEdge, BulkVertex};
    use super::{BulkRow, template_len};
    use fgdb_delta_types::{DeltaRow, LabelId, PropertyKeyId, RelationId};
    use fgdb_types::{CanonicalScalar, CanonicalText, EId, VId};

    /// Each row's template bytes are exactly the canonical create row the
    /// writer's template carries for it, labels deduplicated and both
    /// collections sorted as the writer sorts them, over random rows with
    /// every common scalar kind. A row whose labels repeat is bounded from
    /// above: the writer drops the repeat.
    #[test]
    fn template_len_is_each_rows_canonical_create_bytes() {
        let mut state = 0x7e3a_11e0_0000_0001_u64;
        let mut random = move |bound: u64| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state % bound
        };
        let mut checked = 0;
        for at in 0..500_usize {
            let mut props: Vec<(PropertyKeyId, CanonicalScalar)> = (0..random(5))
                .map(|key| {
                    let value = match random(4) {
                        0 => CanonicalScalar::Int(random(1 << 40) as i64 - (1 << 39)),
                        1 => CanonicalScalar::Bool(random(2) == 0),
                        2 => CanonicalScalar::Text(
                            CanonicalText::new_ucs_basic(&"x".repeat(random(300) as usize))
                                .unwrap(),
                        ),
                        _ => CanonicalScalar::Null,
                    };
                    (PropertyKeyId(10 - key), value)
                })
                .collect();
            let (row, canonical) = if at % 2 == 0 {
                let mut labels: Vec<LabelId> = (0..random(4)).map(|l| LabelId(9 - l)).collect();
                let row = BulkRow::Vertex(BulkVertex {
                    key: format!("v{at}"),
                    labels: labels.clone(),
                    props: props.clone(),
                });
                labels.sort_unstable();
                props.sort_by_key(|(key, _)| *key);
                let canonical = DeltaRow::CreateVertex {
                    vid: VId(u128::MAX),
                    birth_ordinal: u64::MAX,
                    labels,
                    props,
                    valid_time: None,
                };
                (row, canonical)
            } else {
                let row = BulkRow::Edge(BulkEdge {
                    key: format!("e{at}"),
                    source: "a".into(),
                    destination: "b".into(),
                    relation: RelationId(random(1 << 20)),
                    props: props.clone(),
                });
                props.sort_by_key(|(key, _)| *key);
                let BulkRow::Edge(edge) = &row else {
                    unreachable!()
                };
                let canonical = DeltaRow::CreateEdge {
                    eid: EId(u128::MAX),
                    birth_ordinal: u64::MAX,
                    src: VId(0),
                    relation: edge.relation,
                    dst: VId(0),
                    canonical_key: None,
                    props,
                    valid_time: None,
                };
                (row, canonical)
            };
            let bytes = canonical.canonical_bytes().unwrap().len();
            assert_eq!(template_len(at, &row).unwrap(), bytes, "{row:?}");
            // A repeated label is counted, though the writer drops it.
            if let BulkRow::Vertex(vertex) = &row
                && let Some(&first) = vertex.labels.first()
            {
                let mut repeated = vertex.clone();
                repeated.labels.push(first);
                assert!(template_len(at, &BulkRow::Vertex(repeated)).unwrap() > bytes);
            }
            checked += 1;
        }
        assert_eq!(checked, 500);
    }
}
