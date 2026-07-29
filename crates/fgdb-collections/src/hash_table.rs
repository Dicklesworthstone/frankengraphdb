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

use fgdb_types::QueryCx;
use fgdb_unsafe_arena::{RegionScope, RegionVec, RegionVecError};

pub use crate::probe::CONTROL_GROUP_WIDTH;
use crate::probe::{
    ControlGroup, ControlGroupDispatch, ControlTag, DELETED_CONTROL as DELETED,
    EMPTY_CONTROL as EMPTY, SCALAR_CONTROL_GROUP_DISPATCH,
};

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
    /// Temporary rebuild planning storage could not be reserved.
    AllocationFailed,
    /// Task-local region storage refused a resident table buffer.
    Region(RegionVecError),
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
                formatter.write_str("deterministic hash-table rebuild planning allocation failed")
            }
            Self::Region(source) => {
                write!(
                    formatter,
                    "deterministic hash-table region storage refused: {source}"
                )
            }
            Self::InvariantViolation => {
                formatter.write_str("deterministic hash-table internal invariant violation")
            }
        }
    }
}

impl std::error::Error for HashTableError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Region(source) => Some(source),
            _ => None,
        }
    }
}

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
#[derive(Debug)]
pub struct DeterministicHashTable<'region, K, V> {
    scope: &'region RegionScope,
    seed: u64,
    control_group_dispatch: ControlGroupDispatch,
    controls: RegionVec<'region, u8>,
    entries: RegionVec<'region, Option<Entry<K, V>>>,
    len: usize,
    deleted: usize,
}

impl<'region, K, V> DeterministicHashTable<'region, K, V> {
    /// Creates an empty, allocation-free table with an explicit seed.
    pub fn new_in(scope: &'region RegionScope, seed: u64) -> Result<Self, HashTableError> {
        Self::with_control_group_dispatch_in(scope, seed, SCALAR_CONTROL_GROUP_DISPATCH)
    }

    /// Creates an empty table with an explicit answer-equivalent probe
    /// classifier.
    ///
    /// Alternate dispatches must obey [`ControlGroupDispatch`]'s lane-mask
    /// contract exactly. Keeping selection explicit makes kernel choice
    /// replayable and lets callers prove physical-order equivalence before a
    /// target-specific backend becomes the default.
    pub fn with_control_group_dispatch_in(
        scope: &'region RegionScope,
        seed: u64,
        control_group_dispatch: ControlGroupDispatch,
    ) -> Result<Self, HashTableError> {
        Ok(Self {
            scope,
            seed,
            control_group_dispatch,
            controls: RegionVec::new_in(scope).map_err(HashTableError::Region)?,
            entries: RegionVec::new_in(scope).map_err(HashTableError::Region)?,
            len: 0,
            deleted: 0,
        })
    }

    /// Creates a table that can hold at least `min_entries` without growth.
    pub fn try_with_capacity_in(
        scope: &'region RegionScope,
        cx: &QueryCx,
        seed: u64,
        min_entries: usize,
    ) -> Result<Self, HashTableError> {
        Self::try_with_capacity_and_dispatch_in(
            scope,
            cx,
            seed,
            min_entries,
            SCALAR_CONTROL_GROUP_DISPATCH,
        )
    }

    /// Creates a preallocated table with an explicit answer-equivalent probe
    /// classifier.
    pub fn try_with_capacity_and_dispatch_in(
        scope: &'region RegionScope,
        cx: &QueryCx,
        seed: u64,
        min_entries: usize,
        control_group_dispatch: ControlGroupDispatch,
    ) -> Result<Self, HashTableError> {
        let buckets = required_bucket_count(min_entries)?;
        Self::allocate(scope, cx, seed, buckets, control_group_dispatch)
    }

