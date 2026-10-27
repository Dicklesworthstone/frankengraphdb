//! Hashed maps and sets keyed by engine identities, for the writer's point
//! lookups (fgdb-79kyi).
//!
//! A bulk load asks the writer, for every row, whether its endpoints are live
//! and whether its edge identity is live or spent. Ordered maps answered each
//! question with a descent through millions of keys, in the random order a
//! keyed identity permutation gives: about 15% of a web-google load. These
//! answer it with one probe into a slot table and one read of a dense entry.
//!
//! The layout follows the edge-history validator's index (fgdb-2n2a2,
//! fgdb-yzt2b): a fixed mix of both identity halves (no ambient randomness),
//! linear probing bounded at [`PROBE_LIMIT`] slots, and an ordered overflow
//! for an identity whose probe run is full. A host may write identities of
//! its own choosing, so the worst distribution costs what the ordered map did,
//! never a quadratic scan. Nothing here is ordered: iteration order depends on
//! insertion history, and no caller may depend on it.

use fgdb_types::{EId, VId};
use std::collections::BTreeMap;

/// Probe-run length before an identity goes to the ordered overflow.
const PROBE_LIMIT: usize = 32;
/// A slot that never held an entry. A run ends at the first one.
const EMPTY: u32 = u32::MAX;
/// A slot whose entry was removed. Probes pass over it.
const TOMBSTONE: u32 = u32::MAX - 1;

/// An identity a map can be keyed by: both halves feed the bucket mix.
pub trait IdKey: Copy + Ord {
    fn halves(self) -> (u64, u64);
}

impl IdKey for EId {
    fn halves(self) -> (u64, u64) {
        (self.0 as u64, (self.0 >> 64) as u64)
    }
}

impl IdKey for VId {
    fn halves(self) -> (u64, u64) {
        (self.0 as u64, (self.0 >> 64) as u64)
    }
}

/// The bucket of `key` in a table of `slots` (a power of two) slots: a folded
/// 64x64 multiply of the two halves, each offset by an odd constant so a zero
/// half does not zero the product, keeping the high bits.
fn bucket<K: IdKey>(key: K, slots: usize) -> usize {
    let (low, high) = key.halves();
    let product =
        u128::from(low ^ 0x9e37_79b9_7f4a_7c15) * u128::from(high ^ 0xc2b2_ae3d_27d4_eb4f);
    let mixed = (product as u64) ^ ((product >> 64) as u64);
    (mixed >> (64 - slots.trailing_zeros())) as usize
}

/// Where a key's entry index lives.
enum Place {
    Slot(usize),
    Overflow,
}

/// A map from engine identities to values, hashed with bounded probes.
#[derive(Clone)]
pub(crate) struct IdMap<K, V> {
    /// The entries, dense: removal swaps the last entry into the hole.
    entries: Vec<(K, V)>,
    /// `EMPTY`, `TOMBSTONE`, or an index into `entries`. Zero or a power of
    /// two long, at most three-quarters entries plus tombstones.
    slots: Vec<u32>,
    tombstones: usize,
    /// Keys whose probe run was full when they arrived, with their indices.
    overflow: BTreeMap<K, u32>,
}

impl<K, V> Default for IdMap<K, V> {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            slots: Vec::new(),
            tombstones: 0,
            overflow: BTreeMap::new(),
        }
    }
}

