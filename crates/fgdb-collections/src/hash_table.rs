//! Deterministic, safe scalar open-addressed hash table.
//!
//! This module is the semantic implementation behind the future vectorized
//! hash-table kernel.  Hash policy is explicit, probe order is bounded, and
//! control metadata is stored separately from keys and values.  The latter
//! gives a future ledgered SIMD boundary a single control-byte group to inspect
//! without changing the table's observable semantics.

use core::borrow::Borrow;
use core::fmt;
use core::hash::{Hash, Hasher};
use core::mem;
use core::slice;

/// Number of control bytes inspected by one scalar probe group.
///
/// Future SIMD implementations must preserve the scalar group's bucket order
/// and may only replace the control-byte classification step.
pub const CONTROL_GROUP_WIDTH: usize = 16;

const EMPTY: u8 = 0x80;
const DELETED: u8 = 0xfe;
const MIN_BUCKETS: usize = CONTROL_GROUP_WIDTH;
const LOAD_DENOMINATOR: usize = 8;
const HASH_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const HASH_PRIME: u64 = 0x0000_0100_0000_01b3;

/// Stable seeded scalar hasher used by [`DeterministicHashTable`].
///
/// Integer methods use little-endian bytes explicitly, so a given sequence of
/// `Hasher` calls and seed has the same result on every supported target.
#[derive(Clone, Debug)]
pub struct SeededHasher {
    state: u64,
    byte_len: u64,
}

impl SeededHasher {
    /// Starts a new hash stream with an explicit seed.
    #[must_use]
    pub const fn new(seed: u64) -> Self {
        Self {
            state: HASH_OFFSET ^ mix64(seed ^ 0x9e37_79b9_7f4a_7c15),
            byte_len: 0,
        }
    }
}

impl Hasher for SeededHasher {
    fn finish(&self) -> u64 {
        mix64(self.state ^ self.byte_len.wrapping_mul(0x9e37_79b9_7f4a_7c15))
    }

    fn write(&mut self, bytes: &[u8]) {
        self.byte_len = self.byte_len.wrapping_add(bytes.len() as u64);
        for &byte in bytes {
            self.state ^= u64::from(byte);
            self.state = self.state.wrapping_mul(HASH_PRIME);
        }
    }

    fn write_u8(&mut self, value: u8) {
        self.write(&[value]);
    }

    fn write_u16(&mut self, value: u16) {
        self.write(&value.to_le_bytes());
    }

    fn write_u32(&mut self, value: u32) {
        self.write(&value.to_le_bytes());
    }

    fn write_u64(&mut self, value: u64) {
        self.write(&value.to_le_bytes());
    }

    fn write_u128(&mut self, value: u128) {
        self.write(&value.to_le_bytes());
    }

    fn write_usize(&mut self, value: usize) {
        self.write_u64(value as u64);
    }

    fn write_i8(&mut self, value: i8) {
        self.write(&value.to_le_bytes());
    }

    fn write_i16(&mut self, value: i16) {
        self.write(&value.to_le_bytes());
    }

    fn write_i32(&mut self, value: i32) {
        self.write(&value.to_le_bytes());
    }

    fn write_i64(&mut self, value: i64) {
        self.write(&value.to_le_bytes());
    }

    fn write_i128(&mut self, value: i128) {
        self.write(&value.to_le_bytes());
    }

    fn write_isize(&mut self, value: isize) {
        self.write_i64(value as i64);
    }
}

const fn mix64(mut value: u64) -> u64 {
    value ^= value >> 30;
    value = value.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value ^= value >> 27;
    value = value.wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

/// Failure from checked table allocation or capacity arithmetic.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HashTableError {
    /// The requested logical capacity cannot be represented.
    CapacityOverflow,
    /// The allocator rejected a checked reservation.
    AllocationFailed,
    /// Private control metadata and entry storage disagreed.
    InvariantViolation,
}

impl fmt::Display for HashTableError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CapacityOverflow => {
                formatter.write_str("deterministic hash-table capacity overflow")
            }
            Self::AllocationFailed => {
                formatter.write_str("deterministic hash-table allocation failed")
            }
            Self::InvariantViolation => {
                formatter.write_str("deterministic hash-table internal invariant violation")
            }
        }
    }
}

