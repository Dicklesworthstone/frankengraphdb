//! Identity to position in a sealed vertex table (fgdb-61p1n).
//!
//! Every authorized vertex test and property read resolves a `VId` in the
//! statement's sorted vertex table; on a 264k-vertex graph a binary search is
//! about 18 cache-missing probes. A table refilled from a cached admission
//! core is the same sorted table in every statement that reuses the core, so
//! one index built from it serves them all.
//!
//! Lookups are always checked against the table: a slot only says where to
//! look. A key set that collides past `PROBE_LIMIT` (identities can be
//! chosen by writers) costs those keys a binary search, never a wrong answer.
//! Lookups charge nothing, as the binary search they replace charged nothing.

use fgdb_types::VId;

/// Probes before a lookup falls back to binary search.
const PROBE_LIMIT: usize = 32;
const EMPTY: u32 = u32::MAX;

#[derive(Debug)]
pub(crate) struct VertexPositions {
    slots: Box<[u32]>,
    mask: usize,
    len: usize,
}

impl VertexPositions {
    /// Index `table`, sorted and unique by identity. `None` when it is too
    /// long to address with a slot.
    pub(crate) fn build<T>(table: &[(VId, T)]) -> Option<Self> {
        let len = u32::try_from(table.len()).ok().filter(|&len| len < EMPTY)?;
        let capacity = (len as usize).max(1).checked_mul(2)?.next_power_of_two();
        let mask = capacity - 1;
        let mut slots = vec![EMPTY; capacity].into_boxed_slice();
        for (position, &(vid, _)) in table.iter().enumerate() {
            let mut at = mix(vid) & mask;
            for _ in 0..PROBE_LIMIT {
                if slots[at] == EMPTY {
                    slots[at] = position as u32;
                    break;
                }
                at = (at + 1) & mask;
            }
            // A key with no free slot in its window is left out: its lookup
            // meets PROBE_LIMIT occupied slots and binary searches instead.
        }
        Some(Self {
            slots,
            mask,
            len: table.len(),
        })
    }

    /// Where `vid` is in `table`, the table this index was built from.
    pub(crate) fn find<T>(&self, table: &[(VId, T)], vid: VId) -> Option<usize> {
        debug_assert_eq!(table.len(), self.len, "an index of a different table");
        let mut at = mix(vid) & self.mask;
        for _ in 0..PROBE_LIMIT {
            let position = self.slots[at];
            if position == EMPTY {
                return None;
            }
            if table[position as usize].0 == vid {
                return Some(position as usize);
            }
            at = (at + 1) & self.mask;
        }
        table.binary_search_by_key(&vid, |&(vid, _)| vid).ok()
    }
}

/// Both halves of the identity, folded with odd multipliers.
fn mix(vid: VId) -> usize {
    let low = vid.0 as u64;
    let high = (vid.0 >> 64) as u64;
    let folded = (low ^ high.rotate_left(29)).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    let mixed = (folded ^ (folded >> 32)).wrapping_mul(0xc2b2_ae3d_27d4_eb4f);
    (mixed ^ (mixed >> 29)) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(ids: impl IntoIterator<Item = u128>) -> Vec<(VId, ())> {
        let mut table: Vec<(VId, ())> = ids.into_iter().map(|id| (VId(id), ())).collect();
        table.sort_unstable_by_key(|&(vid, _)| vid);
        table.dedup_by_key(|&mut (vid, _)| vid);
        table
    }

    /// Every member is found at its position and every non-member is
    /// absent, for dense, sparse, high-half-only and empty identity sets.
    #[test]
    fn every_identity_is_found_at_its_position_and_no_other_is() {
        let sets: Vec<Vec<(VId, ())>> = vec![
            table(0..1_000),
            table((0..1_000).map(|at| at * 7_919 + 3)),
            table((0..500).map(|at| at << 64)),
            table([u128::MAX, 0, 1 << 127]),
            table([]),
        ];
        for set in &sets {
            let positions = VertexPositions::build(set).unwrap();
            for (at, &(vid, _)) in set.iter().enumerate() {
                assert_eq!(positions.find(set, vid), Some(at));
            }
            for missing in [5_000_000_u128, (1 << 100) + 1, 2] {
                let expected = set
                    .binary_search_by_key(&VId(missing), |&(vid, _)| vid)
                    .ok();
                assert_eq!(positions.find(set, VId(missing)), expected);
            }
        }
    }

    /// Identities chosen to share one slot overflow every probe window, and
    /// still resolve exactly, through the binary-search fallback.
    #[test]
    fn colliding_identities_fall_back_to_binary_search_and_stay_exact() {
        // A small table has a small mask; collect identities whose slot is 0.
        let mut ids = Vec::new();
        let mut candidate = 0_u128;
        while ids.len() < 200 {
            if mix(VId(candidate)) & 511 == 0 {
                ids.push(candidate);
            }
            candidate += 1;
        }
        let set = table(ids);
        let positions = VertexPositions::build(&set).unwrap();
        assert_eq!(positions.mask, 511);
        for (at, &(vid, _)) in set.iter().enumerate() {
            assert_eq!(positions.find(&set, vid), Some(at));
        }
        assert_eq!(positions.find(&set, VId(u128::MAX)), None);
    }
}
