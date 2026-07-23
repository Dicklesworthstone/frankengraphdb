//! A deterministic, byte-keyed adaptive radix tree.
//!
//! This module implements the classical ART fan-out progression
//! `Node4 -> Node16 -> Node48 -> Node256`. Edges are kept in byte order and
//! unary paths are compressed into node prefixes. The tree is an in-memory
//! collection: no representation in this module is a durable byte format.

use core::fmt;
use core::ops::{Bound, RangeBounds};
use std::collections::TryReserveError;

/// Default maximum accepted key length (16 MiB).
pub const DEFAULT_MAX_KEY_BYTES: usize = 16 * 1024 * 1024;

/// Default maximum number of entries.
pub const DEFAULT_MAX_ENTRIES: usize = 1 << 30;

/// Resource limits enforced before an insertion can grow the tree.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ArtLimits {
    /// Maximum byte length of one key.
    pub max_key_bytes: usize,
    /// Maximum number of distinct keys.
    pub max_entries: usize,
}

impl Default for ArtLimits {
    fn default() -> Self {
        Self {
            max_key_bytes: DEFAULT_MAX_KEY_BYTES,
            max_entries: DEFAULT_MAX_ENTRIES,
        }
    }
}

/// A fallible ART operation failed without changing the logical mapping.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ArtError {
    /// The key exceeds the tree's configured key-size limit.
    KeyTooLong { len: usize, max: usize },
    /// A new distinct key would exceed the configured entry limit.
    EntryLimitReached { max: usize },
    /// The global allocator refused a bounded reservation.
    AllocationFailed {
        operation: &'static str,
        requested: usize,
    },
    /// An impossible internal representation transition was detected.
    InvariantViolation { operation: &'static str },
}

impl fmt::Display for ArtError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::KeyTooLong { len, max } => {
                write!(f, "ART key length {len} exceeds configured maximum {max}")
            }
            Self::EntryLimitReached { max } => {
                write!(f, "ART entry count reached configured maximum {max}")
            }
            Self::AllocationFailed {
                operation,
                requested,
            } => write!(
                f,
                "ART allocation failed while {operation} ({requested} elements requested)"
            ),
            Self::InvariantViolation { operation } => {
                write!(f, "ART representation invariant failed while {operation}")
            }
        }
    }
}

impl std::error::Error for ArtError {}

/// The adaptive representation used by one internal ART node.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum NodeKind {
    Node4,
    Node16,
    Node48,
    Node256,
}

/// Counts of internal node representations, useful for diagnostics and tests.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct NodeKindHistogram {
    pub node4: usize,
    pub node16: usize,
    pub node48: usize,
    pub node256: usize,
}

impl NodeKindHistogram {
    /// Returns the count for one representation.
    #[must_use]
    pub const fn count(self, kind: NodeKind) -> usize {
        match kind {
            NodeKind::Node4 => self.node4,
            NodeKind::Node16 => self.node16,
            NodeKind::Node48 => self.node48,
            NodeKind::Node256 => self.node256,
        }
    }

    /// Returns the total number of allocated nodes.
    #[must_use]
    pub const fn total(self) -> usize {
        self.node4 + self.node16 + self.node48 + self.node256
    }

    fn record(&mut self, kind: NodeKind) {
        match kind {
            NodeKind::Node4 => self.node4 += 1,
            NodeKind::Node16 => self.node16 += 1,
            NodeKind::Node48 => self.node48 += 1,
            NodeKind::Node256 => self.node256 += 1,
        }
    }
}

struct Entry<V> {
    key: Vec<u8>,
    value: V,
}

struct Node<V> {
    /// Bytes after the incoming parent edge. At the root this is the initial
    /// common prefix of every key.
    prefix: Vec<u8>,
    entry: Option<Entry<V>>,
    children: Children<V>,
}

enum Children<V> {
    Node4 {
        keys: Vec<u8>,
        nodes: Vec<Node<V>>,
    },
    Node16 {
        keys: Vec<u8>,
        nodes: Vec<Node<V>>,
    },
    Node48 {
        /// Zero means absent; otherwise the stored value is slot + 1.
        index: Vec<u8>,
        nodes: Vec<Node<V>>,
    },
    Node256 {
        nodes: Vec<Option<Node<V>>>,
        len: usize,
    },
}

impl<V> Children<V> {
    const NODE4_CAPACITY: usize = 4;
    const NODE16_CAPACITY: usize = 16;
    const NODE48_CAPACITY: usize = 48;
    const NODE256_CAPACITY: usize = 256;

    fn empty() -> Self {
        Self::Node4 {
            keys: Vec::new(),
            nodes: Vec::new(),
        }
    }

    fn kind(&self) -> NodeKind {
        match self {
            Self::Node4 { .. } => NodeKind::Node4,
            Self::Node16 { .. } => NodeKind::Node16,
            Self::Node48 { .. } => NodeKind::Node48,
            Self::Node256 { .. } => NodeKind::Node256,
        }
    }

    fn len(&self) -> usize {
        match self {
            Self::Node4 { keys, .. } | Self::Node16 { keys, .. } => keys.len(),
            Self::Node48 { nodes, .. } => nodes.len(),
            Self::Node256 { len, .. } => *len,
        }
    }

    fn get(&self, edge: u8) -> Option<&Node<V>> {
        match self {
            Self::Node4 { keys, nodes } | Self::Node16 { keys, nodes } => keys
                .binary_search(&edge)
                .ok()
                .and_then(|slot| nodes.get(slot)),
            Self::Node48 { index, nodes } => {
                let slot = index[usize::from(edge)];
                (slot != 0).then(|| &nodes[usize::from(slot - 1)])
            }
            Self::Node256 { nodes, .. } => nodes[usize::from(edge)].as_ref(),
        }
    }