impl std::error::Error for HashTableError {}

#[derive(Clone, Debug)]
struct Entry<K, V> {
    hash: u64,
    key: K,
    value: V,
}

/// Open-addressed map with explicit hashing seed and deterministic probe order.
///
/// For an identical seed and operation sequence, physical iteration order is
/// identical.  The table never relies on process entropy or randomized global
/// state.
#[derive(Clone, Debug)]
pub struct DeterministicHashTable<K, V> {
    seed: u64,
    controls: Vec<u8>,
    entries: Vec<Option<Entry<K, V>>>,
    len: usize,
    deleted: usize,
}

impl<K, V> DeterministicHashTable<K, V> {
    /// Creates an empty, allocation-free table with an explicit seed.
    #[must_use]
    pub const fn new(seed: u64) -> Self {
        Self {
            seed,
            controls: Vec::new(),
            entries: Vec::new(),
            len: 0,
            deleted: 0,
        }
    }

    /// Creates a table that can hold at least `min_entries` without growth.
    pub fn try_with_capacity(seed: u64, min_entries: usize) -> Result<Self, HashTableError> {
        let buckets = required_bucket_count(min_entries)?;
        Self::allocate(seed, buckets)
    }

    fn allocate(seed: u64, buckets: usize) -> Result<Self, HashTableError> {
        if buckets == 0 {
            return Ok(Self::new(seed));
        }
        if buckets < MIN_BUCKETS
            || !buckets.is_power_of_two()
            || !buckets.is_multiple_of(CONTROL_GROUP_WIDTH)
        {
            return Err(HashTableError::CapacityOverflow);
        }

        let mut controls = Vec::new();
        controls
            .try_reserve_exact(buckets)
            .map_err(|_| HashTableError::AllocationFailed)?;
        controls.resize(buckets, EMPTY);

        let mut entries = Vec::new();
        entries
            .try_reserve_exact(buckets)
            .map_err(|_| HashTableError::AllocationFailed)?;
        entries.resize_with(buckets, || None);

        Ok(Self {
            seed,
            controls,
            entries,
            len: 0,
            deleted: 0,
        })
    }

    /// Hash seed governing physical placement.
    #[must_use]
    pub const fn seed(&self) -> u64 {
        self.seed
    }

    /// Number of live entries.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Whether the table contains no live entries.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Number of live entries that fit before logical-capacity growth.
    #[must_use]
    pub fn capacity(&self) -> usize {
        max_live_entries(self.bucket_count())
    }

    /// Number of physical open-addressing buckets.
    #[must_use]
    pub fn bucket_count(&self) -> usize {
        self.controls.len()
    }

    /// Reserves room for at least `additional` more live entries.
    pub fn try_reserve(&mut self, additional: usize) -> Result<(), HashTableError> {
        let required_entries = self
            .len
            .checked_add(additional)
            .ok_or(HashTableError::CapacityOverflow)?;
        let required_buckets = required_bucket_count(required_entries)?;
        if required_buckets > self.bucket_count() {
            self.rebuild(required_buckets)?;
        }
        Ok(())
    }

    /// Rebuilds into the smallest table that can hold both the current entries
    /// and `min_entries`, removing all tombstones.
    pub fn try_rehash(&mut self, min_entries: usize) -> Result<(), HashTableError> {
        let required_entries = self.len.max(min_entries);
        let required_buckets = required_bucket_count(required_entries)?;
        self.rebuild(required_buckets)
    }

    /// Removes every entry while retaining the allocation and seed.
    pub fn clear(&mut self) {
        for slot in &mut self.entries {
            *slot = None;
        }
        self.controls.fill(EMPTY);
        self.len = 0;
        self.deleted = 0;
    }

