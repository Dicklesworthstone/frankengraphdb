//! Stable external radix partitioning without empty-prefix runs.
//!
//! A uniform hash bit cannot separate any groups. Find the first bit that
//! actually varies before opening either child writer, then advance both
//! children past exactly that bit. Skipped bits were equal for EVERY row;
//! selecting a bit from only two sample rows could strand distinct groups.
//! The rule is deterministic, independent of input order and hash skew.

use super::*;

#[derive(Default)]
struct VaryingBits {
    first: Option<[u8; 32]>,
    varying: [u8; 32],
}

impl VaryingBits {
    fn observe(&mut self, hash: [u8; 32]) {
        if let Some(first) = self.first {
            for ((varying, first), next) in self.varying.iter_mut().zip(first).zip(hash) {
                *varying |= first ^ next;
            }
        } else {
            self.first = Some(hash);
        }
    }

    fn first_after(&self, depth: usize) -> Option<usize> {
        (depth..256).find(|&bit| (self.varying[bit / 8] >> (bit % 8)) & 1 != 0)
    }
}

fn key_hash(
    row: &GraphValueRow,
    definition: &SpillAggregateDefinition,
    pool: &MemoryPool,
    work: &mut Work<'_>,
) -> Result<[u8; 32]> {
    if row.len() != definition.input_width() {
        return invalid();
    }
    let mut hash = fgdb_crypto::Hasher::new();
    hash.update(b"fgdb.native-aggregate.partition.v1");
    // The selected values' combined canonical bytes are no larger than the
    // complete row. Reserve one codec workspace before allocating any key.
    let (len, nodes) = account::encoded_shape(row, work.cx)?;
    work.charge(len.checked_add(nodes).ok_or(SpillError::SizeOverflow)?)?;
    let budget = len
        .checked_mul(8)
        .and_then(|bytes| {
            nodes
                .checked_mul(128)
                .and_then(|node_bytes| bytes.checked_add(node_bytes))
        })
        .ok_or(SpillError::SizeOverflow)?;
    let _codec = pool.reserve(work.cx, budget).map_err(SpillError::Memory)?;
    for &key in definition.group_key_columns() {
        let bytes = row.values()[key]
            .canonical_bytes()
            .map_err(NativeSpoolError::Encode)?;
        work.charge(8)?;
        hash.update(&(bytes.len() as u64).to_be_bytes());
        for chunk in bytes.chunks(4096) {
            work.charge(chunk.len())?;
            hash.update(chunk);
        }
    }
    Ok(hash.finalize().0)
}