    fn get_mut(&mut self, edge: u8) -> Option<&mut Node<V>> {
        match self {
            Self::Node4 { keys, nodes } | Self::Node16 { keys, nodes } => keys
                .binary_search(&edge)
                .ok()
                .and_then(|slot| nodes.get_mut(slot)),
            Self::Node48 { index, nodes } => {
                let slot = index[usize::from(edge)];
                (slot != 0).then(|| &mut nodes[usize::from(slot - 1)])
            }
            Self::Node256 { nodes, .. } => nodes[usize::from(edge)].as_mut(),
        }
    }

    fn edge_at(&self, ordinal: usize) -> Option<(u8, &Node<V>)> {
        match self {
            Self::Node4 { keys, nodes } | Self::Node16 { keys, nodes } => {
                Some((*keys.get(ordinal)?, nodes.get(ordinal)?))
            }
            Self::Node48 { index, nodes } => {
                let mut seen = 0usize;
                for edge in u8::MIN..=u8::MAX {
                    let slot = index[usize::from(edge)];
                    if slot != 0 {
                        if seen == ordinal {
                            return Some((edge, &nodes[usize::from(slot - 1)]));
                        }
                        seen += 1;
                    }
                }
                None
            }
            Self::Node256 { nodes, .. } => {
                let mut seen = 0usize;
                for edge in u8::MIN..=u8::MAX {
                    if let Some(node) = &nodes[usize::from(edge)] {
                        if seen == ordinal {
                            return Some((edge, node));
                        }
                        seen += 1;
                    }
                }
                None
            }
        }
    }

    fn only_child_ref(&self) -> Option<(u8, &Node<V>)> {
        (self.len() == 1).then(|| self.edge_at(0)).flatten()
    }

    fn try_reserve<T>(
        vec: &mut Vec<T>,
        additional: usize,
        operation: &'static str,
    ) -> Result<(), ArtError> {
        if additional == 0 {
            return Ok(());
        }
        vec.try_reserve_exact(additional)
            .map_err(|_: TryReserveError| ArtError::AllocationFailed {
                operation,
                requested: vec.len().saturating_add(additional),
            })
    }

    fn try_prepare_pair(
        keys: &mut Vec<u8>,
        nodes: &mut Vec<Node<V>>,
        capacity: usize,
        operation: &'static str,
    ) -> Result<(), ArtError> {
        let key_additional = capacity.saturating_sub(keys.len());
        let node_additional = capacity.saturating_sub(nodes.len());
        Self::try_reserve(keys, key_additional, operation)?;
        Self::try_reserve(nodes, node_additional, operation)
    }

    fn try_insert(&mut self, edge: u8, node: Node<V>) -> Result<(), ArtError> {
        match self {
            Self::Node4 { keys, nodes } if keys.len() < Self::NODE4_CAPACITY => {
                Self::try_prepare_pair(keys, nodes, Self::NODE4_CAPACITY, "growing Node4")?;
                let slot = keys.binary_search(&edge).unwrap_or_else(|slot| slot);
                keys.insert(slot, edge);
                nodes.insert(slot, node);
                return Ok(());
            }
            Self::Node16 { keys, nodes } if keys.len() < Self::NODE16_CAPACITY => {
                Self::try_prepare_pair(keys, nodes, Self::NODE16_CAPACITY, "growing Node16")?;
                let slot = keys.binary_search(&edge).unwrap_or_else(|slot| slot);
                keys.insert(slot, edge);
                nodes.insert(slot, node);
                return Ok(());
            }
            Self::Node48 { index, nodes } if nodes.len() < Self::NODE48_CAPACITY => {
                let additional = Self::NODE48_CAPACITY.saturating_sub(nodes.len());
                Self::try_reserve(nodes, additional, "growing Node48")?;
                let slot = nodes.len();
                nodes.push(node);
                index[usize::from(edge)] = encode_node48_slot(slot);
                return Ok(());
            }
            Self::Node256 { nodes, len } => {
                debug_assert_eq!(nodes.len(), Self::NODE256_CAPACITY);
                debug_assert!(nodes[usize::from(edge)].is_none());
                nodes[usize::from(edge)] = Some(node);
                *len += 1;
                return Ok(());
            }
            _ => {}
        }

        match self {
            Self::Node4 { .. } => self.try_grow_4_to_16(edge, node),
            Self::Node16 { .. } => self.try_grow_16_to_48(edge, node),
            Self::Node48 { .. } => self.try_grow_48_to_256(edge, node),
            Self::Node256 { .. } => {
                // All 256 byte edges are occupied, so callers cannot reach
                // this arm with a genuinely absent edge.
                Err(ArtError::InvariantViolation {
                    operation: "adding a 257th Node256 edge",
                })
            }
        }
    }

    fn try_grow_4_to_16(&mut self, edge: u8, node: Node<V>) -> Result<(), ArtError> {
        let mut new_keys = Vec::new();
        let mut new_nodes = Vec::new();
        Self::try_prepare_pair(
            &mut new_keys,
            &mut new_nodes,
            Self::NODE16_CAPACITY,
            "promoting Node4 to Node16",
        )?;

        let old = core::mem::replace(self, Self::empty());
        let (mut keys, mut old_nodes) = match old {
            Self::Node4 { keys, nodes } => (keys, nodes),
            other => {
                *self = other;
                return Err(ArtError::InvariantViolation {
                    operation: "promoting Node4 to Node16",
                });
            }
        };
        new_keys.append(&mut keys);
        new_nodes.append(&mut old_nodes);
        let slot = new_keys.binary_search(&edge).unwrap_or_else(|slot| slot);
        new_keys.insert(slot, edge);
        new_nodes.insert(slot, node);
        *self = Self::Node16 {
            keys: new_keys,
            nodes: new_nodes,
        };
        Ok(())
    }