    /// Iterates live entries in deterministic physical-bucket order.
    #[must_use]
    pub fn iter(&self) -> Iter<'_, K, V> {
        Iter {
            slots: self.entries.iter(),
            remaining: self.len,
        }
    }

    fn hash<Q: Hash + ?Sized>(&self, key: &Q) -> u64 {
        let mut hasher = SeededHasher::new(self.seed);
        key.hash(&mut hasher);
        hasher.finish()
    }

    fn rebuild(&mut self, buckets: usize) -> Result<(), HashTableError> {
        if !self.storage_is_consistent() {
            return Err(HashTableError::InvariantViolation);
        }
        if buckets == 0 && self.len != 0 {
            return Err(HashTableError::CapacityOverflow);
        }
        if max_live_entries(buckets) < self.len {
            return Err(HashTableError::CapacityOverflow);
        }

        let mut replacement = Self::allocate(self.seed, buckets)?;
        let mut planned_indexes = Vec::new();
        planned_indexes
            .try_reserve_exact(self.len)
            .map_err(|_| HashTableError::AllocationFailed)?;
        for entry in self.entries.iter().flatten() {
            let index = first_empty_bucket(&replacement.controls, entry.hash)
                .ok_or(HashTableError::InvariantViolation)?;
            replacement.controls[index] = hash_tag(entry.hash);
            planned_indexes.push(index);
        }
        if planned_indexes.len() != self.len {
            return Err(HashTableError::InvariantViolation);
        }

        let old = mem::replace(self, replacement);
        for (entry, index) in old.entries.into_iter().flatten().zip(planned_indexes) {
            self.entries[index] = Some(entry);
            self.len += 1;
        }
        Ok(())
    }

    fn storage_is_consistent(&self) -> bool {
        if self.controls.len() != self.entries.len() {
            return false;
        }
        let mut live = 0_usize;
        let mut deleted = 0_usize;
        for (&control, slot) in self.controls.iter().zip(&self.entries) {
            match (control, slot) {
                (EMPTY, None) => {}
                (DELETED, None) => deleted += 1,
                (tag, Some(entry)) if tag < EMPTY && tag == hash_tag(entry.hash) => live += 1,
                _ => return false,
            }
        }
        live == self.len && deleted == self.deleted
    }
}

impl<K: Hash + Eq, V> DeterministicHashTable<K, V> {
    /// Inserts `key` and `value`, returning the replaced value when present.
    ///
    /// Growth and tombstone-cleanup allocation are checked and reported rather
    /// than delegated to an infallible `Vec` growth operation.
    pub fn insert(&mut self, key: K, value: V) -> Result<Option<V>, HashTableError> {
        if self.bucket_count() == 0 {
            self.rebuild(MIN_BUCKETS)?;
        }

        let hash = self.hash(&key);
        let mut location = self.locate_for_insert(&key, hash)?;
        match location {
            InsertLocation::Existing(index) => {
                let Some(entry) = self.entries[index].as_mut() else {
                    return Err(HashTableError::InvariantViolation);
                };
                return Ok(Some(mem::replace(&mut entry.value, value)));
            }
            InsertLocation::Vacant(_) | InsertLocation::Saturated => {}
        }

        let additional_control = match location {
            InsertLocation::Vacant(index) => usize::from(self.controls[index] == EMPTY),
            InsertLocation::Saturated => 1,
            InsertLocation::Existing(_) => 0,
        };
        let occupied_after_insert = self
            .len
            .checked_add(self.deleted)
            .and_then(|used| used.checked_add(additional_control))
            .ok_or(HashTableError::CapacityOverflow)?;
        if occupied_after_insert > max_live_entries(self.bucket_count()) {
            let target = if self
                .len
                .checked_add(1)
                .ok_or(HashTableError::CapacityOverflow)?
                > max_live_entries(self.bucket_count())
            {
                required_bucket_count(
                    self.len
                        .checked_add(1)
                        .ok_or(HashTableError::CapacityOverflow)?,
                )?
            } else {
                self.bucket_count()
            };
            self.rebuild(target)?;
            location = self.locate_for_insert(&key, hash)?;
        }

        let index = match location {
            InsertLocation::Existing(index) => {
                let Some(entry) = self.entries[index].as_mut() else {
                    return Err(HashTableError::InvariantViolation);
                };
                return Ok(Some(mem::replace(&mut entry.value, value)));
            }
            InsertLocation::Vacant(index) => index,
            InsertLocation::Saturated => {
                let target = self
                    .bucket_count()
                    .checked_mul(2)
                    .ok_or(HashTableError::CapacityOverflow)?;
                self.rebuild(target)?;
                match self.locate_for_insert(&key, hash)? {
                    InsertLocation::Vacant(index) => index,
                    InsertLocation::Existing(index) => {
                        let Some(entry) = self.entries[index].as_mut() else {
                            return Err(HashTableError::InvariantViolation);
                        };
                        return Ok(Some(mem::replace(&mut entry.value, value)));
                    }
                    InsertLocation::Saturated => return Err(HashTableError::InvariantViolation),
                }
            }
        };

        if self.controls[index] == DELETED {
            self.deleted -= 1;
        }
        self.controls[index] = hash_tag(hash);
        self.entries[index] = Some(Entry { hash, key, value });
        self.len += 1;
        Ok(None)
    }

