//! Borrowed graph access for the same FreeJoin operator family.
//!
//! Only navigation metadata is owned. Keys and parallel-edge occurrences stay
//! in the caller's immutable, already-admitted adjacency/vertex generation.
//! No source is opened here and a numeric generation tag grants no authority.

use super::{JoinVariable, TrieCursor, TrieRelation};
use crate::GlaExecutionEvent;
use fgdb_types::VId;
use std::collections::BTreeMap;

#[derive(Debug, PartialEq, Eq)]
pub enum GraphTrieError<E> {
    Control(E),
    RepeatedVariable,
    UnsortedInput,
}
impl<E: core::fmt::Display> core::fmt::Display for GraphTrieError<E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Control(error) => error.fmt(f),
            Self::RepeatedVariable => f.write_str("use a diagonal trie for a repeated endpoint"),
            Self::UnsortedInput => f.write_str("graph trie input is not in canonical key order"),
        }
    }
}
impl<E: core::error::Error + 'static> core::error::Error for GraphTrieError<E> {}

struct Prefix<'a> {
    key: &'a VId,
    neighbors: &'a [VId],
    // Exclusive ends of distinct neighbor runs; multiplicities are differences.
    ends: Vec<usize>,
    unary_weight: usize,
}

/// Unary vertex/diagonal or binary adjacency trie. Orientation must already
/// match `order`; the adapter never silently transposes or masks its source.
/// Empty descriptors do not become keys. Vertex domains have set semantics;
/// adjacency and diagonal leaves retain exact parallel-edge multiplicity.
pub struct GraphTrie<'a> {
    order: Vec<JoinVariable>,
    prefixes: Vec<Prefix<'a>>,
    pairs: usize,
    generation: u64,
}
impl core::fmt::Debug for GraphTrie<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("GraphTrie")
            .field("order", &self.order)
            .field("generation", &self.generation)
            .field("keys", &"[REDACTED]")
            .finish()
    }
}