    fn try_grow_16_to_48(&mut self, edge: u8, node: Node<V>) -> Result<(), ArtError> {
        let mut new_nodes = Vec::new();
        Self::try_reserve(
            &mut new_nodes,
            Self::NODE48_CAPACITY,
            "promoting Node16 to Node48",
        )?;
        let mut new_index = try_zeroed_node48_index("promoting Node16 to Node48")?;

        let old = core::mem::replace(self, Self::empty());
        let (keys, nodes) = match old {
            Self::Node16 { keys, nodes } => (keys, nodes),
            other => {
                *self = other;
                return Err(ArtError::InvariantViolation {
                    operation: "promoting Node16 to Node48",
                });
            }
        };
        for (old_edge, old_node) in keys.into_iter().zip(nodes) {
            let slot = new_nodes.len();
            new_nodes.push(old_node);
            new_index[usize::from(old_edge)] = encode_node48_slot(slot);
        }
        let slot = new_nodes.len();
        new_nodes.push(node);
        new_index[usize::from(edge)] = encode_node48_slot(slot);
        *self = Self::Node48 {
            index: new_index,
            nodes: new_nodes,
        };
        Ok(())
    }

    fn try_grow_48_to_256(&mut self, edge: u8, node: Node<V>) -> Result<(), ArtError> {
        let mut new_nodes = Vec::new();
        Self::try_reserve(
            &mut new_nodes,
            Self::NODE256_CAPACITY,
            "promoting Node48 to Node256",
        )?;
        new_nodes.resize_with(Self::NODE256_CAPACITY, || None);

        let old = core::mem::replace(self, Self::empty());
        let (index, nodes) = match old {
            Self::Node48 { index, nodes } => (index, nodes),
            other => {
                *self = other;
                return Err(ArtError::InvariantViolation {
                    operation: "promoting Node48 to Node256",
                });
            }
        };
        let old_len = nodes.len();
        let mut edges_by_slot = [0u8; 48];
        for old_edge in u8::MIN..=u8::MAX {
            let slot = index[usize::from(old_edge)];
            if slot != 0 {
                edges_by_slot[usize::from(slot - 1)] = old_edge;
            }
        }
        for (slot, old_node) in nodes.into_iter().enumerate() {
            new_nodes[usize::from(edges_by_slot[slot])] = Some(old_node);
        }
        new_nodes[usize::from(edge)] = Some(node);
        *self = Self::Node256 {
            nodes: new_nodes,
            len: old_len + 1,
        };
        Ok(())
    }

    fn remove(&mut self, edge: u8) -> Option<Node<V>> {
        let removed = match self {
            Self::Node4 { keys, nodes } | Self::Node16 { keys, nodes } => {
                let slot = keys.binary_search(&edge).ok()?;
                keys.remove(slot);
                Some(nodes.remove(slot))
            }
            Self::Node48 { index, nodes } => {
                let encoded_slot = index[usize::from(edge)];
                if encoded_slot == 0 {
                    return None;
                }
                let slot = usize::from(encoded_slot - 1);
                let old_len = nodes.len();
                index[usize::from(edge)] = 0;
                let removed = nodes.swap_remove(slot);
                if slot + 1 != old_len {
                    let moved_from = encode_node48_slot(old_len - 1);
                    for mapped in index {
                        if *mapped == moved_from {
                            *mapped = encoded_slot;
                            break;
                        }
                    }
                }
                Some(removed)
            }
            Self::Node256 { nodes, len } => {
                let removed = nodes[usize::from(edge)].take()?;
                *len -= 1;
                Some(removed)
            }
        };
        self.shrink_best_effort();
        removed
    }

    fn shrink_best_effort(&mut self) {
        if matches!(self, Self::Node16 { keys, .. } if keys.len() <= Self::NODE4_CAPACITY) {
            let old = core::mem::replace(self, Self::empty());
            if let Self::Node16 { keys, nodes } = old {
                *self = Self::Node4 { keys, nodes };
            }
            return;
        }

        if matches!(self, Self::Node48 { nodes, .. } if nodes.len() <= Self::NODE16_CAPACITY) {
            let mut new_keys = Vec::new();
            let mut new_nodes = Vec::new();
            if Self::try_prepare_pair(
                &mut new_keys,
                &mut new_nodes,
                Self::NODE16_CAPACITY,
                "demoting Node48 to Node16",
            )
            .is_err()
            {
                return;
            }
            let old = core::mem::replace(self, Self::empty());
            if let Self::Node48 { index, nodes } = old {
                let mut edges_by_slot = [0u8; 48];
                for edge in u8::MIN..=u8::MAX {
                    let slot = index[usize::from(edge)];
                    if slot != 0 {
                        edges_by_slot[usize::from(slot - 1)] = edge;
                    }
                }
                for (slot, node) in nodes.into_iter().enumerate() {
                    let edge = edges_by_slot[slot];
                    let insertion = new_keys.binary_search(&edge).unwrap_or_else(|at| at);
                    new_keys.insert(insertion, edge);
                    new_nodes.insert(insertion, node);
                }
                *self = Self::Node16 {
                    keys: new_keys,
                    nodes: new_nodes,
                };
            }
            return;
        }

        if matches!(self, Self::Node256 { len, .. } if *len <= Self::NODE48_CAPACITY) {
            let mut compact_nodes = Vec::new();
            if Self::try_reserve(
                &mut compact_nodes,
                Self::NODE48_CAPACITY,
                "demoting Node256 to Node48",
            )
            .is_err()
            {
                return;
            }
            let Ok(mut index) = try_zeroed_node48_index("demoting Node256 to Node48") else {
                return;
            };
            let old = core::mem::replace(self, Self::empty());
            if let Self::Node256 { nodes, len: _ } = old {
                for (edge, maybe_node) in nodes.into_iter().enumerate() {
                    if let Some(node) = maybe_node {
                        let slot = compact_nodes.len();
                        compact_nodes.push(node);
                        index[edge] = encode_node48_slot(slot);
                    }
                }
                *self = Self::Node48 {
                    index,
                    nodes: compact_nodes,
                };
                self.shrink_best_effort();
            }
        }
    }