    fn locate_for_insert(&self, key: &K, hash: u64) -> Result<InsertLocation, HashTableError> {
        let tag = hash_tag(hash);
        let mut first_deleted = None;
        let mut probe = GroupProbe::new(hash, self.bucket_count());
        while let Some(group_start) = probe.next_group() {
            for lane in 0..CONTROL_GROUP_WIDTH {
                let index = (group_start + lane) & (self.bucket_count() - 1);
                match self.controls[index] {
                    EMPTY => {
                        if self.entries[index].is_some() {
                            return Err(HashTableError::InvariantViolation);
                        }
                        return Ok(InsertLocation::Vacant(first_deleted.unwrap_or(index)));
                    }
                    DELETED => {
                        if self.entries[index].is_some() {
                            return Err(HashTableError::InvariantViolation);
                        }
                        if first_deleted.is_none() {
                            first_deleted = Some(index);
                        }
                    }
                    control if control == tag => {
                        let Some(entry) = self.entries[index].as_ref() else {
                            return Err(HashTableError::InvariantViolation);
                        };
                        if entry.hash == hash && entry.key == *key {
                            return Ok(InsertLocation::Existing(index));
                        }
                    }
                    _ => {
                        if self.entries[index].is_none() {
                            return Err(HashTableError::InvariantViolation);
                        }
                    }
                }
            }
        }
        Ok(first_deleted.map_or(InsertLocation::Saturated, InsertLocation::Vacant))
    }
}

impl<K, V> DeterministicHashTable<K, V> {
    /// Returns a shared value reference for a borrowed form of the key.
    #[must_use]
    pub fn get<Q>(&self, key: &Q) -> Option<&V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.find_index(key)
            .and_then(|index| self.entries[index].as_ref().map(|entry| &entry.value))
    }

    /// Returns a mutable value reference for a borrowed form of the key.
    #[must_use]
    pub fn get_mut<Q>(&mut self, key: &Q) -> Option<&mut V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        let index = self.find_index(key)?;
        self.entries[index].as_mut().map(|entry| &mut entry.value)
    }

    /// Whether the table contains a borrowed form of the key.
    #[must_use]
    pub fn contains_key<Q>(&self, key: &Q) -> bool
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.find_index(key).is_some()
    }

    /// Removes a key and returns its value.
    pub fn remove<Q>(&mut self, key: &Q) -> Option<V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        let index = self.find_index(key)?;
        let entry = self.entries[index].take()?;
        self.controls[index] = DELETED;
        self.len -= 1;
        self.deleted += 1;
        if self.len == 0 {
            self.controls.fill(EMPTY);
            self.deleted = 0;
        }
        Some(entry.value)
    }

    fn find_index<Q>(&self, key: &Q) -> Option<usize>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        if self.bucket_count() == 0 {
            return None;
        }
        let hash = self.hash(key);
        let tag = hash_tag(hash);
        let mut probe = GroupProbe::new(hash, self.bucket_count());
        while let Some(group_start) = probe.next_group() {
            for lane in 0..CONTROL_GROUP_WIDTH {
                let index = (group_start + lane) & (self.bucket_count() - 1);
                match self.controls[index] {
                    EMPTY => return None,
                    DELETED => {
                        if self.entries[index].is_some() {
                            return None;
                        }
                    }
                    control if control == tag => {
                        let entry = self.entries[index].as_ref()?;
                        if entry.hash == hash && entry.key.borrow() == key {
                            return Some(index);
                        }
                    }
                    _ => {
                        self.entries[index].as_ref()?;
                    }
                }
            }
        }
        None
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InsertLocation {
    Existing(usize),
    Vacant(usize),
    Saturated,
}