async fn partition_bit<F: AsyncRead + AsyncWrite + AsyncSeek + Unpin>(
    parent: &Partition,
    source: &mut SpillFile<F>,
    definition: &SpillAggregateDefinition,
    work: &mut Work<'_>,
    resolver: Option<&(dyn CanonicalScalarResolver + Send + Sync)>,
) -> Result<usize> {
    if parent.depth >= 256 {
        return Err(NativeAggregateSpoolError::PartitionDepth);
    }
    let pool = source.memory_pool().clone();
    let _prefix = pool
        .reserve(work.cx, size_of::<VaryingBits>())
        .map_err(SpillError::Memory)?;
    let mut reader = parent.spool.reader(source);
    let mut bits = VaryingBits::default();
    while let Some(bytes) = reader.next_row(work.cx).await? {
        work.charge(bytes.len())?;
        let _decoded = decoded_reservation(&pool, work.cx, bytes.as_ref(), 1)?;
        let row = decode_row(bytes.as_ref(), resolver)?;
        let hash = key_hash(&row, definition, &pool, work)?;
        work.charge(32)?;
        bits.observe(hash);
        if bits.first_after(parent.depth) == Some(parent.depth) {
            // No later row can introduce an earlier eligible split. Both
            // child passes still authenticate/validate the complete source
            // before any split handle can escape, including its unread suffix.
            return Ok(parent.depth);
        }
    }
    if reader.state() != ScanState::Exhausted {
        return Err(NativeSpoolError::IncompleteCursor.into());
    }
    // Identical full hashes cannot be separated by this bounded radix owner.
    // Refuse without manufacturing hundreds of useless runs or losing rows.
    bits.first_after(parent.depth)
        .ok_or(NativeAggregateSpoolError::PartitionDepth)
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn split_partition<A, B>(
    parent: &Partition,
    source: &mut SpillFile<A>,
    destination: &mut SpillFile<B>,
    definition: &SpillAggregateDefinition,
    page_bytes: usize,
    work: &mut Work<'_>,
    resolver: Option<&(dyn CanonicalScalarResolver + Send + Sync)>,
) -> Result<[Partition; 2]>
where
    A: AsyncRead + AsyncWrite + AsyncSeek + Unpin,
    B: AsyncRead + AsyncWrite + AsyncSeek + Unpin,
{
    let bit = partition_bit(parent, source, definition, work, resolver).await?;
    let pool = source.memory_pool().clone();
    let mut children = [None, None];
    // PagedSpillWriter owns an exclusive file borrow. Two bounded read passes
    // avoid an unbounded open-writer/file catalog and preserve source order.
    for selected in [0_u8, 1] {
        let mut reader = parent.spool.reader(source);
        let mut writer = destination.paged_writer(work.cx, page_bytes)?;
        let mut rows = 0_u64;
        while let Some(bytes) = reader.next_row(work.cx).await? {
            work.charge(bytes.len())?;
            let _decoded = decoded_reservation(&pool, work.cx, bytes.as_ref(), 1)?;
            let row = decode_row(bytes.as_ref(), resolver)?;
            let hash = key_hash(&row, definition, &pool, work)?;
            if (hash[bit / 8] >> (bit % 8)) & 1 == selected {
                work.write(&mut writer, bytes.as_ref()).await?;
                rows = rows.checked_add(1).ok_or(SpillError::SizeOverflow)?;
            }
        }
        if reader.state() != ScanState::Exhausted {
            return Err(NativeSpoolError::IncompleteCursor.into());
        }
        // The discovery pass proved each child has a witness. Never accept a
        // changed, incomplete or misrouted source as an empty valid partition.
        if rows == 0 {
            return invalid();
        }
        let run = writer.finish(work.cx).await?;
        let mut child = parent.spool.clone();
        child.run = run;
        child.rows.result_rows = rows;
        children[usize::from(selected)] = Some(child);
    }
    let [Some(left), Some(right)] = children else {
        return invalid();
    };
    if left.row_count().checked_add(right.row_count()) != Some(parent.spool.row_count()) {
        return invalid();
    }
    Ok([left, right].map(|spool| Partition {
        spool,
        in_source: !parent.in_source,
        depth: bit + 1,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn varying(hashes: &[[u8; 32]]) -> VaryingBits {
        let mut bits = VaryingBits::default();
        for &hash in hashes {
            bits.observe(hash);
        }
        bits
    }

    #[test]
    fn split_skips_uniform_bits_and_preserves_the_entire_remaining_hash_domain() {
        assert_eq!(varying(&[]).first_after(0), None);
        assert_eq!(varying(&[[7; 32]; 3]).first_after(0), None);
        for bit in 0..256 {
            let a = [0x69; 32];
            let mut b = a;
            b[bit / 8] ^= 1 << (bit % 8);
            let bits = varying(&[a, b, a]);
            assert_eq!(bits.first_after(0), Some(bit));
            assert_eq!(bits.first_after(bit), Some(bit));
            assert_eq!(bits.first_after(bit + 1), None);
            assert_eq!(bits.first_after(256), None);
        }
    }

    #[test]
    fn a_late_row_can_introduce_an_earlier_split_than_the_first_two_rows() {
        let a = [0; 32];
        let mut b = a;
        b[31] = 0x80;
        let mut c = a;
        c[1] = 1;
        for hashes in [
            [a, b, c],
            [a, c, b],
            [b, a, c],
            [b, c, a],
            [c, a, b],
            [c, b, a],
        ] {
            let bits = varying(&hashes);
            assert_eq!(bits.first_after(0), Some(8));
            assert_eq!(bits.first_after(9), Some(255));
        }
    }

    #[test]
    fn compressed_tree_retains_duplicate_occurrences_without_spending_empty_runs() {
        // Every digest shares 248 bits. The old one-bit-at-a-time split
        // spends 496 empty/nonempty runs before separating the first pair.
        let input: Vec<_> = (0..4)
            .flat_map(|occurrence| {
                (0..64_u8).rev().map(move |key| {
                    let mut hash = [0x5a; 32];
                    hash[31] = key;
                    (hash, occurrence)
                })
            })
            .collect();
        let mut pending = vec![(input.clone(), 0)];
        let mut leaves = std::collections::BTreeMap::new();
        let mut runs = 1;
        while let Some((rows, depth)) = pending.pop() {
            let hashes: Vec<_> = rows.iter().map(|(hash, _)| *hash).collect();
            if let Some(bit) = varying(&hashes).first_after(depth) {
                assert!((depth..256).contains(&bit));
                let (left, right): (Vec<_>, Vec<_>) = rows
                    .into_iter()
                    .partition(|(hash, _)| (hash[bit / 8] >> (bit % 8)) & 1 == 0);
                assert!(!left.is_empty() && !right.is_empty());
                pending.push((left, bit + 1));
                pending.push((right, bit + 1));
                runs += 2;
            } else {
                let key = rows[0].0;
                assert!(rows.iter().all(|(hash, _)| *hash == key));
                assert!(leaves.insert(key, rows).is_none());
            }
        }
        assert_eq!(runs, 2 * 64 - 1);
        assert_eq!(leaves.len(), 64);
        for (hash, actual) in leaves {
            let expected: Vec<_> = input
                .iter()
                .filter(|(key, _)| *key == hash)
                .copied()
                .collect();
            assert_eq!(actual, expected);
        }
    }
}