    fn take_only_child(&mut self) -> Option<(u8, Node<V>)> {
        let edge = self.only_child_ref()?.0;
        self.remove(edge).map(|node| (edge, node))
    }
}

impl<V> Entry<V> {
    fn try_new(key: &[u8], value: V) -> Result<Self, ArtError> {
        Ok(Self {
            key: try_copy_bytes(key, "copying an ART key")?,
            value,
        })
    }
}

impl<V> Node<V> {
    fn try_leaf(prefix: &[u8], full_key: &[u8], value: V) -> Result<Self, ArtError> {
        Ok(Self {
            prefix: try_copy_bytes(prefix, "copying an ART leaf prefix")?,
            entry: Some(Entry::try_new(full_key, value)?),
            children: Children::empty(),
        })
    }

    fn get(&self, remaining: &[u8]) -> Option<&V> {
        if !remaining.starts_with(&self.prefix) {
            return None;
        }
        let remaining = &remaining[self.prefix.len()..];
        if remaining.is_empty() {
            return self.entry.as_ref().map(|entry| &entry.value);
        }
        self.children
            .get(remaining[0])
            .and_then(|child| child.get(&remaining[1..]))
    }

    fn get_mut(&mut self, remaining: &[u8]) -> Option<&mut V> {
        if !remaining.starts_with(&self.prefix) {
            return None;
        }
        let remaining = &remaining[self.prefix.len()..];
        if remaining.is_empty() {
            return self.entry.as_mut().map(|entry| &mut entry.value);
        }
        self.children
            .get_mut(remaining[0])
            .and_then(|child| child.get_mut(&remaining[1..]))
    }

    fn find_prefix_node(&self, remaining: &[u8]) -> Option<&Self> {
        let common = common_prefix_len(&self.prefix, remaining);
        if common == remaining.len() {
            // The requested prefix can finish in the middle of this node's
            // compressed prefix; every entry below it still matches.
            return Some(self);
        }
        if common != self.prefix.len() {
            return None;
        }
        let remaining = &remaining[common..];
        self.children
            .get(remaining[0])
            .and_then(|child| child.find_prefix_node(&remaining[1..]))
    }

    fn try_insert(
        &mut self,
        remaining: &[u8],
        full_key: &[u8],
        value: V,
    ) -> Result<Option<V>, ArtError> {
        let common = common_prefix_len(&self.prefix, remaining);
        if common != self.prefix.len() {
            return self.try_split_and_insert(common, remaining, full_key, value);
        }

        let remaining = &remaining[common..];
        if remaining.is_empty() {
            if let Some(entry) = &mut self.entry {
                return Ok(Some(core::mem::replace(&mut entry.value, value)));
            }
            self.entry = Some(Entry::try_new(full_key, value)?);
            return Ok(None);
        }

        let edge = remaining[0];
        if let Some(child) = self.children.get_mut(edge) {
            return child.try_insert(&remaining[1..], full_key, value);
        }

        let leaf = Self::try_leaf(&remaining[1..], full_key, value)?;
        self.children.try_insert(edge, leaf)?;
        Ok(None)
    }

    fn try_split_and_insert(
        &mut self,
        common: usize,
        remaining: &[u8],
        full_key: &[u8],
        value: V,
    ) -> Result<Option<V>, ArtError> {
        debug_assert!(common < self.prefix.len());
        let old_edge = self.prefix[common];
        let parent_prefix = try_copy_bytes(&self.prefix[..common], "splitting an ART prefix")?;
        let old_suffix = try_copy_bytes(&self.prefix[common + 1..], "copying a split ART suffix")?;

        let (parent_entry, new_child) = if common == remaining.len() {
            (Some(Entry::try_new(full_key, value)?), None)
        } else {
            let new_edge = remaining[common];
            let leaf = Self::try_leaf(&remaining[common + 1..], full_key, value)?;
            (None, Some((new_edge, leaf)))
        };

        // Reserve all storage before moving the existing node. After this
        // point the split cannot fail and insertion remains logically atomic.
        let child_count = 1 + usize::from(new_child.is_some());
        let mut keys = Vec::new();
        let mut nodes = Vec::new();
        Children::<V>::try_reserve(&mut keys, child_count, "allocating split edges")?;
        Children::<V>::try_reserve(&mut nodes, child_count, "allocating split nodes")?;

        let parent = Self {
            prefix: parent_prefix,
            entry: parent_entry,
            children: Children::Node4 { keys, nodes },
        };
        let mut old_node = core::mem::replace(self, parent);
        old_node.prefix = old_suffix;

        let Children::Node4 { keys, nodes } = &mut self.children else {
            return Err(ArtError::InvariantViolation {
                operation: "materializing a split Node4 parent",
            });
        };
        if let Some((new_edge, leaf)) = new_child {
            if new_edge < old_edge {
                keys.push(new_edge);
                nodes.push(leaf);
                keys.push(old_edge);
                nodes.push(old_node);
            } else {
                keys.push(old_edge);
                nodes.push(old_node);
                keys.push(new_edge);
                nodes.push(leaf);
            }
        } else {
            keys.push(old_edge);
            nodes.push(old_node);
        }
        Ok(None)
    }

    fn remove(&mut self, remaining: &[u8]) -> Option<V> {
        if !remaining.starts_with(&self.prefix) {
            return None;
        }
        let remaining = &remaining[self.prefix.len()..];
        let removed = if remaining.is_empty() {
            self.entry.take().map(|entry| entry.value)
        } else {
            let edge = remaining[0];
            let (removed, child_became_empty) = {
                let child = self.children.get_mut(edge)?;
                let removed = child.remove(&remaining[1..]);
                let empty = child.entry.is_none() && child.children.len() == 0;
                (removed, empty)
            };
            if child_became_empty {
                let _ = self.children.remove(edge);
            }
            removed
        };

        if removed.is_some() {
            self.recompress_unary_best_effort();
        }
        removed
    }