#[derive(Clone, Copy, Debug)]
struct GroupProbe {
    next_bucket: usize,
    groups_left: usize,
    mask: usize,
}

impl GroupProbe {
    fn new(hash: u64, buckets: usize) -> Self {
        debug_assert!(buckets >= MIN_BUCKETS);
        debug_assert!(buckets.is_power_of_two());
        Self {
            next_bucket: hash as usize & (buckets - 1),
            groups_left: buckets / CONTROL_GROUP_WIDTH,
            mask: buckets - 1,
        }
    }

    fn next_group(&mut self) -> Option<usize> {
        if self.groups_left == 0 {
            return None;
        }
        let current = self.next_bucket;
        self.next_bucket = (self.next_bucket + CONTROL_GROUP_WIDTH) & self.mask;
        self.groups_left -= 1;
        Some(current)
    }
}

const fn hash_tag(hash: u64) -> u8 {
    ((hash >> 57) & 0x7f) as u8
}

fn first_empty_bucket(controls: &[u8], hash: u64) -> Option<usize> {
    if controls.is_empty() {
        return None;
    }
    let mut probe = GroupProbe::new(hash, controls.len());
    while let Some(group_start) = probe.next_group() {
        for lane in 0..CONTROL_GROUP_WIDTH {
            let index = (group_start + lane) & (controls.len() - 1);
            if controls[index] == EMPTY {
                return Some(index);
            }
        }
    }
    None
}

const fn max_live_entries(buckets: usize) -> usize {
    buckets - (buckets / LOAD_DENOMINATOR)
}

fn required_bucket_count(entries: usize) -> Result<usize, HashTableError> {
    if entries == 0 {
        return Ok(0);
    }
    let mut buckets = MIN_BUCKETS;
    while max_live_entries(buckets) < entries {
        buckets = buckets
            .checked_mul(2)
            .ok_or(HashTableError::CapacityOverflow)?;
    }
    Ok(buckets)
}

/// Iterator over live entries in physical-bucket order.
#[derive(Clone, Debug)]
pub struct Iter<'a, K, V> {
    slots: slice::Iter<'a, Option<Entry<K, V>>>,
    remaining: usize,
}

impl<'a, K, V> Iterator for Iter<'a, K, V> {
    type Item = (&'a K, &'a V);

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(entry) = self.slots.by_ref().flatten().next() {
            self.remaining -= 1;
            return Some((&entry.key, &entry.value));
        }
        None
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

impl<K, V> ExactSizeIterator for Iter<'_, K, V> {
    fn len(&self) -> usize {
        self.remaining
    }
}