    fn allocate(
        scope: &'region RegionScope,
        cx: &QueryCx,
        seed: u64,
        buckets: usize,
        control_group_dispatch: ControlGroupDispatch,
    ) -> Result<Self, HashTableError> {
        if buckets == 0 {
            return Self::with_control_group_dispatch_in(scope, seed, control_group_dispatch);
        }
        if buckets < MIN_BUCKETS
            || !buckets.is_power_of_two()
            || !buckets.is_multiple_of(CONTROL_GROUP_WIDTH)
        {
            return Err(HashTableError::CapacityOverflow);
        }

        let mut controls =
            RegionVec::with_capacity_in(scope, cx, buckets).map_err(HashTableError::Region)?;
        controls
            .try_resize(cx, buckets, EMPTY)
            .map_err(HashTableError::Region)?;

        let mut entries =
            RegionVec::with_capacity_in(scope, cx, buckets).map_err(HashTableError::Region)?;
        entries
            .try_resize_with(cx, buckets, || None)
            .map_err(HashTableError::Region)?;

        Ok(Self {
            scope,
            seed,
            control_group_dispatch,
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
    pub fn try_reserve(&mut self, cx: &QueryCx, additional: usize) -> Result<(), HashTableError> {
        let required_entries = self
            .len
            .checked_add(additional)
            .ok_or(HashTableError::CapacityOverflow)?;
        let required_buckets = required_bucket_count(required_entries)?;
        if required_buckets > self.bucket_count() {
            consumer_checkpoint(cx)?;
            self.rebuild(cx, required_buckets)?;
        }
        Ok(())
    }

    /// Rebuilds into the smallest table that can hold both the current entries
    /// and `min_entries`, removing all tombstones.
    pub fn try_rehash(&mut self, cx: &QueryCx, min_entries: usize) -> Result<(), HashTableError> {
        let required_entries = self.len.max(min_entries);
        let required_buckets = required_bucket_count(required_entries)?;
        consumer_checkpoint(cx)?;
        self.rebuild(cx, required_buckets)
    }

    /// Removes every entry while retaining the allocation and seed.
    pub fn clear(&mut self) {
        for slot in self.entries.iter_mut() {
            *slot = None;
        }
        self.controls.as_mut_slice().fill(EMPTY);
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

    fn rebuild(&mut self, cx: &QueryCx, buckets: usize) -> Result<(), HashTableError> {
        if !self.storage_is_consistent() {
            return Err(HashTableError::InvariantViolation);
        }
        if buckets == 0 && self.len != 0 {
            return Err(HashTableError::CapacityOverflow);
        }
        if max_live_entries(buckets) < self.len {
            return Err(HashTableError::CapacityOverflow);
        }

        let mut replacement = Self::allocate(
            self.scope,
            cx,
            self.seed,
            buckets,
            self.control_group_dispatch,
        )?;
        let mut planned_indexes = Vec::new();
        planned_indexes
            .try_reserve_exact(self.len)
            .map_err(|_| HashTableError::AllocationFailed)?;
        for entry in self.entries.iter().flatten() {
            let index = first_empty_bucket(
                replacement.controls.as_slice(),
                entry.hash,
                replacement.control_group_dispatch,
            )
            .ok_or(HashTableError::InvariantViolation)?;
            *replacement
                .controls
                .get_mut(index)
                .ok_or(HashTableError::InvariantViolation)? =
                ControlTag::from_hash(entry.hash).get();
            planned_indexes.push(index);
        }
        if planned_indexes.len() != self.len {
            return Err(HashTableError::InvariantViolation);
        }

        let mut old = mem::replace(self, replacement);
        let mut planned_indexes = planned_indexes.into_iter();
        for slot in old.entries.iter_mut() {
            let Some(entry) = slot.take() else {
                continue;
            };
            let index = planned_indexes
                .next()
                .ok_or(HashTableError::InvariantViolation)?;
            *self
                .entries
                .get_mut(index)
                .ok_or(HashTableError::InvariantViolation)? = Some(entry);
            self.len += 1;
        }
        if planned_indexes.next().is_some() {
            return Err(HashTableError::InvariantViolation);
        }
        Ok(())
    }

    fn storage_is_consistent(&self) -> bool {
        if self.controls.len() != self.entries.len() {
            return false;
        }
        let mut live = 0_usize;
        let mut deleted = 0_usize;
        for (&control, slot) in self.controls.iter().zip(self.entries.iter()) {
            match (control, slot) {
                (EMPTY, None) => {}
                (DELETED, None) => deleted += 1,
                (tag, Some(entry))
                    if tag < EMPTY && tag == ControlTag::from_hash(entry.hash).get() =>
                {
                    live += 1;
                }
                _ => return false,
            }
        }
        live == self.len && deleted == self.deleted
    }
}

impl<'region, K: Hash + Eq, V> DeterministicHashTable<'region, K, V> {
    /// Inserts `key` and `value`, returning the replaced value when present.
    ///
    /// Growth and tombstone-cleanup allocation are checked and reported rather
    /// than delegated to an infallible global-heap growth operation.
    pub fn insert(&mut self, cx: &QueryCx, key: K, value: V) -> Result<Option<V>, HashTableError> {
        consumer_checkpoint(cx)?;
        if self.bucket_count() == 0 {
            self.rebuild(cx, MIN_BUCKETS)?;
        }

        let hash = self.hash(&key);
        let mut location = self.locate_for_insert(&key, hash)?;
        match location {
            InsertLocation::Existing(index) => {
                let Some(entry) = self.entries.get_mut(index).and_then(Option::as_mut) else {
                    return Err(HashTableError::InvariantViolation);
                };
                return Ok(Some(mem::replace(&mut entry.value, value)));
            }
            InsertLocation::Vacant(_) | InsertLocation::Saturated => {}
        }

        let additional_control = match location {
            InsertLocation::Vacant(index) => usize::from(
                self.controls
                    .get(index)
                    .is_some_and(|control| *control == EMPTY),
            ),
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
            self.rebuild(cx, target)?;
            location = self.locate_for_insert(&key, hash)?;
        }

        let index = match location {
            InsertLocation::Existing(index) => {
                let Some(entry) = self.entries.get_mut(index).and_then(Option::as_mut) else {
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
                self.rebuild(cx, target)?;
                match self.locate_for_insert(&key, hash)? {
                    InsertLocation::Vacant(index) => index,
                    InsertLocation::Existing(index) => {
                        let Some(entry) = self.entries.get_mut(index).and_then(Option::as_mut)
                        else {
                            return Err(HashTableError::InvariantViolation);
                        };
                        return Ok(Some(mem::replace(&mut entry.value, value)));
                    }
                    InsertLocation::Saturated => return Err(HashTableError::InvariantViolation),
                }
            }
        };

        if self
            .controls
            .get(index)
            .is_some_and(|control| *control == DELETED)
        {
            self.deleted -= 1;
        }
        *self
            .controls
            .get_mut(index)
            .ok_or(HashTableError::InvariantViolation)? = ControlTag::from_hash(hash).get();
        *self
            .entries
            .get_mut(index)
            .ok_or(HashTableError::InvariantViolation)? = Some(Entry { hash, key, value });
        self.len += 1;
        Ok(None)
    }

    fn locate_for_insert(&self, key: &K, hash: u64) -> Result<InsertLocation, HashTableError> {
        let tag = ControlTag::from_hash(hash);
        let mut first_deleted = None;
        let mut probe = GroupProbe::new(hash, self.bucket_count());
        while let Some(group_start) = probe.next_group() {
            let group = ControlGroup::gather_wrapping(self.controls.as_slice(), group_start)
                .ok_or(HashTableError::InvariantViolation)?;
            let masks = self.control_group_dispatch.classify(&group, tag);
            let lanes_before_empty = masks.empty.first().unwrap_or(CONTROL_GROUP_WIDTH);
            for lane in 0..lanes_before_empty {
                let index = (group_start + lane) & (self.bucket_count() - 1);
                if masks.deleted.contains(lane) {
                    if self.entries.get(index).is_some_and(Option::is_some) {
                        return Err(HashTableError::InvariantViolation);
                    }
                    if first_deleted.is_none() {
                        first_deleted = Some(index);
                    }
                } else if masks.matching.contains(lane) {
                    let Some(entry) = self.entries.get(index).and_then(Option::as_ref) else {
                        return Err(HashTableError::InvariantViolation);
                    };
                    if entry.hash == hash && entry.key == *key {
                        return Ok(InsertLocation::Existing(index));
                    }
                } else if self.entries.get(index).is_none_or(Option::is_none) {
                    return Err(HashTableError::InvariantViolation);
                }
            }
            if let Some(empty_lane) = masks.empty.first() {
                let index = (group_start + empty_lane) & (self.bucket_count() - 1);
                if self.entries.get(index).is_some_and(Option::is_some) {
                    return Err(HashTableError::InvariantViolation);
                }
                return Ok(InsertLocation::Vacant(first_deleted.unwrap_or(index)));
            }
        }
        Ok(first_deleted.map_or(InsertLocation::Saturated, InsertLocation::Vacant))
    }
}

impl<'region, K, V> DeterministicHashTable<'region, K, V> {
    /// Returns a shared value reference for a borrowed form of the key.
    #[must_use]
    pub fn get<Q>(&self, key: &Q) -> Option<&V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.find_index(key)
            .and_then(|index| self.entries.get(index))
            .and_then(Option::as_ref)
            .map(|entry| &entry.value)
    }

    /// Returns a mutable value reference for a borrowed form of the key.
    #[must_use]
    pub fn get_mut<Q>(&mut self, key: &Q) -> Option<&mut V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        let index = self.find_index(key)?;
        self.entries
            .get_mut(index)
            .and_then(Option::as_mut)
            .map(|entry| &mut entry.value)
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
        let entry = self.entries.get_mut(index)?.take()?;
        *self.controls.get_mut(index)? = DELETED;
        self.len -= 1;
        self.deleted += 1;
        if self.len == 0 {
            self.controls.as_mut_slice().fill(EMPTY);
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
        let tag = ControlTag::from_hash(hash);
        let mut probe = GroupProbe::new(hash, self.bucket_count());
        while let Some(group_start) = probe.next_group() {
            let group = ControlGroup::gather_wrapping(self.controls.as_slice(), group_start)?;
            let masks = self.control_group_dispatch.classify(&group, tag);
            let lanes_before_empty = masks.empty.first().unwrap_or(CONTROL_GROUP_WIDTH);
            for lane in 0..lanes_before_empty {
                let index = (group_start + lane) & (self.bucket_count() - 1);
                if masks.deleted.contains(lane) {
                    if self.entries.get(index).is_some_and(Option::is_some) {
                        return None;
                    }
                } else if masks.matching.contains(lane) {
                    let entry = self.entries.get(index)?.as_ref()?;
                    if entry.hash == hash && entry.key.borrow() == key {
                        return Some(index);
                    }
                } else {
                    self.entries.get(index)?.as_ref()?;
                }
            }
            if masks.empty.first().is_some() {
                return None;
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

fn first_empty_bucket(
    controls: &[u8],
    hash: u64,
    control_group_dispatch: ControlGroupDispatch,
) -> Option<usize> {
    if controls.is_empty() {
        return None;
    }
    let mut probe = GroupProbe::new(hash, controls.len());
    while let Some(group_start) = probe.next_group() {
        let group = ControlGroup::gather_wrapping(controls, group_start)?;
        let masks = control_group_dispatch.classify(&group, ControlTag::from_hash(hash));
        if let Some(lane) = masks.empty.first() {
            return Some((group_start + lane) & (controls.len() - 1));
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

fn consumer_checkpoint(cx: &QueryCx) -> Result<(), HashTableError> {
    cx.checkpoint()
        .map_err(|_| HashTableError::Region(RegionVecError::CheckpointRefused))
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

impl<'a, 'region, K, V> IntoIterator for &'a DeterministicHashTable<'region, K, V> {
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
    use crate::probe::{
        ControlGroupDispatch, SCALAR_CONTROL_GROUP_DISPATCH, SWAR_CONTROL_GROUP_DISPATCH,
    };
    #[cfg(miri)]
    use asupersync::Cx;
    #[cfg(miri)]
    use asupersync::cx::cap;
    #[cfg(not(miri))]
    use asupersync::lab::run_async_under_lab;
    use core::hash::{Hash, Hasher};
    use fgdb_types::{PurposeContexts, QueryCx};
    use fgdb_unsafe_arena::RegionScope;
    use std::collections::{BTreeMap, HashMap};

    #[cfg(not(miri))]
    fn query_cx() -> QueryCx {
        let (query, report) = run_async_under_lab(0x4a55_7ab1, |root| async move {
            PurposeContexts::narrow_runtime_root(&root).query()
        });
        assert!(
            report.invariant_violations.is_empty(),
            "lab invariant violation: {report:?}"
        );
        query
    }

    #[cfg(miri)]
    fn query_cx() -> QueryCx {
        let root = Cx::<cap::All>::for_testing();
        PurposeContexts::narrow_runtime_root(&root).query()
    }

    fn test_scope() -> RegionScope {
        RegionScope::with_capacity(1 << 20, 1 << 28)
    }

    #[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
    struct CollisionKey(u32);

    impl Hash for CollisionKey {
        fn hash<H: Hasher>(&self, state: &mut H) {
            0x5au8.hash(state);
        }
    }

    fn physical_index<K: Eq, V>(
        table: &DeterministicHashTable<'_, K, V>,
        key: &K,
    ) -> Option<usize> {
        table
            .entries
            .iter()
            .position(|slot| slot.as_ref().is_some_and(|entry| entry.key == *key))
    }

    fn sorted_table(table: &DeterministicHashTable<'_, u64, i64>) -> Vec<(u64, i64)> {
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
        let scope = test_scope();
        let cx = query_cx();
        let mut table = DeterministicHashTable::new_in(&scope, 17).expect("table opens");
        assert_eq!(table.insert(&cx, "alpha".to_owned(), 10), Ok(None));
        assert_eq!(table.insert(&cx, "beta".to_owned(), 20), Ok(None));
        assert_eq!(table.insert(&cx, "alpha".to_owned(), 11), Ok(Some(10)));
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
        let scope = test_scope();
        let cx = query_cx();
        let mut table = DeterministicHashTable::new_in(&scope, 7).expect("table opens");
        for key in 0..14 {
            assert_eq!(table.insert(&cx, CollisionKey(key), key * 10), Ok(None));
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

        assert_eq!(table.insert(&cx, CollisionKey(100), 1000), Ok(None));
        assert_eq!(table.bucket_count(), 16);
        assert_eq!(physical_index(&table, &CollisionKey(100)), first_hole);
        assert_eq!(table.deleted, 1);
        assert_eq!(table.insert(&cx, CollisionKey(101), 1010), Ok(None));
        assert_eq!(table.bucket_count(), 16);
        assert_eq!(physical_index(&table, &CollisionKey(101)), second_hole);
        assert_eq!(table.deleted, 0);
    }

    #[test]
    fn probing_wraps_across_the_last_bucket() {
        let scope = test_scope();
        let cx = query_cx();
        let mut table = DeterministicHashTable::new_in(&scope, 91).expect("table opens");

        let mut keys = Vec::new();
        let mut candidate = 0_u64;
        while keys.len() < 8 {
            if table.hash(&candidate) as usize & 15 == 14 {
                keys.push(candidate);
            }
            candidate += 1;
        }

        for (ordinal, &key) in keys.iter().enumerate() {
            assert_eq!(table.insert(&cx, key, ordinal), Ok(None));
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
        let scope = test_scope();
        let cx = query_cx();
        let mut table = DeterministicHashTable::new_in(&scope, 23).expect("table opens");
        for key in 0_u64..14 {
            assert_eq!(table.insert(&cx, key, key), Ok(None));
        }
        assert_eq!(table.bucket_count(), 16);
        assert_eq!(table.capacity(), 14);

        assert_eq!(table.insert(&cx, 3, 300), Ok(Some(3)));
        assert_eq!(table.bucket_count(), 16);
        assert_eq!(table.insert(&cx, 14, 14), Ok(None));
        assert_eq!(table.bucket_count(), 32);

        assert_eq!(table.try_reserve(&cx, 1_000), Ok(()));
        assert!(table.capacity() >= table.len() + 1_000);
        for key in 0_u64..15 {
            assert_eq!(table.get(&key), Some(&(if key == 3 { 300 } else { key })));
        }

        assert_eq!(
            table.try_reserve(&cx, usize::MAX),
            Err(HashTableError::CapacityOverflow)
        );
        assert!(matches!(
            DeterministicHashTable::<u8, u8>::try_with_capacity_in(&scope, &cx, 0, usize::MAX),
            Err(HashTableError::CapacityOverflow)
        ));
    }

    #[test]
    fn rehash_clears_tombstones_without_semantic_drift() {
        let scope = test_scope();
        let cx = query_cx();
        let mut table = DeterministicHashTable::<u64, i64>::new_in(&scope, 5).expect("table opens");
        for key in 0_u64..100 {
            assert_eq!(table.insert(&cx, key, key as i64 * 3), Ok(None));
        }
        for key in (0_u64..100).step_by(2) {
            assert_eq!(table.remove(&key), Some(key as i64 * 3));
        }
        assert!(table.deleted > 0);

        let before = sorted_table(&table);
        assert_eq!(table.try_rehash(&cx, table.len()), Ok(()));
        assert_eq!(table.deleted, 0);
        assert_eq!(sorted_table(&table), before);
    }

    #[test]
    fn full_hash_collisions_survive_growth_and_deletion_churn() {
        let scope = test_scope();
        let cx = query_cx();
        let mut table = DeterministicHashTable::new_in(&scope, 0x77).expect("table opens");
        for key in 0..300 {
            assert_eq!(table.insert(&cx, CollisionKey(key), key), Ok(None));
        }
        assert!(table.bucket_count() > MIN_BUCKETS);

        for key in (0..300).step_by(4) {
            assert_eq!(table.remove(&CollisionKey(key)), Some(key));
        }
        for key in 300..450 {
            assert_eq!(table.insert(&cx, CollisionKey(key), key), Ok(None));
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
        let scope = test_scope();
        let cx = query_cx();
        let mut table =
            DeterministicHashTable::new_in(&scope, 0xd1ff_e2e3_1234_5678).expect("table opens");
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
                    assert_eq!(table.insert(&cx, key, value), Ok(expected));
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
    fn two_seeds_have_identical_physical_iteration_across_dispatches() {
        fn exercise<'region>(
            scope: &'region RegionScope,
            cx: &QueryCx,
            seed: u64,
            dispatch: ControlGroupDispatch,
        ) -> DeterministicHashTable<'region, u64, u64> {
            let mut table =
                DeterministicHashTable::with_control_group_dispatch_in(scope, seed, dispatch)
                    .expect("table opens");
            for key in 0_u64..300 {
                assert_eq!(table.insert(cx, key, key.rotate_left(7)), Ok(None));
            }
            for key in (0_u64..300).step_by(3) {
                assert_eq!(table.remove(&key), Some(key.rotate_left(7)));
            }
            for key in 500_u64..650 {
                assert_eq!(table.insert(cx, key, key.rotate_right(5)), Ok(None));
            }
            for key in (1_u64..250).step_by(11) {
                if let Some(value) = table.get_mut(&key) {
                    *value ^= 0xa5a5;
                }
            }
            table
        }

        let cx = query_cx();
        let mut physical_orders = Vec::with_capacity(2);
        for seed in [0x0123_4567_89ab_cdef, 0xfedc_ba98_7654_3210] {
            let first_scope = test_scope();
            let second_scope = test_scope();
            let swar_scope = test_scope();
            let first = exercise(&first_scope, &cx, seed, SCALAR_CONTROL_GROUP_DISPATCH);
            let second = exercise(&second_scope, &cx, seed, SCALAR_CONTROL_GROUP_DISPATCH);
            let swar = exercise(&swar_scope, &cx, seed, SWAR_CONTROL_GROUP_DISPATCH);
            let first_entries = first
                .iter()
                .map(|(&key, &value)| (key, value))
                .collect::<Vec<_>>();
            let second_entries = second
                .iter()
                .map(|(&key, &value)| (key, value))
                .collect::<Vec<_>>();
            let swar_entries = swar
                .iter()
                .map(|(&key, &value)| (key, value))
                .collect::<Vec<_>>();

            assert_eq!(
                first.bucket_count(),
                second.bucket_count(),
                "seed={seed:#018x}"
            );
            assert_eq!(first_entries, second_entries, "seed={seed:#018x}");
            assert_eq!(first.controls, second.controls, "seed={seed:#018x}");
            assert_eq!(
                first.bucket_count(),
                swar.bucket_count(),
                "seed={seed:#018x}"
            );
            assert_eq!(first_entries, swar_entries, "seed={seed:#018x}");
            assert_eq!(first.controls, swar.controls, "seed={seed:#018x}");
            physical_orders.push(
                first_entries
                    .into_iter()
                    .map(|(key, _)| key)
                    .collect::<Vec<_>>(),
            );
        }
        assert_ne!(
            physical_orders[0], physical_orders[1],
            "the two parity seeds must exercise distinct physical layouts"
        );
    }

    #[test]
    fn iterator_reports_exact_remaining_length() {
        let scope = test_scope();
        let cx = query_cx();
        let mut table = DeterministicHashTable::new_in(&scope, 1).expect("table opens");
        for key in 0..20 {
            assert_eq!(table.insert(&cx, key, key + 1), Ok(None));
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