    fn recompress_unary_best_effort(&mut self) {
        let Some((_, child)) = self.children.only_child_ref() else {
            return;
        };
        if self.entry.is_some() {
            return;
        }
        let additional = child.prefix.len().saturating_add(1);
        if self
            .prefix
            .try_reserve_exact(additional)
            .map_err(|_: TryReserveError| ())
            .is_err()
        {
            // Compression is a representation optimization. Keeping a valid
            // unary node is preferable to making successful deletion fallible
            // after the logical value has already been removed.
            return;
        }
        let Some((edge, mut child)) = self.children.take_only_child() else {
            return;
        };
        self.prefix.push(edge);
        self.prefix.append(&mut child.prefix);
        self.entry = child.entry;
        self.children = child.children;
    }

    fn record_histogram(&self, histogram: &mut NodeKindHistogram) {
        // A terminal leaf owns an empty Node4-shaped container for lazy
        // expansion, but that container is not an ART inner node and must not
        // inflate diagnostic representation counts.
        if self.children.len() != 0 {
            histogram.record(self.children.kind());
        }
        for ordinal in 0..self.children.len() {
            if let Some((_, child)) = self.children.edge_at(ordinal) {
                child.record_histogram(histogram);
            }
        }
    }
}

fn common_prefix_len(left: &[u8], right: &[u8]) -> usize {
    left.iter()
        .zip(right)
        .take_while(|(left, right)| left == right)
        .count()
}

fn encode_node48_slot(slot: usize) -> u8 {
    debug_assert!(slot < 48);
    (slot as u8) + 1
}

fn try_zeroed_node48_index(operation: &'static str) -> Result<Vec<u8>, ArtError> {
    let mut index = Vec::new();
    index
        .try_reserve_exact(256)
        .map_err(|_: TryReserveError| ArtError::AllocationFailed {
            operation,
            requested: 256,
        })?;
    index.resize(256, 0);
    Ok(index)
}

fn try_copy_bytes(bytes: &[u8], operation: &'static str) -> Result<Vec<u8>, ArtError> {
    let mut copy = Vec::new();
    copy.try_reserve_exact(bytes.len())
        .map_err(|_: TryReserveError| ArtError::AllocationFailed {
            operation,
            requested: bytes.len(),
        })?;
    copy.extend_from_slice(bytes);
    Ok(copy)
}

/// A safe, generic adaptive radix map keyed by arbitrary bytes.
pub struct AdaptiveRadixTree<V> {
    root: Option<Node<V>>,
    len: usize,
    limits: ArtLimits,
}

/// Concise alias for callers that prefer a map-shaped name.
pub type ArtMap<V> = AdaptiveRadixTree<V>;

impl<V> Default for AdaptiveRadixTree<V> {
    fn default() -> Self {
        Self::new()
    }
}

impl<V> AdaptiveRadixTree<V> {
    /// Creates an empty tree with conservative default resource limits.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            root: None,
            len: 0,
            limits: ArtLimits {
                max_key_bytes: DEFAULT_MAX_KEY_BYTES,
                max_entries: DEFAULT_MAX_ENTRIES,
            },
        }
    }

    /// Creates an empty tree with caller-selected resource limits.
    #[must_use]
    pub const fn with_limits(limits: ArtLimits) -> Self {
        Self {
            root: None,
            len: 0,
            limits,
        }
    }

    /// Returns the limits enforced by this tree.
    #[must_use]
    pub const fn limits(&self) -> ArtLimits {
        self.limits
    }

    /// Returns the number of distinct keys.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Returns whether the tree contains no entries.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Inserts a key or replaces its existing value.
    ///
    /// All allocations needed for a new mapping use bounded, fallible
    /// reservations. Replacing an existing value does not grow the tree.
    pub fn insert<K>(&mut self, key: K, value: V) -> Result<Option<V>, ArtError>
    where
        K: AsRef<[u8]>,
    {
        self.try_insert(key, value)
    }

    /// Explicitly named alias for [`Self::insert`].
    pub fn try_insert<K>(&mut self, key: K, value: V) -> Result<Option<V>, ArtError>
    where
        K: AsRef<[u8]>,
    {
        let key = key.as_ref();
        if key.len() > self.limits.max_key_bytes {
            return Err(ArtError::KeyTooLong {
                len: key.len(),
                max: self.limits.max_key_bytes,
            });
        }

        let exists = self.get(key).is_some();
        if !exists && self.len >= self.limits.max_entries {
            return Err(ArtError::EntryLimitReached {
                max: self.limits.max_entries,
            });
        }

        let replaced = match &mut self.root {
            Some(root) => root.try_insert(key, key, value)?,
            None => {
                self.root = Some(Node::try_leaf(key, key, value)?);
                None
            }
        };
        if !exists {
            self.len += 1;
        }
        Ok(replaced)
    }

    /// Returns a shared reference to a key's value.
    #[must_use]
    pub fn get<K>(&self, key: K) -> Option<&V>
    where
        K: AsRef<[u8]>,
    {
        self.root.as_ref()?.get(key.as_ref())
    }

    /// Returns an exclusive reference to a key's value.
    pub fn get_mut<K>(&mut self, key: K) -> Option<&mut V>
    where
        K: AsRef<[u8]>,
    {
        self.root.as_mut()?.get_mut(key.as_ref())
    }

    /// Returns whether a key has a mapping.
    #[must_use]
    pub fn contains_key<K>(&self, key: K) -> bool
    where
        K: AsRef<[u8]>,
    {
        self.get(key).is_some()
    }

    /// Removes a key and returns its value.
    ///
    /// Unary paths are recompressed after removal when the prefix buffer can
    /// be extended. If the allocator refuses that optional reservation, the
    /// logically equivalent uncompressed path remains valid.
    pub fn remove<K>(&mut self, key: K) -> Option<V>
    where
        K: AsRef<[u8]>,
    {
        let removed = self.root.as_mut()?.remove(key.as_ref());
        if removed.is_some() {
            self.len -= 1;
            let root_is_empty = self
                .root
                .as_ref()
                .is_some_and(|root| root.entry.is_none() && root.children.len() == 0);
            if root_is_empty {
                self.root = None;
            }
        }
        removed
    }

    /// Iterates over every mapping in lexicographic byte-key order.
    #[must_use]
    pub fn iter(&self) -> ArtIter<'_, V> {
        ArtIter::new(self.root.as_ref())
    }

    /// Iterates over mappings whose keys start with `prefix`.
    #[must_use]
    pub fn prefix<K>(&self, prefix: K) -> ArtIter<'_, V>
    where
        K: AsRef<[u8]>,
    {
        self.prefix_iter(prefix)
    }

    /// Explicitly named alias for [`Self::prefix`].
    #[must_use]
    pub fn prefix_iter<K>(&self, prefix: K) -> ArtIter<'_, V>
    where
        K: AsRef<[u8]>,
    {
        let node = self
            .root
            .as_ref()
            .and_then(|root| root.find_prefix_node(prefix.as_ref()));
        ArtIter::new(node)
    }

    /// Iterates over mappings in a standard lexicographic byte range.
    ///
    /// Byte-slice bounds can be supplied without allocating:
    ///
    /// ```
    /// use core::ops::Bound;
    /// use fgdb_collections::art::AdaptiveRadixTree;
    ///
    /// let mut tree = AdaptiveRadixTree::new();
    /// assert_eq!(tree.insert(b"ant", 1), Ok(None));
    /// assert_eq!(tree.insert(b"bee", 2), Ok(None));
    /// let entries: Vec<_> = tree
    ///     .range((
    ///         Bound::Included(&b"ant"[..]),
    ///         Bound::Excluded(&b"cat"[..]),
    ///     ))
    ///     .collect();
    /// assert_eq!(entries.len(), 2);
    /// ```
    pub fn range<R>(&self, bounds: R) -> ArtRange<'_, V, R>
    where
        R: RangeBounds<[u8]>,
    {
        ArtRange {
            inner: self.iter(),
            bounds,
            exhausted: false,
        }
    }

    /// Reports the current adaptive-node representation mix.
    #[must_use]
    pub fn node_kind_histogram(&self) -> NodeKindHistogram {
        let mut histogram = NodeKindHistogram::default();
        if let Some(root) = &self.root {
            root.record_histogram(&mut histogram);
        }
        histogram
    }
}

