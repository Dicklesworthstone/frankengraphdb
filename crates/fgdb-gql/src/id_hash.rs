//! Execution-local hash maps keyed by engine identities.
//!
//! An evaluation probes maps such as endpoint verdicts and edge counts per
//! vertex once or twice for every admitted edge. Identities are already
//! well-spread 128-bit values, so std's SipHash (two finalization rounds over
//! sixteen bytes) was a third of a grouped one-edge aggregate. This hasher
//! mixes each 64-bit word into a state seeded per map from std's randomly
//! keyed hasher, then finishes with a full avalanche: chosen identities cannot
//! target a seed they never see. These maps are probed and never iterated
//! before their keys are sorted, so no hash order reaches a result.

use core::hash::{BuildHasher, Hasher};

const MULTIPLIER: u64 = 0x9e37_79b9_7f4a_7c15;

/// A per-map seed; `Default` draws a fresh one.
#[derive(Clone, Copy, Debug)]
pub struct IdState(u64);

impl Default for IdState {
    fn default() -> Self {
        Self(std::collections::hash_map::RandomState::new().hash_one(MULTIPLIER))
    }
}

impl BuildHasher for IdState {
    type Hasher = IdHasher;

    fn build_hasher(&self) -> IdHasher {
        IdHasher(self.0)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct IdHasher(u64);

impl IdHasher {
    fn word(&mut self, word: u64) {
        self.0 = (self.0 ^ word).wrapping_mul(MULTIPLIER).rotate_left(29);
    }
}

impl Hasher for IdHasher {
    fn finish(&self) -> u64 {
        // splitmix64's finalizer: every input bit reaches every output bit.
        let mut x = self.0;
        x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        x ^ (x >> 31)
    }

    fn write(&mut self, bytes: &[u8]) {
        for chunk in bytes.chunks(8) {
            let mut word = [0; 8];
            word[..chunk.len()].copy_from_slice(chunk);
            self.word(u64::from_le_bytes(word));
        }
        self.word(bytes.len() as u64);
    }

    fn write_u8(&mut self, value: u8) {
        self.word(u64::from(value));
    }

    fn write_u32(&mut self, value: u32) {
        self.word(u64::from(value));
    }

    fn write_u64(&mut self, value: u64) {
        self.word(value);
    }

    fn write_u128(&mut self, value: u128) {
        self.word(value as u64);
        self.word((value >> 64) as u64);
    }

    fn write_usize(&mut self, value: usize) {
        self.word(value as u64);
    }
}

/// A hash map over identity keys with a fresh seed per map.
pub type IdMap<K, V> = std::collections::HashMap<K, V, IdState>;
/// A hash set over identity keys with a fresh seed per set.
pub type IdSet<K> = std::collections::HashSet<K, IdState>;

#[cfg(test)]
mod tests {
    use super::*;
    use core::hash::Hash;
    use fgdb_types::VId;

    fn hash(state: &IdState, value: impl Hash) -> u64 {
        state.hash_one(value)
    }

    #[test]
    fn identity_keys_spread_and_seeds_differ() {
        let state = IdState::default();
        // Sequential and high-bit-only identities land in distinct low bits,
        // the bits a table uses first.
        let mut low = std::collections::BTreeSet::new();
        for at in 0..4096_u128 {
            low.insert(hash(&state, VId(at)) & 0xfff);
            low.insert(hash(&state, VId(at << 100)) & 0xfff);
        }
        assert!(low.len() > 2_500, "{}", low.len());
        // Equal keys hash equally under one seed; another map's seed differs.
        assert_eq!(
            hash(&state, (3_usize, VId(9))),
            hash(&state, (3_usize, VId(9)))
        );
        assert_ne!(
            hash(&state, (3_usize, VId(9))),
            hash(&state, (9_usize, VId(3)))
        );
        let other = IdState::default();
        assert_ne!(hash(&state, VId(1)), hash(&other, VId(1)));
        let mut map = IdMap::default();
        for at in 0..10_000_u128 {
            map.insert(VId(at.wrapping_mul(0x1_0000_0001)), at);
        }
        assert_eq!(map.len(), 10_000);
        assert_eq!(map[&VId(5_u128.wrapping_mul(0x1_0000_0001))], 5);
    }
}
