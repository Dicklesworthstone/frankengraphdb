//! The hashed identity map answers exactly as an ordered map under every
//! operation sequence, including identities that all share one bucket.

use super::{IdKey, IdMap, IdSet};
use fgdb_types::{EId, VId};
use std::collections::{BTreeMap, BTreeSet};

/// xorshift64*: deterministic, dependency-free.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound
    }
}

/// An identity whose halves never vary: every key lands in one bucket, so the
/// probe run fills and the overflow carries the rest.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
struct Colliding(u64);

impl IdKey for Colliding {
    fn halves(self) -> (u64, u64) {
        (7, 11)
    }
}

/// Apply `ops` random operations over `keys` distinct keys to both maps,
/// comparing every answer and, every so often, the whole contents.
fn agree<K: IdKey + std::fmt::Debug>(seed: u64, keys: &[K], ops: usize) {
    let mut rng = Rng(seed);
    let mut map = IdMap::new();
    let mut oracle = BTreeMap::new();
    for step in 0..ops {
        let key = keys[rng.below(keys.len() as u64) as usize];
        match rng.below(10) {
            0..=4 => {
                let value = rng.next();
                assert_eq!(
                    map.insert(key, value),
                    oracle.insert(key, value),
                    "step {step}"
                );
            }
            5..=7 => assert_eq!(map.remove(&key), oracle.remove(&key), "step {step}"),
            _ => {
                assert_eq!(map.get(&key), oracle.get(&key), "step {step}");
                assert_eq!(
                    map.contains_key(&key),
                    oracle.contains_key(&key),
                    "step {step}"
                );
            }
        }
        assert_eq!(map.len(), oracle.len(), "step {step}");
        if step % 257 == 0 {
            for probe in keys {
                assert_eq!(map.get(probe), oracle.get(probe), "step {step} {probe:?}");
            }
            let mut held: Vec<_> = map.iter().map(|(key, value)| (*key, *value)).collect();
            held.sort();
            assert_eq!(
                held,
                oracle.iter().map(|(k, v)| (*k, *v)).collect::<Vec<_>>()
            );
        }
    }
}

#[test]
fn an_identity_map_answers_as_an_ordered_map() {
    let mut rng = Rng(0x1_d3a9_0b5e);
    // Spread identities, as the keyed permutation issues them.
    let spread: Vec<EId> = (0..3_000).map(|_| EId(u128::from(rng.next()))).collect();
    agree(1, &spread, 60_000);
    // Structured identities a host might choose: both halves counting.
    let structured: Vec<VId> = (0..2_000u128).map(|n| VId((n << 64) | (n % 7))).collect();
    agree(2, &structured, 60_000);
    // Few keys, many removals and reinsertions: tombstones and rebuilds.
    let few: Vec<EId> = (1..=40u128).map(EId).collect();
    agree(3, &few, 30_000);
}

#[test]
fn identities_sharing_one_bucket_overflow_and_still_answer_exactly() {
    let colliding: Vec<Colliding> = (0..300).map(Colliding).collect();
    agree(4, &colliding, 40_000);
}

#[test]
fn equality_and_collection_ignore_insertion_order() {
    let forward: IdMap<EId, u32> = (0..500u32).map(|n| (EId(u128::from(n) * 977), n)).collect();
    let backward: IdMap<EId, u32> = (0..500u32)
        .rev()
        .map(|n| (EId(u128::from(n) * 977), n))
        .collect();
    assert_eq!(forward, backward);
    // A later pair replaces an earlier one with an equal key.
    let replaced: IdMap<EId, u32> = [(EId(1), 1), (EId(2), 2), (EId(1), 3)]
        .into_iter()
        .collect();
    assert_eq!(replaced.get(&EId(1)), Some(&3));
    assert_eq!(replaced.len(), 2);
    let set: IdSet<VId> = [VId(5), VId(9), VId(5)].into_iter().collect();
    let oracle: BTreeSet<VId> = [VId(5), VId(9)].into_iter().collect();
    assert_eq!(set.len(), oracle.len());
    assert!(set.contains(&VId(9)) && !set.contains(&VId(6)));
}