impl<V: fmt::Debug> fmt::Debug for AdaptiveRadixTree<V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map().entries(self.iter()).finish()
    }
}

impl<'tree, V> IntoIterator for &'tree AdaptiveRadixTree<V> {
    type Item = (&'tree [u8], &'tree V);
    type IntoIter = ArtIter<'tree, V>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

struct IterFrame<'tree, V> {
    node: &'tree Node<V>,
    yielded_entry: bool,
    next_child: usize,
}

/// Lexicographically ordered iterator over ART mappings.
pub struct ArtIter<'tree, V> {
    stack: Vec<IterFrame<'tree, V>>,
}

impl<'tree, V> ArtIter<'tree, V> {
    fn new(root: Option<&'tree Node<V>>) -> Self {
        let mut stack = Vec::new();
        if let Some(node) = root {
            stack.push(IterFrame {
                node,
                yielded_entry: false,
                next_child: 0,
            });
        }
        Self { stack }
    }
}

impl<'tree, V> Iterator for ArtIter<'tree, V> {
    type Item = (&'tree [u8], &'tree V);

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let frame = self.stack.last_mut()?;
            if !frame.yielded_entry {
                frame.yielded_entry = true;
                if let Some(entry) = &frame.node.entry {
                    return Some((&entry.key, &entry.value));
                }
            }

            if frame.next_child < frame.node.children.len() {
                let ordinal = frame.next_child;
                frame.next_child += 1;
                if let Some((_, child)) = frame.node.children.edge_at(ordinal) {
                    self.stack.push(IterFrame {
                        node: child,
                        yielded_entry: false,
                        next_child: 0,
                    });
                }
            } else {
                self.stack.pop();
            }
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (0, None)
    }
}

/// Ordered iterator over a bounded subset of an ART.
pub struct ArtRange<'tree, V, R> {
    inner: ArtIter<'tree, V>,
    bounds: R,
    exhausted: bool,
}

