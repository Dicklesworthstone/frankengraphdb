//! Query-owned FGEC reads: one frame on the healthy path, one charged stripe
//! and decoder on the repair path. The authenticated header, not the current
//! writer configuration, determines every allocation bound.

use super::*;
use crate::tiered::memory::{MemoryCharge, MemoryError, MemoryPool};
use asupersync::raptorq::systematic::SystematicParams;

#[cfg(test)]
mod tests;

#[derive(Clone, Copy)]
enum Selection {
    Object { limit: u64 },
    Range { offset: u64, len: usize },
}

fn overflow() -> ExtentError {
    ExtentError::Memory(MemoryError::SizeOverflow)
}

/// Conservative capacity bound for the pinned sequential RaptorQ decoder.
/// With L intermediate columns and at most M=L+R equations (including K'
/// padding), all sparse and dense matrices fit M*L cells. Each stripe creates
/// a fresh decoder and calls decode once: its factor cache holds one signature.
///
/// The matrix allowance covers received columns/coefficients, canonical sparse
/// equations, the constraint and HDPC matrices, dense coefficients and their
/// signature/sort scratch, including vector growth. Sixteen M-wide symbol
/// buffers cover received payloads, RHS, the dense RHS snapshot, solved and
/// reconstructed symbols plus transient verification/pivot copies. Per-row
/// slack covers vector headers, hash buckets, queues and column maps. This is
/// requested capacity accounting, as for MemoryPool, not allocator or RSS size.
fn repair_bytes(geom: &ExtentGeometry, stripe: usize) -> Result<usize, ExtentError> {
    let (_, records) = geom.stripe_records_span(stripe);
    let (_, trailer, symbol_bytes) = geom.stripe_trailer_span(stripe);
    let params = SystematicParams::try_for_source_block(geom.stripe_k, symbol_bytes)
        .map_err(|_| ExtentError::InvalidParameters("RaptorQ parameters unavailable"))?;
    let rows = params.l.checked_add(geom.stripe_r).ok_or_else(overflow)?;
    let matrices = rows
        .checked_mul(params.l)
        .and_then(|bytes| bytes.checked_mul(32 * size_of::<usize>()))
        .ok_or_else(overflow)?;
    let symbols = rows
        .checked_mul(symbol_bytes)
        .and_then(|bytes| bytes.checked_mul(16))
        .ok_or_else(overflow)?;
    matrices
        .checked_add(symbols)
        .and_then(|bytes| bytes.checked_add(rows.checked_mul(2048)?))
        // The encoded stripe and reconstructed records coexist with the
        // foundation's source symbols; the trailer is read independently.
        .and_then(|bytes| bytes.checked_add(records.checked_mul(2)?))
        .and_then(|bytes| bytes.checked_add(trailer))
        .ok_or_else(overflow)
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn read_admitted_object<F: VfsFile>(
    cx: &impl StorageReadCx,
    file: F,
    config: &StrataEcsConfig,
    namespace: DatabaseSecurityNamespaceId,
    kind: ExtentObjectKind,
    object_id: ObjectId,
    limit: u64,
    pool: &MemoryPool,
) -> Result<Vec<u8>, Result<ExtentError, std::io::Error>> {
    read(
        cx,
        file,
        config,
        namespace,
        kind,
        object_id,
        Selection::Object { limit },
        pool,
        None,
    )
    .await
}

/// Retain actual plaintext capacity beyond the I/O future. Unlike callers
/// with a pre-reserved output workspace, a run decoder does not know its
/// demand until the authenticated extent header is available.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn read_admitted_object_owned<F: VfsFile>(
    cx: &impl StorageReadCx,
    file: F,
    config: &StrataEcsConfig,
    namespace: DatabaseSecurityNamespaceId,
    kind: ExtentObjectKind,
    object_id: ObjectId,
    limit: u64,
    pool: &MemoryPool,
) -> Result<(Vec<u8>, MemoryCharge), Result<ExtentError, std::io::Error>> {
    let mut charge = pool
        .reserve(cx, 0)
        .map_err(|error| Ok(ExtentError::Memory(error)))?;
    let bytes = read(
        cx,
        file,
        config,
        namespace,
        kind,
        object_id,
        Selection::Object { limit },
        pool,
        Some(&mut charge),
    )
    .await?;
    Ok((bytes, charge))
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn read_admitted_range<F: VfsFile>(
    cx: &impl StorageReadCx,
    file: F,
    config: &StrataEcsConfig,
    namespace: DatabaseSecurityNamespaceId,
    kind: ExtentObjectKind,
    object_id: ObjectId,
    offset: u64,
    len: usize,
    pool: &MemoryPool,
) -> Result<Vec<u8>, Result<ExtentError, std::io::Error>> {
    read(
        cx,
        file,
        config,
        namespace,
        kind,
        object_id,
        Selection::Range { offset, len },
        pool,
        None,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn read<F: VfsFile>(
    cx: &impl StorageReadCx,
    mut file: F,
    config: &StrataEcsConfig,
    namespace: DatabaseSecurityNamespaceId,
    kind: ExtentObjectKind,
    object_id: ObjectId,
    selection: Selection,
    pool: &MemoryPool,
    mut output_charge: Option<&mut MemoryCharge>,
) -> Result<Vec<u8>, Result<ExtentError, std::io::Error>> {
    let coords = coord_transcript(config, namespace, kind, object_id);
    let frame_key = object_subkey("fgdb:strata-ecs:frame-key:v1", config, &coords);
    let nonce_key = object_subkey("fgdb:strata-ecs:frame-nonce:v1", config, &coords);
    let sym_key = object_subkey("fgdb:strata-ecs:sym-key:v1", config, &coords);
    let hdr_key = object_subkey("fgdb:strata-ecs:hdr-key:v1", config, &coords);
    let future = async move {
        let (header, geom) =
            read_validated_header_from_file(&mut file, config, kind, &hdr_key).await?;
        let full = matches!(selection, Selection::Object { .. });
        let (offset, len) = match selection {
            Selection::Object { limit } => {
                if header.plaintext_len > limit {
                    return Err(Ok(ExtentError::PlaintextTooLarge {
                        limit,
                        observed: header.plaintext_len,
                    }));
                }
                (0, geom.plaintext_len)
            }
            Selection::Range { offset, len } => (offset, len),
        };
        let end = offset.checked_add(len as u64).ok_or(Ok(overflow()))?;
        if end > header.plaintext_len {
            return Err(Ok(ExtentError::MalformedHeader));
        }
        if !full && len == 0 {
            return Ok(Vec::new());
        }
        if let Some(charge) = output_charge.as_deref_mut() {
            charge
                .grow(cx, len)
                .map_err(|error| Ok(ExtentError::Memory(error)))?;
        }
        // The caller now owns returned plaintext capacity; this additional
        // charge covers its coexistence with frame plaintext/AEAD scratch and
        // the full object's Merkle leaves until the transfer completes.
        let leaves_count = if full { geom.frame_count } else { 0 };
        let scratch = leaves_count
            .checked_mul(FRAME_DIGEST_BYTES)
            .and_then(|bytes| bytes.checked_add(len))
            .and_then(|bytes| bytes.checked_add(geom.full_record_bytes.checked_mul(4)?))
            .ok_or(Ok(overflow()))?;
        let _scratch = pool
            .reserve(cx, scratch)
            .map_err(|e| Ok(ExtentError::Memory(e)))?;
        let first =
            usize::try_from(offset / geom.frame_bytes as u64).map_err(|_| Ok(overflow()))?;
        let last = if full {
            geom.frame_count - 1
        } else {
            usize::try_from((end - 1) / geom.frame_bytes as u64).map_err(|_| Ok(overflow()))?
        };
        let mut out = Vec::new();
        out.try_reserve_exact(len).map_err(|_| {
            Ok(ExtentError::Memory(MemoryError::AllocationFailed {
                requested: len,
            }))
        })?;
        if out.capacity() > len {
            if let Some(charge) = output_charge.as_deref_mut() {
                charge
                    .grow(cx, out.capacity() - len)
                    .map_err(|error| Ok(ExtentError::Memory(error)))?;
            }
        }
        let mut leaves = Vec::with_capacity(leaves_count);
        for ordinal in first..=last {
            let raw = read_exact_or_short(
                &mut file,
                geom.frame_offset(ordinal) as u64,
                geom.frame_record_len(ordinal),
            )
            .await
            .map_err(Err)?;
            let opened = open_single_frame_record(
                &raw,
                ordinal as u32,
                geom.frame_plain_len(ordinal),
                &coords,
                &frame_key,
                &nonce_key,
            );
            let (digest, plain) = match opened {
                Ok(opened) => opened,
                Err(_) => {
                    let stripe = ordinal / geom.stripe_frames;
                    // Reserve BEFORE reading the trailer or stripe, and before
                    // constructing the foundation decoder's first matrix.
                    let _repair = pool
                        .reserve(cx, repair_bytes(&geom, stripe).map_err(Ok)?)
                        .map_err(|e| Ok(ExtentError::Memory(e)))?;
                    let (record_offset, record_len) = geom.stripe_records_span(stripe);
                    let (trailer_offset, trailer_len, _) = geom.stripe_trailer_span(stripe);
                    let trailer =
                        read_exact_or_short(&mut file, trailer_offset as u64, trailer_len)
                            .await
                            .map_err(Err)?;
                    let records = read_exact_or_short(&mut file, record_offset as u64, record_len)
                        .await
                        .map_err(Err)?;
                    let (recovered, _) = recover_stripe_records(
                        stripe, &geom, &trailer, &records, &coords, &sym_key,
                    )
                    .map_err(Ok)?;
                    let (start, stop) = geom.stripe_frame_range(stripe);
                    let mut cursor = 0;
                    let mut selected = None;
                    for candidate in start..stop {
                        let size = geom.frame_record_len(candidate);
                        let frame = open_single_frame_record(
                            &recovered[cursor..cursor + size],
                            candidate as u32,
                            geom.frame_plain_len(candidate),
                            &coords,
                            &frame_key,
                            &nonce_key,
                        )
                        .map_err(Ok)?;
                        if candidate == ordinal {
                            selected = Some(frame);
                        }
                        cursor += size;
                    }
                    selected.ok_or(Ok(ExtentError::AuthenticationFailed))?
                }
            };
            if full {
                leaves.push(digest);
            }
            let start = (ordinal * geom.frame_bytes) as u64;
            let from = usize::try_from(offset.saturating_sub(start)).map_err(|_| Ok(overflow()))?;
            let to = usize::try_from((end - start).min(plain.len() as u64))
                .map_err(|_| Ok(overflow()))?;
            out.extend_from_slice(&plain[from..to]);
        }
        // ubs:ignore — Merkle root over authenticated public frame digests.
        if full && compute_merkle_root(&leaves) != header.merkle_root {
            return Err(Ok(ExtentError::MerkleMismatch));
        }
        Ok(out)
    };
    // Header probes and the boxed I/O future precede geometry admission. Use
    // the actual future size, including the caller's generic VfsFile state.
    let header_bytes = size_of_val(&future)
        .checked_add(2 * HEADER_BYTES)
        .ok_or(Ok(overflow()))?;
    let _header_charge = pool
        .reserve(cx, header_bytes)
        .map_err(|e| Ok(ExtentError::Memory(e)))?;
    cx.with_restriction_async(future).erase_send().await
}