impl<'a, K, V> IntoIterator for &'a DeterministicHashTable<K, V> {
    type Item = (&'a K, &'a V);
    type IntoIter = Iter<'a, K, V>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CONTROL_GROUP_WIDTH, DeterministicHashTable, EMPTY, GroupProbe, HashTableError,
        MIN_BUCKETS, SeededHasher,
    };
    use core::hash::{Hash, Hasher};
    use std::collections::{BTreeMap, HashMap};

    #[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
    struct CollisionKey(u32);

    impl Hash for CollisionKey {
        fn hash<H: Hasher>(&self, state: &mut H) {
            0x5au8.hash(state);
        }
    }

    fn physical_index<K: Eq, V>(table: &DeterministicHashTable<K, V>, key: &K) -> Option<usize> {
        table
            .entries
            .iter()
            .position(|slot| slot.as_ref().is_some_and(|entry| entry.key == *key))
    }

    fn sorted_table(table: &DeterministicHashTable<u64, i64>) -> Vec<(u64, i64)> {
        let mut rows: Vec<_> = table.iter().map(|(&key, &value)| (key, value)).collect();
        rows.sort_unstable();
        rows
    }

    fn sorted_hash_map(map: &HashMap<u64, i64>) -> Vec<(u64, i64)> {
        let mut rows: Vec<_> = map.iter().map(|(&key, &value)| (key, value)).collect();
        rows.sort_unstable();
        rows
    }

    #[test]
    fn seeded_hasher_is_segment_stable_and_little_endian() {
        let mut integer = SeededHasher::new(41);
        integer.write_u32(0x0102_0304);

        let mut bytes = SeededHasher::new(41);
        bytes.write(&[0x04, 0x03]);
        bytes.write(&[0x02, 0x01]);

        assert_eq!(integer.finish(), bytes.finish());
        assert_ne!(
            SeededHasher::new(40).finish(),
            SeededHasher::new(41).finish()
        );
    }

    #[test]
    fn group_probe_is_bounded_and_visits_each_bucket_once() {
        for buckets in [16, 32, 64, 256] {
            for start in 0..buckets as u64 {
                let mut seen = vec![false; buckets];
                let mut groups = 0;
                let mut probe = GroupProbe::new(start, buckets);
                while let Some(group_start) = probe.next_group() {
                    groups += 1;
                    for lane in 0..CONTROL_GROUP_WIDTH {
                        let index = (group_start + lane) & (buckets - 1);
                        assert!(!seen[index]);
                        seen[index] = true;
                    }
                }
                assert_eq!(groups, buckets / CONTROL_GROUP_WIDTH);
                assert!(seen.into_iter().all(core::convert::identity));
                assert_eq!(probe.next_group(), None);
            }
        }
    }

    #[test]
    fn insert_replace_lookup_mutate_and_remove() {
        let mut table = DeterministicHashTable::new(17);
        assert_eq!(table.insert("alpha".to_owned(), 10), Ok(None));
        assert_eq!(table.insert("beta".to_owned(), 20), Ok(None));
        assert_eq!(table.insert("alpha".to_owned(), 11), Ok(Some(10)));
        assert_eq!(table.len(), 2);
        assert_eq!(table.get("alpha"), Some(&11));
        assert_eq!(table.get("missing"), None);
        assert!(table.contains_key("beta"));

        if let Some(value) = table.get_mut("beta") {
            *value = 23;
        }
        assert_eq!(table.get("beta"), Some(&23));
        assert_eq!(table.remove("alpha"), Some(11));
        assert_eq!(table.remove("alpha"), None);
        assert_eq!(table.len(), 1);

        table.clear();
        assert!(table.is_empty());
        assert_eq!(table.bucket_count(), MIN_BUCKETS);
        assert!(table.controls.iter().all(|&control| control == EMPTY));
    }

    #[test]
    fn collision_chain_survives_tombstones_and_reuses_the_first_one() {
        let mut table = DeterministicHashTable::new(7);
        for key in 0..14 {
            assert_eq!(table.insert(CollisionKey(key), key * 10), Ok(None));
        }
        assert_eq!(table.bucket_count(), 16);

        let first_hole = physical_index(&table, &CollisionKey(1));
        let second_hole = physical_index(&table, &CollisionKey(4));
        assert!(first_hole.is_some());
        assert!(second_hole.is_some());
        assert_eq!(table.remove(&CollisionKey(1)), Some(10));
        assert_eq!(table.remove(&CollisionKey(4)), Some(40));
        assert_eq!(table.deleted, 2);

        for key in 0..14 {
            if key != 1 && key != 4 {
                assert_eq!(table.get(&CollisionKey(key)), Some(&(key * 10)));
            }
        }
        assert_eq!(table.get(&CollisionKey(1)), None);

        assert_eq!(table.insert(CollisionKey(100), 1000), Ok(None));
        assert_eq!(table.bucket_count(), 16);
        assert_eq!(physical_index(&table, &CollisionKey(100)), first_hole);
        assert_eq!(table.deleted, 1);
        assert_eq!(table.insert(CollisionKey(101), 1010), Ok(None));
        assert_eq!(table.bucket_count(), 16);
        assert_eq!(physical_index(&table, &CollisionKey(101)), second_hole);
        assert_eq!(table.deleted, 0);
    }

    #[test]
    fn probing_wraps_across_the_last_bucket() {
        let mut table = DeterministicHashTable::new(91);

        let mut keys = Vec::new();
        let mut candidate = 0_u64;
        while keys.len() < 8 {
            if table.hash(&candidate) as usize & 15 == 14 {
                keys.push(candidate);
            }
            candidate += 1;
        }

        for (ordinal, &key) in keys.iter().enumerate() {
            assert_eq!(table.insert(key, ordinal), Ok(None));
        }
        assert_eq!(table.bucket_count(), 16);
        let positions: Vec<_> = keys.iter().map(|key| physical_index(&table, key)).collect();
        assert_eq!(
            positions,
            vec![
                Some(14),
                Some(15),
                Some(0),
                Some(1),
                Some(2),
                Some(3),
                Some(4),
                Some(5)
            ]
        );
        for (ordinal, key) in keys.iter().enumerate() {
            assert_eq!(table.get(key), Some(&ordinal));
        }
    }

    #[test]
    fn growth_is_checked_and_replacement_does_not_grow() {
        let mut table = DeterministicHashTable::new(23);
        for key in 0_u64..14 {
            assert_eq!(table.insert(key, key), Ok(None));
        }
        assert_eq!(table.bucket_count(), 16);
        assert_eq!(table.capacity(), 14);

        assert_eq!(table.insert(3, 300), Ok(Some(3)));
        assert_eq!(table.bucket_count(), 16);
        assert_eq!(table.insert(14, 14), Ok(None));
        assert_eq!(table.bucket_count(), 32);

        assert_eq!(table.try_reserve(1_000), Ok(()));
        assert!(table.capacity() >= table.len() + 1_000);
        for key in 0_u64..15 {
            assert_eq!(table.get(&key), Some(&(if key == 3 { 300 } else { key })));
        }

        assert_eq!(
            table.try_reserve(usize::MAX),
            Err(HashTableError::CapacityOverflow)
        );
        assert!(matches!(
            DeterministicHashTable::<u8, u8>::try_with_capacity(0, usize::MAX),
            Err(HashTableError::CapacityOverflow)
        ));
    }

    #[test]
    fn rehash_clears_tombstones_without_semantic_drift() {
        let mut table = DeterministicHashTable::<u64, i64>::new(5);
        for key in 0_u64..100 {
            assert_eq!(table.insert(key, key as i64 * 3), Ok(None));
        }
        for key in (0_u64..100).step_by(2) {
            assert_eq!(table.remove(&key), Some(key as i64 * 3));
        }
        assert!(table.deleted > 0);

        let before = sorted_table(&table);
        assert_eq!(table.try_rehash(table.len()), Ok(()));
        assert_eq!(table.deleted, 0);
        assert_eq!(sorted_table(&table), before);
    }

    #[test]
    fn full_hash_collisions_survive_growth_and_deletion_churn() {
        let mut table = DeterministicHashTable::new(0x77);
        for key in 0..300 {
            assert_eq!(table.insert(CollisionKey(key), key), Ok(None));
        }
        assert!(table.bucket_count() > MIN_BUCKETS);

        for key in (0..300).step_by(4) {
            assert_eq!(table.remove(&CollisionKey(key)), Some(key));
        }
        for key in 300..450 {
            assert_eq!(table.insert(CollisionKey(key), key), Ok(None));
        }

        for key in 0..450 {
            let expected = if key < 300 && key % 4 == 0 {
                None
            } else {
                Some(&key)
            };
            assert_eq!(table.get(&CollisionKey(key)), expected);
        }
    }

    #[test]
    fn differential_operation_stream_matches_standard_maps() {
        let mut table = DeterministicHashTable::new(0xd1ff_e2e3_1234_5678);
        let mut hash_map = HashMap::new();
        let mut tree_map = BTreeMap::new();
        let mut state = 0x243f_6a88_85a3_08d3_u64;

        for step in 0..25_000_u64 {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            let key = (state >> 17) % 1_009;
            let value = ((state >> 33) as i64) ^ step as i64;
            match state & 7 {
                0..=3 => {
                    let expected = tree_map.insert(key, value);
                    assert_eq!(hash_map.insert(key, value), expected);
                    assert_eq!(table.insert(key, value), Ok(expected));
                }
                4 => {
                    let expected = tree_map.remove(&key);
                    assert_eq!(hash_map.remove(&key), expected);
                    assert_eq!(table.remove(&key), expected);
                }
                5 => {
                    assert_eq!(table.get(&key), tree_map.get(&key));
                    assert_eq!(hash_map.get(&key), tree_map.get(&key));
                }
                _ => {
                    let delta = (step % 31) as i64;
                    let actual = table.get_mut(&key).map(|slot| {
                        *slot += delta;
                        *slot
                    });
                    let expected = tree_map.get_mut(&key).map(|slot| {
                        *slot += delta;
                        *slot
                    });
                    let hash_value = hash_map.get_mut(&key).map(|slot| {
                        *slot += delta;
                        *slot
                    });
                    assert_eq!(actual, expected);
                    assert_eq!(hash_value, expected);
                }
            }

            assert_eq!(table.len(), tree_map.len());
            if step % 257 == 0 {
                let expected: Vec<_> = tree_map.iter().map(|(&key, &value)| (key, value)).collect();
                assert_eq!(sorted_table(&table), expected);
                assert_eq!(sorted_hash_map(&hash_map), expected);
            }
        }

        let expected: Vec<_> = tree_map.iter().map(|(&key, &value)| (key, value)).collect();
        assert_eq!(sorted_table(&table), expected);
        assert_eq!(sorted_hash_map(&hash_map), expected);
    }

    #[test]
    fn identical_seed_and_operations_have_identical_physical_iteration() {
        fn exercise(seed: u64) -> DeterministicHashTable<u64, u64> {
            let mut table = DeterministicHashTable::new(seed);
            for key in 0_u64..300 {
                assert_eq!(table.insert(key, key.rotate_left(7)), Ok(None));
            }
            for key in (0_u64..300).step_by(3) {
                assert_eq!(table.remove(&key), Some(key.rotate_left(7)));
            }
            for key in 500_u64..650 {
                assert_eq!(table.insert(key, key.rotate_right(5)), Ok(None));
            }
            for key in (1_u64..250).step_by(11) {
                if let Some(value) = table.get_mut(&key) {
                    *value ^= 0xa5a5;
                }
            }
            table
        }

        let first = exercise(0x0123_4567_89ab_cdef);
        let second = exercise(0x0123_4567_89ab_cdef);
        assert_eq!(first.bucket_count(), second.bucket_count());
        assert_eq!(
            first
                .iter()
                .map(|(&key, &value)| (key, value))
                .collect::<Vec<_>>(),
            second
                .iter()
                .map(|(&key, &value)| (key, value))
                .collect::<Vec<_>>()
        );
        assert_eq!(first.controls, second.controls);

        let different_seed = exercise(0xfedc_ba98_7654_3210);
        assert_ne!(
            first.iter().map(|(&key, _)| key).collect::<Vec<_>>(),
            different_seed
                .iter()
                .map(|(&key, _)| key)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn iterator_reports_exact_remaining_length() {
        let mut table = DeterministicHashTable::new(1);
        for key in 0..20 {
            assert_eq!(table.insert(key, key + 1), Ok(None));
        }
        let mut iter = table.iter();
        for remaining in (1..=20).rev() {
            assert_eq!(iter.len(), remaining);
            assert!(iter.next().is_some());
        }
        assert_eq!(iter.len(), 0);
        assert_eq!(iter.next(), None);
    }
}