impl<'tree, V, R> Iterator for ArtRange<'tree, V, R>
where
    R: RangeBounds<[u8]>,
{
    type Item = (&'tree [u8], &'tree V);

    fn next(&mut self) -> Option<Self::Item> {
        if self.exhausted {
            return None;
        }
        for (key, value) in self.inner.by_ref() {
            let above_start = match self.bounds.start_bound() {
                Bound::Included(start) => key >= start,
                Bound::Excluded(start) => key > start,
                Bound::Unbounded => true,
            };
            if !above_start {
                continue;
            }
            let below_end = match self.bounds.end_bound() {
                Bound::Included(end) => key <= end,
                Bound::Excluded(end) => key < end,
                Bound::Unbounded => true,
            };
            if below_end {
                return Some((key, value));
            }
            self.exhausted = true;
            return None;
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn owned_entries(tree: &AdaptiveRadixTree<u64>) -> Vec<(Vec<u8>, u64)> {
        tree.iter()
            .map(|(key, value)| (key.to_vec(), *value))
            .collect()
    }

    fn reference_entries(reference: &BTreeMap<Vec<u8>, u64>) -> Vec<(Vec<u8>, u64)> {
        reference
            .iter()
            .map(|(key, value)| (key.clone(), *value))
            .collect()
    }

    #[test]
    fn empty_key_replacement_and_mutation_are_map_compatible() {
        let mut tree = AdaptiveRadixTree::new();
        assert!(tree.is_empty());
        assert_eq!(tree.insert([], 7), Ok(None));
        assert_eq!(tree.insert(b"a", 11), Ok(None));
        assert_eq!(tree.insert(b"ab", 13), Ok(None));
        assert_eq!(tree.insert([], 17), Ok(Some(7)));
        assert_eq!(tree.len(), 3);
        assert_eq!(tree.get([]), Some(&17));
        assert_eq!(tree.get(b"a"), Some(&11));
        assert_eq!(tree.get(b"ab"), Some(&13));
        assert_eq!(tree.get(b"missing"), None);

        if let Some(value) = tree.get_mut(b"a") {
            *value += 100;
        }
        assert_eq!(tree.get(b"a"), Some(&111));
        assert!(tree.contains_key([]));
        assert_eq!(
            owned_entries(&tree),
            vec![(vec![], 17), (b"a".to_vec(), 111), (b"ab".to_vec(), 13)]
        );
    }

    #[test]
    fn copied_keys_are_independent_of_caller_storage() {
        let mut tree = AdaptiveRadixTree::new();
        let mut key = b"stable-key".to_vec();
        assert_eq!(tree.insert(&key, 1), Ok(None));
        key.fill(b'x');
        assert_eq!(tree.get(b"stable-key"), Some(&1));
        assert_eq!(tree.get(&key), None);
        assert_eq!(tree.node_kind_histogram(), NodeKindHistogram::default());
    }

    #[test]
    fn fanout_grows_and_shrinks_through_every_art_representation() {
        let mut tree = AdaptiveRadixTree::new();
        for edge in u8::MIN..=u8::MAX {
            assert_eq!(tree.insert([edge], u64::from(edge)), Ok(None));
            let expected_kind = match tree.len() {
                0..=4 => NodeKind::Node4,
                5..=16 => NodeKind::Node16,
                17..=48 => NodeKind::Node48,
                _ => NodeKind::Node256,
            };
            assert_eq!(
                tree.root.as_ref().map(|root| root.children.kind()),
                Some(expected_kind)
            );
        }
        assert_eq!(tree.len(), 256);
        assert_eq!(
            tree.node_kind_histogram(),
            NodeKindHistogram {
                node4: 0,
                node16: 0,
                node48: 0,
                node256: 1,
            }
        );
        for edge in u8::MIN..=u8::MAX {
            assert_eq!(tree.get([edge]), Some(&u64::from(edge)));
        }

        for edge in (48u8..=u8::MAX).rev() {
            assert_eq!(tree.remove([edge]), Some(u64::from(edge)));
        }
        assert_eq!(tree.len(), 48);
        assert_eq!(
            tree.root.as_ref().map(|root| root.children.kind()),
            Some(NodeKind::Node48)
        );

        for edge in (16u8..48).rev() {
            assert_eq!(tree.remove([edge]), Some(u64::from(edge)));
        }
        assert_eq!(
            tree.root.as_ref().map(|root| root.children.kind()),
            Some(NodeKind::Node16)
        );

        for edge in (4u8..16).rev() {
            assert_eq!(tree.remove([edge]), Some(u64::from(edge)));
        }
        assert_eq!(
            tree.root.as_ref().map(|root| root.children.kind()),
            Some(NodeKind::Node4)
        );
        assert_eq!(
            owned_entries(&tree),
            vec![(vec![0], 0), (vec![1], 1), (vec![2], 2), (vec![3], 3)]
        );
        assert_eq!(
            tree.node_kind_histogram(),
            NodeKindHistogram {
                node4: 1,
                node16: 0,
                node48: 0,
                node256: 0,
            }
        );
    }

    #[test]
    fn node48_swap_removal_preserves_every_other_edge_mapping() {
        let mut tree = AdaptiveRadixTree::new();
        for edge in 0u8..40 {
            assert_eq!(tree.insert([edge], u64::from(edge) * 3), Ok(None));
        }
        for edge in [7u8, 0, 31, 18, 39, 5, 22] {
            assert_eq!(tree.remove([edge]), Some(u64::from(edge) * 3));
        }
        for edge in 0u8..40 {
            let expected = if [7u8, 0, 31, 18, 39, 5, 22].contains(&edge) {
                None
            } else {
                Some(&(u64::from(edge) * 3))
            };
            assert_eq!(tree.get([edge]), expected);
        }
        assert_eq!(
            tree.root.as_ref().map(|root| root.children.kind()),
            Some(NodeKind::Node48)
        );
    }

    #[test]
    fn shared_prefix_splits_and_removal_recompresses_paths() {
        let mut tree = AdaptiveRadixTree::new();
        let fixtures = [
            (&b"prefix-alpha"[..], 1),
            (&b"prefix-alpine"[..], 2),
            (&b"prefix"[..], 3),
            (&b"prefix-beta"[..], 4),
            (&b"prefix-betamax"[..], 5),
            (&b"prefix-\0"[..], 6),
            (&b"prefix-\xff"[..], 7),
        ];
        for (key, value) in fixtures {
            assert_eq!(tree.insert(key, value), Ok(None));
        }
        for (key, value) in fixtures {
            assert_eq!(tree.get(key), Some(&value));
        }

        assert_eq!(tree.remove(b"prefix"), Some(3));
        assert_eq!(tree.remove(b"prefix-alpha"), Some(1));
        assert_eq!(tree.remove(b"prefix-alpine"), Some(2));
        assert_eq!(tree.remove(b"prefix-beta"), Some(4));
        assert_eq!(tree.remove(b"prefix-betamax"), Some(5));
        assert_eq!(tree.remove(b"prefix-\0"), Some(6));
        assert_eq!(tree.len(), 1);
        assert_eq!(tree.get(b"prefix-\xff"), Some(&7));
        assert_eq!(
            tree.root.as_ref().map(|root| root.prefix.as_slice()),
            Some(&b"prefix-\xff"[..])
        );
        assert_eq!(tree.remove(b"prefix-\xff"), Some(7));
        assert!(tree.is_empty());
        assert_eq!(tree.node_kind_histogram(), NodeKindHistogram::default());
    }

    #[test]
    fn prefix_iteration_handles_prefixes_ending_inside_compressed_paths() {
        let mut tree = AdaptiveRadixTree::new();
        for (index, key) in [
            &b"alphabet"[..],
            &b"alpha-numeric"[..],
            &b"alpine"[..],
            &b"beta"[..],
            &b"betamax"[..],
        ]
        .into_iter()
        .enumerate()
        {
            assert_eq!(tree.insert(key, index as u64), Ok(None));
        }

        let alpha: Vec<_> = tree.prefix(b"alph").map(|(key, _)| key.to_vec()).collect();
        assert_eq!(alpha, vec![b"alpha-numeric".to_vec(), b"alphabet".to_vec()]);
        let bet: Vec<_> = tree
            .prefix_iter(b"bet")
            .map(|(key, _)| key.to_vec())
            .collect();
        assert_eq!(bet, vec![b"beta".to_vec(), b"betamax".to_vec()]);
        assert_eq!(tree.prefix(b"z").next(), None);
        assert_eq!(tree.prefix([]).count(), tree.len());
    }

    #[test]
    fn ordered_range_matches_btree_map_for_all_bound_forms() {
        let mut tree = AdaptiveRadixTree::new();
        let mut reference = BTreeMap::new();
        for (index, key) in [
            &b""[..],
            &b"a"[..],
            &b"aa"[..],
            &b"ab"[..],
            &b"b"[..],
            &b"ba"[..],
            &b"\x80"[..],
            &b"\xff"[..],
        ]
        .into_iter()
        .enumerate()
        {
            assert_eq!(tree.insert(key, index as u64), Ok(None));
            reference.insert(key.to_vec(), index as u64);
        }

        let ranges = [
            (Bound::Unbounded, Bound::Unbounded),
            (Bound::Included(&b"a"[..]), Bound::Excluded(&b"b"[..])),
            (Bound::Excluded(&b"a"[..]), Bound::Included(&b"ba"[..])),
            (Bound::Included(&b"\x00"[..]), Bound::Included(&b"\xff"[..])),
        ];
        for bounds in ranges {
            let actual: Vec<_> = tree
                .range(bounds)
                .map(|(key, value)| (key.to_vec(), *value))
                .collect();
            let expected: Vec<_> = reference
                .range::<[u8], _>(bounds)
                .map(|(key, value)| (key.clone(), *value))
                .collect();
            assert_eq!(actual, expected);
        }
    }

    #[test]
    fn configured_limits_reject_growth_but_allow_replacement() {
        let mut tree = AdaptiveRadixTree::with_limits(ArtLimits {
            max_key_bytes: 3,
            max_entries: 2,
        });
        assert_eq!(tree.insert(b"a", 1), Ok(None));
        assert_eq!(tree.insert(b"bbb", 2), Ok(None));
        assert_eq!(
            tree.insert(b"long", 3),
            Err(ArtError::KeyTooLong { len: 4, max: 3 })
        );
        assert_eq!(
            tree.insert(b"cc", 3),
            Err(ArtError::EntryLimitReached { max: 2 })
        );
        assert_eq!(tree.insert(b"a", 4), Ok(Some(1)));
        assert_eq!(tree.get(b"a"), Some(&4));
        assert_eq!(tree.len(), 2);
    }

    struct DeterministicRng(u64);

    impl DeterministicRng {
        fn next(&mut self) -> u64 {
            let mut value = self.0;
            value ^= value << 13;
            value ^= value >> 7;
            value ^= value << 17;
            self.0 = value;
            value
        }

        fn key(&mut self) -> Vec<u8> {
            let len = (self.next() % 18) as usize;
            let mut key = Vec::with_capacity(len + 7);
            if self.next().is_multiple_of(3) {
                key.extend_from_slice(b"shared/");
            }
            for _ in 0..len {
                key.push(self.next().to_le_bytes()[0]);
            }
            key
        }
    }

    #[test]
    fn deterministic_operation_sequences_match_btree_map() {
        const SEED: u64 = 0xd1b5_4a32_d192_ed03;
        let mut rng = DeterministicRng(SEED);
        let mut tree = AdaptiveRadixTree::new();
        let mut reference = BTreeMap::<Vec<u8>, u64>::new();

        for step in 0u64..10_000 {
            let key = rng.key();
            match rng.next() % 5 {
                0 | 1 => {
                    let value = rng.next() ^ step;
                    let expected = reference.insert(key.clone(), value);
                    assert_eq!(
                        tree.insert(&key, value),
                        Ok(expected),
                        "seed={SEED} step={step}"
                    );
                }
                2 => {
                    assert_eq!(
                        tree.remove(&key),
                        reference.remove(&key),
                        "seed={SEED} step={step}"
                    );
                }
                3 => {
                    assert_eq!(
                        tree.get(&key).copied(),
                        reference.get(&key).copied(),
                        "seed={SEED} step={step}"
                    );
                }
                _ => {
                    let delta = rng.next() & 0xff;
                    if let Some(value) = tree.get_mut(&key) {
                        *value = value.wrapping_add(delta);
                    }
                    if let Some(value) = reference.get_mut(&key) {
                        *value = value.wrapping_add(delta);
                    }
                }
            }

            assert_eq!(tree.len(), reference.len(), "seed={SEED} step={step}");
            if step.is_multiple_of(37) {
                assert_eq!(
                    owned_entries(&tree),
                    reference_entries(&reference),
                    "seed={SEED} step={step}"
                );
            }
        }
        assert_eq!(owned_entries(&tree), reference_entries(&reference));
    }
}