impl<'a> GraphTrie<'a> {
    pub fn adjacency<E>(
        order: [JoinVariable; 2],
        adjacency: &'a BTreeMap<VId, Vec<VId>>,
        generation: u64,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Self, GraphTrieError<E>> {
        if order[0] == order[1] {
            return Err(GraphTrieError::RepeatedVariable);
        }
        let mut meter = |event| control(event).map_err(GraphTrieError::Control);
        for _ in order {
            meter(GlaExecutionEvent::ScratchEntry)?;
        }
        let mut result = Self {
            order: order.to_vec(),
            prefixes: Vec::new(),
            pairs: 0,
            generation,
        };
        for (key, neighbors) in adjacency {
            meter(GlaExecutionEvent::Work)?;
            if neighbors.is_empty() {
                continue;
            }
            meter(GlaExecutionEvent::ScratchEntry)?;
            let mut ends = Vec::new();
            for at in 0..neighbors.len() {
                meter(GlaExecutionEvent::Work)?;
                if at > 0 {
                    if neighbors[at] < neighbors[at - 1] {
                        return Err(GraphTrieError::UnsortedInput);
                    }
                    if neighbors[at] != neighbors[at - 1] {
                        meter(GlaExecutionEvent::ScratchEntry)?;
                        ends.push(at);
                    }
                }
            }
            meter(GlaExecutionEvent::ScratchEntry)?;
            ends.push(neighbors.len());
            // Each run accounts for at least one separately stored input VId.
            result.pairs += ends.len();
            result.prefixes.push(Prefix {
                key,
                neighbors,
                ends,
                unary_weight: 0,
            });
        }
        Ok(result)
    }

    pub fn vertices<E>(
        variable: JoinVariable,
        vertices: &'a [VId],
        generation: u64,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Self, GraphTrieError<E>> {
        let mut meter = |event| control(event).map_err(GraphTrieError::Control);
        meter(GlaExecutionEvent::ScratchEntry)?;
        let mut result = Self {
            order: vec![variable],
            prefixes: Vec::new(),
            pairs: 0,
            generation,
        };
        for (at, key) in vertices.iter().enumerate() {
            meter(GlaExecutionEvent::Work)?;
            if at > 0 {
                if *key < vertices[at - 1] {
                    return Err(GraphTrieError::UnsortedInput);
                }
                if *key == vertices[at - 1] {
                    continue;
                }
            }
            meter(GlaExecutionEvent::ScratchEntry)?;
            result.prefixes.push(Prefix {
                key,
                neighbors: &[],
                ends: Vec::new(),
                unary_weight: 1,
            });
        }
        Ok(result)
    }

    /// Restrict both endpoints to the same variable without declaring an
    /// invalid two-column schema or losing multiple real self-loop occurrences.
    pub fn diagonal<E>(
        variable: JoinVariable,
        adjacency: &'a BTreeMap<VId, Vec<VId>>,
        generation: u64,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Self, GraphTrieError<E>> {
        let mut meter = |event| control(event).map_err(GraphTrieError::Control);
        meter(GlaExecutionEvent::ScratchEntry)?;
        let mut result = Self {
            order: vec![variable],
            prefixes: Vec::new(),
            pairs: 0,
            generation,
        };
        for (key, neighbors) in adjacency {
            meter(GlaExecutionEvent::Work)?;
            let mut weight = 0;
            for (at, value) in neighbors.iter().enumerate() {
                meter(GlaExecutionEvent::Work)?;
                if at > 0 && *value < neighbors[at - 1] {
                    return Err(GraphTrieError::UnsortedInput);
                }
                weight += usize::from(value == key);
            }
            if weight == 0 {
                continue;
            }
            meter(GlaExecutionEvent::ScratchEntry)?;
            result.prefixes.push(Prefix {
                key,
                neighbors: &[],
                ends: Vec::new(),
                unary_weight: weight,
            });
        }
        Ok(result)
    }
}

/// Cheap immutable navigation state. Parent and child borrow the same adapter,
/// which in turn borrows the admitted graph; neither outlives that generation.
#[derive(Clone)]
pub struct GraphTrieCursor<'t, 'a> {
    trie: &'t GraphTrie<'a>,
    depth: usize,
    prefix: usize,
    position: usize,
    weight: usize,
}
impl<'a> TrieRelation<VId> for GraphTrie<'a> {
    type Cursor<'t>
        = GraphTrieCursor<'t, 'a>
    where
        Self: 't;
    fn attribute_order(&self) -> &[JoinVariable] {
        &self.order
    }
    fn cursor(&self) -> Self::Cursor<'_> {
        GraphTrieCursor {
            trie: self,
            depth: 0,
            prefix: 0,
            position: 0,
            weight: 0,
        }
    }
}
impl GraphTrieCursor<'_, '_> {
    fn len(&self) -> usize {
        if self.depth == self.trie.order.len() {
            0
        } else if self.depth == 0 {
            self.trie.prefixes.len()
        } else {
            self.trie.prefixes[self.prefix].ends.len()
        }
    }
    fn at(&self, position: usize) -> &VId {
        if self.depth == 0 {
            self.trie.prefixes[position].key
        } else {
            let prefix = &self.trie.prefixes[self.prefix];
            let start = if position == 0 {
                0
            } else {
                prefix.ends[position - 1]
            };
            &prefix.neighbors[start]
        }
    }
}
impl TrieCursor<VId> for GraphTrieCursor<'_, '_> {
    fn remaining_order(&self) -> &[JoinVariable] {
        &self.trie.order[self.depth..]
    }
    fn generation(&self) -> u64 {
        self.trie.generation
    }
    fn key(&self) -> Option<&VId> {
        (self.position < self.len()).then(|| self.at(self.position))
    }
    fn multiplicity(&self) -> Option<usize> {
        (self.depth == self.trie.order.len()).then_some(self.weight)
    }
    fn distinct_prefixes(&self, attributes: usize) -> usize {
        if attributes > self.remaining_order().len() {
            return 0;
        }
        if attributes == 0 {
            return usize::from(
                self.multiplicity()
                    .map_or(self.len() != 0, |weight| weight != 0),
            );
        }
        if attributes == 2 {
            self.trie.pairs
        } else {
            self.len()
        }
    }
    fn advance<E>(
        &mut self,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<(), E> {
        control(GlaExecutionEvent::Work)?;
        if self.position < self.len() {
            self.position += 1;
        }
        Ok(())
    }
    fn seek<E>(
        &mut self,
        target: &VId,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<(), E> {
        let mut low = self.position;
        let mut high = self.len();
        // Search commits navigation only after all comparisons are admitted.
        while low < high {
            let middle = low + (high - low) / 2;
            control(GlaExecutionEvent::Work)?;
            if self.at(middle) < target {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        self.position = low;
        Ok(())
    }
    fn open<E>(
        &self,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Option<Self>, E> {
        control(GlaExecutionEvent::Work)?;
        if self.position >= self.len() {
            return Ok(None);
        }
        let mut child = self.clone();
        child.depth += 1;
        child.position = 0;
        if self.depth == 0 {
            child.prefix = self.position;
            child.weight = self.trie.prefixes[self.position].unary_weight;
        } else {
            let prefix = &self.trie.prefixes[self.prefix];
            let start = if self.position == 0 {
                0
            } else {
                prefix.ends[self.position - 1]
            };
            child.weight = prefix.ends[self.position] - start;
        }
        Ok(Some(child))
    }
}

/// Zero-`BTreeMap` contiguous CSR trie over sorted `(VId, VId)` or unary `VId`
/// runs (`fgdb-rc1111-loom-conformal-freejoin-txo5d`).
///
/// Keys are stored in flat `u128` (`VId::0`) arrays so [`SliceAdjacencyTrieCursor::seek`]
/// dispatches directly into [`fgdb_unsafe_simd::sorted_set::seek_ge_u128`] (4-wide
/// SIMD gallop + scalar fallback) while preserving exact parallel-edge
/// multiplicities and charging governed [`GlaExecutionEvent`]s.
#[derive(Clone)]
pub struct SliceAdjacencyTrie {
    order: Vec<JoinVariable>,
    prefix_keys_raw: Vec<u128>,
    prefix_keys: Vec<VId>,
    /// Range `[start, end)` into `neighbor_keys` / `neighbor_weights` per prefix.
    prefix_ranges: Vec<(usize, usize)>,
    prefix_unary_weights: Vec<usize>,
    neighbor_keys_raw: Vec<u128>,
    neighbor_keys: Vec<VId>,
    neighbor_weights: Vec<usize>,
    pairs: usize,
    generation: u64,
}

impl core::fmt::Debug for SliceAdjacencyTrie {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SliceAdjacencyTrie")
            .field("order", &self.order)
            .field("prefixes", &self.prefix_keys.len())
            .field("distinct_pairs", &self.pairs)
            .field("generation", &self.generation)
            .finish()
    }
}

impl SliceAdjacencyTrie {
    /// Builds a binary CSR trie from a canonically sorted `(src, dst)` slice
    /// without allocating any `BTreeMap`.
    pub fn from_sorted_pairs<E>(
        order: [JoinVariable; 2],
        pairs: &[(VId, VId)],
        generation: u64,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Self, GraphTrieError<E>> {
        if order[0] == order[1] {
            return Err(GraphTrieError::RepeatedVariable);
        }
        let mut meter = |event| control(event).map_err(GraphTrieError::Control);
        meter(GlaExecutionEvent::ScratchEntry)?;
        meter(GlaExecutionEvent::ScratchEntry)?;

        let mut prefix_keys_raw = Vec::new();
        let mut prefix_keys = Vec::new();
        let mut prefix_ranges = Vec::new();
        let mut neighbor_keys_raw = Vec::new();
        let mut neighbor_keys = Vec::new();
        let mut neighbor_weights = Vec::new();

        let mut idx = 0_usize;
        while idx < pairs.len() {
            meter(GlaExecutionEvent::Work)?;
            let (src, first_dst) = pairs[idx];
            if idx > 0 && pairs[idx] < pairs[idx - 1] {
                return Err(GraphTrieError::UnsortedInput);
            }
            let neighbor_start = neighbor_keys.len();
            let mut cur_dst = first_dst;
            let mut weight = 1_usize;
            idx += 1;
            while idx < pairs.len() && pairs[idx].0 == src {
                meter(GlaExecutionEvent::Work)?;
                let next_dst = pairs[idx].1;
                if next_dst < cur_dst {
                    return Err(GraphTrieError::UnsortedInput);
                }
                if next_dst == cur_dst {
                    weight += 1;
                } else {
                    meter(GlaExecutionEvent::ScratchEntry)?;
                    neighbor_keys_raw.push(cur_dst.0);
                    neighbor_keys.push(cur_dst);
                    neighbor_weights.push(weight);
                    cur_dst = next_dst;
                    weight = 1;
                }
                idx += 1;
            }
            meter(GlaExecutionEvent::ScratchEntry)?;
            neighbor_keys_raw.push(cur_dst.0);
            neighbor_keys.push(cur_dst);
            neighbor_weights.push(weight);

            meter(GlaExecutionEvent::ScratchEntry)?;
            prefix_keys_raw.push(src.0);
            prefix_keys.push(src);
            prefix_ranges.push((neighbor_start, neighbor_keys.len()));
        }

        let distinct_pairs = neighbor_keys.len();
        Ok(Self {
            order: order.to_vec(),
            prefix_keys_raw,
            prefix_keys,
            prefix_ranges,
            prefix_unary_weights: Vec::new(),
            neighbor_keys_raw,
            neighbor_keys,
            neighbor_weights,
            pairs: distinct_pairs,
            generation,
        })
    }

    /// Builds a unary diagonal CSR trie (`src == dst` self-loops) from a sorted
    /// `(src, dst)` slice, preserving parallel self-loop multiplicities.
    pub fn diagonal_from_sorted_pairs<E>(
        variable: JoinVariable,
        pairs: &[(VId, VId)],
        generation: u64,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Self, GraphTrieError<E>> {
        let mut meter = |event| control(event).map_err(GraphTrieError::Control);
        meter(GlaExecutionEvent::ScratchEntry)?;
        let mut prefix_keys_raw = Vec::new();
        let mut prefix_keys = Vec::new();
        let mut prefix_unary_weights = Vec::new();

        let mut idx = 0_usize;
        while idx < pairs.len() {
            meter(GlaExecutionEvent::Work)?;
            if idx > 0 && pairs[idx] < pairs[idx - 1] {
                return Err(GraphTrieError::UnsortedInput);
            }
            let (src, dst) = pairs[idx];
            if src != dst {
                idx += 1;
                continue;
            }
            let mut weight = 1_usize;
            idx += 1;
            while idx < pairs.len() && pairs[idx] == (src, src) {
                meter(GlaExecutionEvent::Work)?;
                weight += 1;
                idx += 1;
            }
            if let Some(&prev) = prefix_keys.last()
                && prev == src
            {
                *prefix_unary_weights.last_mut().expect("non-empty") += weight;
            } else {
                meter(GlaExecutionEvent::ScratchEntry)?;
                prefix_keys_raw.push(src.0);
                prefix_keys.push(src);
                prefix_unary_weights.push(weight);
            }
        }
        Ok(Self {
            order: vec![variable],
            prefix_keys_raw,
            prefix_keys,
            prefix_ranges: Vec::new(),
            prefix_unary_weights,
            neighbor_keys_raw: Vec::new(),
            neighbor_keys: Vec::new(),
            neighbor_weights: Vec::new(),
            pairs: 0,
            generation,
        })
    }

    /// Builds a unary vertex-domain CSR trie from a sorted `VId` slice.
    pub fn from_sorted_vertices<E>(
        variable: JoinVariable,
        vertices: &[VId],
        generation: u64,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Self, GraphTrieError<E>> {
        let mut meter = |event| control(event).map_err(GraphTrieError::Control);
        meter(GlaExecutionEvent::ScratchEntry)?;
        let mut prefix_keys_raw = Vec::new();
        let mut prefix_keys = Vec::new();
        let mut prefix_unary_weights = Vec::new();
        for (at, &vid) in vertices.iter().enumerate() {
            meter(GlaExecutionEvent::Work)?;
            if at > 0 {
                if vid < vertices[at - 1] {
                    return Err(GraphTrieError::UnsortedInput);
                }
                if vid == vertices[at - 1] {
                    continue;
                }
            }
            meter(GlaExecutionEvent::ScratchEntry)?;
            prefix_keys_raw.push(vid.0);
            prefix_keys.push(vid);
            prefix_unary_weights.push(1);
        }
        Ok(Self {
            order: vec![variable],
            prefix_keys_raw,
            prefix_keys,
            prefix_ranges: Vec::new(),
            prefix_unary_weights,
            neighbor_keys_raw: Vec::new(),
            neighbor_keys: Vec::new(),
            neighbor_weights: Vec::new(),
            pairs: 0,
            generation,
        })
    }

    /// Distinct `(src, dst)` pairs (or unary keys for unary tries).
    #[must_use]
    pub fn distinct_pair_count(&self) -> usize {
        if self.order.len() == 2 {
            self.pairs
        } else {
            self.prefix_keys.len()
        }
    }

    /// Prefix keys at level 0.
    #[must_use]
    pub fn prefix_count(&self) -> usize {
        self.prefix_keys.len()
    }
}

/// Immutable cursor over [`SliceAdjacencyTrie`] backed by `fgdb-unsafe-simd`
/// sorted-set gallop/SIMD seek.
#[derive(Clone)]
pub struct SliceAdjacencyTrieCursor<'t> {
    trie: &'t SliceAdjacencyTrie,
    depth: usize,
    prefix: usize,
    position: usize,
    weight: usize,
}

impl TrieRelation<VId> for SliceAdjacencyTrie {
    type Cursor<'t>
        = SliceAdjacencyTrieCursor<'t>
    where
        Self: 't;

    fn attribute_order(&self) -> &[JoinVariable] {
        &self.order
    }

    fn cursor(&self) -> Self::Cursor<'_> {
        SliceAdjacencyTrieCursor {
            trie: self,
            depth: 0,
            prefix: 0,
            position: 0,
            weight: 0,
        }
    }
}

impl SliceAdjacencyTrieCursor<'_> {
    fn len(&self) -> usize {
        if self.depth == self.trie.order.len() {
            0
        } else if self.depth == 0 {
            self.trie.prefix_keys.len()
        } else {
            let (start, end) = self.trie.prefix_ranges[self.prefix];
            end - start
        }
    }

    fn at(&self, position: usize) -> &VId {
        if self.depth == 0 {
            &self.trie.prefix_keys[position]
        } else {
            let (start, _) = self.trie.prefix_ranges[self.prefix];
            &self.trie.neighbor_keys[start + position]
        }
    }

    fn raw_slice(&self) -> &[u128] {
        if self.depth == 0 {
            &self.trie.prefix_keys_raw
        } else {
            let (start, end) = self.trie.prefix_ranges[self.prefix];
            &self.trie.neighbor_keys_raw[start..end]
        }
    }
}

impl TrieCursor<VId> for SliceAdjacencyTrieCursor<'_> {
    fn remaining_order(&self) -> &[JoinVariable] {
        &self.trie.order[self.depth..]
    }

    fn generation(&self) -> u64 {
        self.trie.generation
    }

    fn key(&self) -> Option<&VId> {
        (self.position < self.len()).then(|| self.at(self.position))
    }

    fn multiplicity(&self) -> Option<usize> {
        (self.depth == self.trie.order.len()).then_some(self.weight)
    }

    fn distinct_prefixes(&self, attributes: usize) -> usize {
        if attributes > self.remaining_order().len() {
            return 0;
        }
        if attributes == 0 {
            return usize::from(
                self.multiplicity()
                    .map_or(self.len() != 0, |weight| weight != 0),
            );
        }
        if attributes == 2 {
            self.trie.pairs
        } else {
            self.len()
        }
    }

    fn advance<E>(
        &mut self,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<(), E> {
        control(GlaExecutionEvent::Work)?;
        if self.position < self.len() {
            self.position += 1;
        }
        Ok(())
    }

    fn seek<E>(
        &mut self,
        target: &VId,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<(), E> {
        let slice = self.raw_slice();
        let (next_pos, comparisons) =
            fgdb_unsafe_simd::sorted_set::seek_ge_u128(slice, self.position, target.0);
        for _ in 0..comparisons.max(1) {
            control(GlaExecutionEvent::Work)?;
        }
        self.position = next_pos;
        Ok(())
    }

    fn open<E>(
        &self,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Option<Self>, E> {
        control(GlaExecutionEvent::Work)?;
        if self.position >= self.len() {
            return Ok(None);
        }
        let mut child = self.clone();
        child.depth += 1;
        child.position = 0;
        if self.depth == 0 {
            child.prefix = self.position;
            if self.trie.order.len() == 1 {
                child.weight = self.trie.prefix_unary_weights[self.position];
            }
        } else {
            let (start, _) = self.trie.prefix_ranges[self.prefix];
            child.weight = self.trie.neighbor_weights[start + self.position];
        }
        Ok(Some(child))
    }
}