impl<K: IdKey, V> IdMap<K, V> {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Reserve a known number of distinct identities, as a recovered live
    /// row vector supplies. The slot count is exactly the one ordinary
    /// insertion reaches at this length, avoiding intermediate rehashes
    /// without changing the final probe layout or its bounded overflow.
    /// Keep the ordinary entry vector's geometric spare capacity as well,
    /// so the first post-open insertion does not inherit a deferred copy.
    /// Do not infer this count from an iterator that may repeat identities.
    #[allow(
        clippy::reserve_after_initialization,
        reason = "reserve preserves Vec's type-dependent minimum and post-open spare capacity"
    )]
    pub(crate) fn with_capacity(capacity: usize) -> Self {
        if capacity == 0 {
            return Self::new();
        }
        // The ordinary table grows when entries exceed three quarters of
        // its slots. ceil(4 * capacity / 3), expressed without a multiply,
        // chooses the same final power of two at every growth boundary.
        let slots = capacity
            .checked_add(capacity.div_ceil(3))
            .and_then(usize::checked_next_power_of_two)
            .expect("an identity map capacity fits usize")
            .max(64);
        let mut entries = Vec::new();
        entries.reserve(
            capacity
                .checked_next_power_of_two()
                .expect("an identity map capacity fits usize"),
        );
        Self {
            entries,
            slots: vec![EMPTY; slots],
            tombstones: 0,
            overflow: BTreeMap::new(),
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Where `key`'s index is, if the map holds it.
    fn locate(&self, key: K) -> Option<Place> {
        if !self.slots.is_empty() {
            let mask = self.slots.len() - 1;
            let mut at = bucket(key, self.slots.len());
            for _ in 0..PROBE_LIMIT {
                match self.slots[at] {
                    EMPTY => break,
                    TOMBSTONE => {}
                    index if self.entries[index as usize].0 == key => {
                        return Some(Place::Slot(at));
                    }
                    _ => {}
                }
                at = (at + 1) & mask;
            }
        }
        self.overflow.contains_key(&key).then_some(Place::Overflow)
    }

    fn index_at(&self, key: K, place: &Place) -> usize {
        match place {
            Place::Slot(at) => self.slots[*at] as usize,
            Place::Overflow => self.overflow[&key] as usize,
        }
    }

    pub(crate) fn get(&self, key: &K) -> Option<&V> {
        let place = self.locate(*key)?;
        Some(&self.entries[self.index_at(*key, &place)].1)
    }

    pub(crate) fn get_mut(&mut self, key: &K) -> Option<&mut V> {
        let place = self.locate(*key)?;
        let index = self.index_at(*key, &place);
        Some(&mut self.entries[index].1)
    }

    /// The transient dense slot of a held key. Insertion, replacement,
    /// rehashing and cloning preserve slots. Removal alone moves the last
    /// entry into the removed slot; callers keeping links must repair that
    /// one move. A slot is never an engine identity or a durable address.
    pub(crate) fn dense_slot(&self, key: &K) -> Option<u32> {
        let place = self.locate(*key)?;
        Some(self.index_at(*key, &place) as u32)
    }

    /// Borrow a previously resolved dense slot. The caller preserves the
    /// removal law documented on dense_slot.
    pub(crate) fn at_slot(&self, slot: u32) -> (&K, &V) {
        let (key, value) = &self.entries[slot as usize];
        (key, value)
    }

    pub(crate) fn at_slot_mut(&mut self, slot: u32) -> &mut V {
        &mut self.entries[slot as usize].1
    }

    pub(crate) fn contains_key(&self, key: &K) -> bool {
        self.locate(*key).is_some()
    }

    /// Insert `value` under `key`, returning the value it replaces.
    pub(crate) fn insert(&mut self, key: K, value: V) -> Option<V> {
        if let Some(place) = self.locate(key) {
            let index = self.index_at(key, &place);
            return Some(std::mem::replace(&mut self.entries[index].1, value));
        }
        if (self.entries.len() + self.tombstones + 1) * 4 > self.slots.len() * 3 {
            self.rebuild(self.entries.len() + 1);
        }
        let index = u32::try_from(self.entries.len())
            .ok()
            .filter(|index| *index < TOMBSTONE)
            .expect("an identity map holds fewer than u32::MAX - 1 entries");
        self.entries.push((key, value));
        self.place(key, index);
        None
    }

    /// Record `index` for `key`, absent from the table: the first free slot of
    /// its probe run, or the overflow when the run has none.
    fn place(&mut self, key: K, index: u32) {
        let mask = self.slots.len() - 1;
        let mut at = bucket(key, self.slots.len());
        for _ in 0..PROBE_LIMIT {
            match self.slots[at] {
                EMPTY => {
                    self.slots[at] = index;
                    return;
                }
                TOMBSTONE => {
                    self.slots[at] = index;
                    self.tombstones -= 1;
                    return;
                }
                _ => at = (at + 1) & mask,
            }
        }
        self.overflow.insert(key, index);
    }

    /// Re-place every entry in a table sized for `needed` entries, dropping
    /// every tombstone.
    fn rebuild(&mut self, needed: usize) {
        let slots = (needed * 2).next_power_of_two().max(64);
        self.slots = vec![EMPTY; slots];
        self.tombstones = 0;
        self.overflow.clear();
        for index in 0..self.entries.len() {
            let key = self.entries[index].0;
            self.place(key, index as u32);
        }
    }

    pub(crate) fn remove(&mut self, key: &K) -> Option<V> {
        let place = self.locate(*key)?;
        let index = self.index_at(*key, &place);
        match place {
            Place::Slot(at) => {
                self.slots[at] = TOMBSTONE;
                self.tombstones += 1;
            }
            Place::Overflow => {
                self.overflow.remove(key);
            }
        }
        let (_, value) = self.entries.swap_remove(index);
        // The entry that was last now sits at `index`: repoint its place.
        if let Some(&(moved, _)) = self.entries.get(index) {
            let old = self.entries.len() as u32;
            match self.locate_index(moved, old) {
                Some(Place::Slot(at)) => self.slots[at] = index as u32,
                Some(Place::Overflow) => {
                    self.overflow.insert(moved, index as u32);
                }
                None => unreachable!("a held key has a place"),
            }
        }
        Some(value)
    }

    /// Where `key`, whose entry is at `index`, is recorded. Used while an
    /// entry moves, when its recorded index no longer matches `entries`.
    fn locate_index(&self, key: K, index: u32) -> Option<Place> {
        if !self.slots.is_empty() {
            let mask = self.slots.len() - 1;
            let mut at = bucket(key, self.slots.len());
            for _ in 0..PROBE_LIMIT {
                match self.slots[at] {
                    EMPTY => break,
                    found if found == index => return Some(Place::Slot(at)),
                    _ => at = (at + 1) & mask,
                }
            }
        }
        self.overflow.contains_key(&key).then_some(Place::Overflow)
    }

    /// Every entry, in no particular order.
    pub(crate) fn iter(&self) -> impl Iterator<Item = (&K, &V)> {
        self.entries.iter().map(|(key, value)| (key, value))
    }
}

impl<K: IdKey, V> FromIterator<(K, V)> for IdMap<K, V> {
    /// A later pair replaces an earlier one with an equal key, as inserting in
    /// order does.
    fn from_iter<I: IntoIterator<Item = (K, V)>>(pairs: I) -> Self {
        let mut map = Self::new();
        for (key, value) in pairs {
            map.insert(key, value);
        }
        map
    }
}

/// Equal when both hold the same keys with equal values, in any order.
impl<K: IdKey, V: PartialEq> PartialEq for IdMap<K, V> {
    fn eq(&self, other: &Self) -> bool {
        self.len() == other.len()
            && self
                .iter()
                .all(|(key, value)| other.get(key) == Some(value))
    }
}

impl<K: IdKey + std::fmt::Debug, V: std::fmt::Debug> std::fmt::Debug for IdMap<K, V> {
    /// In key order, so equal maps print alike.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut entries: Vec<_> = self.iter().collect();
        entries.sort_by_key(|(key, _)| **key);
        f.debug_map().entries(entries).finish()
    }
}

/// A set of engine identities, hashed with bounded probes.
#[derive(Clone)]
pub struct IdSet<K>(IdMap<K, ()>);

impl<K> Default for IdSet<K> {
    fn default() -> Self {
        Self(IdMap::default())
    }
}

/// Equal when both hold the same identities.
impl<K: IdKey> PartialEq for IdSet<K> {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl<K: IdKey> IdSet<K> {
    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn contains(&self, key: &K) -> bool {
        self.0.contains_key(key)
    }

    /// Insert `key`; `true` when it was absent.
    pub(crate) fn insert(&mut self, key: K) -> bool {
        self.0.insert(key, ()).is_none()
    }
}

impl<K: IdKey> FromIterator<K> for IdSet<K> {
    fn from_iter<I: IntoIterator<Item = K>>(keys: I) -> Self {
        Self(keys.into_iter().map(|key| (key, ())).collect())
    }
}

impl<K: IdKey + std::fmt::Debug> std::fmt::Debug for IdSet<K> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut keys: Vec<_> = self.0.iter().map(|(key, ())| *key).collect();
        keys.sort();
        f.debug_set().entries(keys).finish()
    }
}

#[cfg(test)]
#[path = "id_map_tests.rs"]
mod tests;
