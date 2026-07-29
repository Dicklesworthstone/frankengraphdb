//! A deterministic, byte-keyed adaptive radix tree.
//!
//! This module implements the classical ART fan-out progression
//! `Node4 -> Node16 -> Node48 -> Node256`. Edges are kept in byte order and
//! unary paths are compressed into node prefixes. The tree is an in-memory
//! collection: no representation in this module is a durable byte format.

use core::fmt;
use core::ops::{Bound, RangeBounds};
use std::collections::TryReserveError;

use fgdb_types::QueryCx;
use fgdb_unsafe_arena::{RegionScope, RegionVec, RegionVecError};

use crate::levenshtein::{LevenshteinAutomaton, LevenshteinError, LevenshteinState};

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
    /// Task-local region storage refused a resident ART buffer.
    Region {
        operation: &'static str,
        source: RegionVecError,
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
            Self::Region { operation, source } => {
                write!(f, "ART region storage refused while {operation}: {source}")
            }
            Self::InvariantViolation { operation } => {
                write!(f, "ART representation invariant failed while {operation}")
            }
        }
    }
}

impl std::error::Error for ArtError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Region { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// Bounded scratch policy for an ART/Levenshtein product walk.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ArtLevenshteinLimits {
    /// Maximum number of live traversal frames.
    ///
    /// One frame retains one automaton row and one ART node reference.
    pub max_stack_frames: usize,
}

impl Default for ArtLevenshteinLimits {
    fn default() -> Self {
        Self {
            max_stack_frames: 1_024,
        }
    }
}

/// A checked ART/Levenshtein product-traversal failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArtLevenshteinError {
    /// Constructing or advancing the bounded automaton state failed.
    Automaton(LevenshteinError),
    /// Reserving the caller-bounded traversal frame stack failed.
    TraversalAllocationFailed { requested_frames: usize },
    /// A matching path needs more live frames than the caller allowed.
    TraversalDepthLimitExceeded { max_stack_frames: usize },
    /// The ART's private child representation violated its own contract.
    RepresentationInvariant { operation: &'static str },
}

impl fmt::Display for ArtLevenshteinError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Automaton(error) => write!(f, "ART Levenshtein state failed: {error}"),
            Self::TraversalAllocationFailed { requested_frames } => write!(
                f,
                "ART Levenshtein traversal could not reserve {requested_frames} frames"
            ),
            Self::TraversalDepthLimitExceeded { max_stack_frames } => write!(
                f,
                "ART Levenshtein traversal exceeded its {max_stack_frames}-frame limit"
            ),
            Self::RepresentationInvariant { operation } => {
                write!(f, "ART representation invariant failed while {operation}")
            }
        }
    }
}

impl std::error::Error for ArtLevenshteinError {}

impl From<LevenshteinError> for ArtLevenshteinError {
    fn from(error: LevenshteinError) -> Self {
        Self::Automaton(error)
    }
}

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

struct Entry<'region, V> {
    key: RegionVec<'region, u8>,
    value: V,
}

struct Node<'region, V> {
    /// Bytes after the incoming parent edge. At the root this is the initial
    /// common prefix of every key.
    prefix: RegionVec<'region, u8>,
    entry: Option<Entry<'region, V>>,
    children: Children<'region, V>,
}

enum Children<'region, V> {
    Node4 {
        keys: RegionVec<'region, u8>,
        nodes: RegionVec<'region, Option<Node<'region, V>>>,
    },
    Node16 {
        keys: RegionVec<'region, u8>,
        nodes: RegionVec<'region, Option<Node<'region, V>>>,
    },
    Node48 {
        /// Zero means absent; otherwise the stored value is slot + 1.
        index: RegionVec<'region, u8>,
        nodes: RegionVec<'region, Option<Node<'region, V>>>,
        len: usize,
    },
    Node256 {
        nodes: RegionVec<'region, Option<Node<'region, V>>>,
        len: usize,
    },
}

impl<'region, V> Children<'region, V> {
    const NODE4_CAPACITY: usize = 4;
    const NODE16_CAPACITY: usize = 16;
    const NODE48_CAPACITY: usize = 48;
    const NODE256_CAPACITY: usize = 256;

    fn empty(scope: &'region RegionScope) -> Result<Self, ArtError> {
        Ok(Self::Node4 {
            keys: RegionVec::new_in(scope).map_err(|source| ArtError::Region {
                operation: "opening empty ART child edges",
                source,
            })?,
            nodes: RegionVec::new_in(scope).map_err(|source| ArtError::Region {
                operation: "opening empty ART child nodes",
                source,
            })?,
        })
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
            Self::Node48 { len, .. } | Self::Node256 { len, .. } => *len,
        }
    }

    fn get(&self, edge: u8) -> Option<&Node<'region, V>> {
        match self {
            Self::Node4 { keys, nodes } | Self::Node16 { keys, nodes } => keys
                .as_slice()
                .binary_search(&edge)
                .ok()
                .and_then(|slot| nodes.get(slot))
                .and_then(Option::as_ref),
            Self::Node48 { index, nodes, .. } => {
                let slot = *index.get(usize::from(edge))?;
                (slot != 0)
                    .then(|| nodes.get(usize::from(slot - 1)).and_then(Option::as_ref))
                    .flatten()
            }
            Self::Node256 { nodes, .. } => nodes.get(usize::from(edge)).and_then(Option::as_ref),
        }
    }

    fn get_mut(&mut self, edge: u8) -> Option<&mut Node<'region, V>> {
        match self {
            Self::Node4 { keys, nodes } | Self::Node16 { keys, nodes } => keys
                .as_slice()
                .binary_search(&edge)
                .ok()
                .and_then(|slot| nodes.get_mut(slot))
                .and_then(Option::as_mut),
            Self::Node48 { index, nodes, .. } => {
                let slot = *index.get(usize::from(edge))?;
                (slot != 0)
                    .then(|| {
                        nodes
                            .get_mut(usize::from(slot - 1))
                            .and_then(Option::as_mut)
                    })
                    .flatten()
            }
            Self::Node256 { nodes, .. } => {
                nodes.get_mut(usize::from(edge)).and_then(Option::as_mut)
            }
        }
    }

    fn edge_at(&self, ordinal: usize) -> Option<(u8, &Node<'region, V>)> {
        match self {
            Self::Node4 { keys, nodes } | Self::Node16 { keys, nodes } => {
                Some((*keys.get(ordinal)?, nodes.get(ordinal)?.as_ref()?))
            }
            Self::Node48 { index, nodes, .. } => {
                let mut seen = 0usize;
                for edge in u8::MIN..=u8::MAX {
                    let slot = *index.get(usize::from(edge))?;
                    if slot != 0 {
                        if seen == ordinal {
                            return Some((
                                edge,
                                nodes.get(usize::from(slot - 1)).and_then(Option::as_ref)?,
                            ));
                        }
                        seen += 1;
                    }
                }
                None
            }
            Self::Node256 { nodes, .. } => {
                let mut seen = 0usize;
                for edge in u8::MIN..=u8::MAX {
                    if let Some(node) = nodes.get(usize::from(edge)).and_then(Option::as_ref) {
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

    fn only_child_ref(&self) -> Option<(u8, &Node<'region, V>)> {
        (self.len() == 1).then(|| self.edge_at(0)).flatten()
    }

    fn try_reserve<T>(
        vec: &mut RegionVec<'region, T>,
        cx: &QueryCx,
        additional: usize,
        operation: &'static str,
    ) -> Result<(), ArtError> {
        if additional == 0 {
            return Ok(());
        }
        vec.try_reserve_exact(cx, additional)
            .map_err(|source| ArtError::Region { operation, source })
    }

    fn try_prepare_pair(
        keys: &mut RegionVec<'region, u8>,
        nodes: &mut RegionVec<'region, Option<Node<'region, V>>>,
        cx: &QueryCx,
        capacity: usize,
        operation: &'static str,
    ) -> Result<(), ArtError> {
        let key_additional = capacity.saturating_sub(keys.len());
        let node_additional = capacity.saturating_sub(nodes.len());
        Self::try_reserve(keys, cx, key_additional, operation)?;
        Self::try_reserve(nodes, cx, node_additional, operation)
    }

    fn try_insert_pair(
        keys: &mut RegionVec<'region, u8>,
        nodes: &mut RegionVec<'region, Option<Node<'region, V>>>,
        cx: &QueryCx,
        capacity: usize,
        edge: u8,
        node: Node<'region, V>,
        operation: &'static str,
    ) -> Result<(), ArtError> {
        Self::try_prepare_pair(keys, nodes, cx, capacity, operation)?;
        let slot = keys
            .as_slice()
            .binary_search(&edge)
            .unwrap_or_else(|slot| slot);
        keys.try_insert(cx, slot, edge)
            .map_err(|source| ArtError::Region { operation, source })?;
        if let Err(source) = nodes.try_insert(cx, slot, Some(node)) {
            let _ = keys.remove(slot);
            return Err(ArtError::Region { operation, source });
        }
        Ok(())
    }

    fn try_insert(
        &mut self,
        scope: &'region RegionScope,
        cx: &QueryCx,
        edge: u8,
        node: Node<'region, V>,
    ) -> Result<(), ArtError> {
        match self {
            Self::Node4 { keys, nodes } if keys.len() < Self::NODE4_CAPACITY => {
                return Self::try_insert_pair(
                    keys,
                    nodes,
                    cx,
                    Self::NODE4_CAPACITY,
                    edge,
                    node,
                    "growing Node4",
                );
            }
            Self::Node16 { keys, nodes } if keys.len() < Self::NODE16_CAPACITY => {
                return Self::try_insert_pair(
                    keys,
                    nodes,
                    cx,
                    Self::NODE16_CAPACITY,
                    edge,
                    node,
                    "growing Node16",
                );
            }
            Self::Node48 { index, nodes, len } if *len < Self::NODE48_CAPACITY => {
                let slot =
                    nodes
                        .iter()
                        .position(Option::is_none)
                        .ok_or(ArtError::InvariantViolation {
                            operation: "finding a vacant Node48 slot",
                        })?;
                *nodes.get_mut(slot).ok_or(ArtError::InvariantViolation {
                    operation: "writing a vacant Node48 slot",
                })? = Some(node);
                *index
                    .get_mut(usize::from(edge))
                    .ok_or(ArtError::InvariantViolation {
                        operation: "writing a Node48 edge index",
                    })? = encode_node48_slot(slot);
                *len += 1;
                return Ok(());
            }
            Self::Node256 { nodes, len } => {
                debug_assert_eq!(nodes.len(), Self::NODE256_CAPACITY);
                let slot =
                    nodes
                        .get_mut(usize::from(edge))
                        .ok_or(ArtError::InvariantViolation {
                            operation: "writing a Node256 edge",
                        })?;
                debug_assert!(slot.is_none());
                *slot = Some(node);
                *len += 1;
                return Ok(());
            }
            _ => {}
        }

        match self {
            Self::Node4 { .. } => self.try_grow_4_to_16(scope, cx, edge, node),
            Self::Node16 { .. } => self.try_grow_16_to_48(scope, cx, edge, node),
            Self::Node48 { .. } => self.try_grow_48_to_256(scope, cx, edge, node),
            Self::Node256 { .. } => {
                // All 256 byte edges are occupied, so callers cannot reach
                // this arm with a genuinely absent edge.
                Err(ArtError::InvariantViolation {
                    operation: "adding a 257th Node256 edge",
                })
            }
        }
    }

    fn try_grow_4_to_16(
        &mut self,
        scope: &'region RegionScope,
        cx: &QueryCx,
        edge: u8,
        node: Node<'region, V>,
    ) -> Result<(), ArtError> {
        let placeholder = Self::empty(scope)?;
        let Self::Node4 { keys, nodes } = self else {
            return Err(ArtError::InvariantViolation {
                operation: "promoting Node4 to Node16",
            });
        };
        Self::try_insert_pair(
            keys,
            nodes,
            cx,
            Self::NODE16_CAPACITY,
            edge,
            node,
            "promoting Node4 to Node16",
        )?;
        let old = core::mem::replace(self, placeholder);
        let (keys, nodes) = match old {
            Self::Node4 { keys, nodes } => (keys, nodes),
            other => {
                *self = other;
                return Err(ArtError::InvariantViolation {
                    operation: "promoting Node4 to Node16",
                });
            }
        };
        *self = Self::Node16 { keys, nodes };
        Ok(())
    }

    fn try_grow_16_to_48(
        &mut self,
        scope: &'region RegionScope,
        cx: &QueryCx,
        edge: u8,
        node: Node<'region, V>,
    ) -> Result<(), ArtError> {
        let mut new_index = try_zeroed_node48_index(scope, cx, "promoting Node16 to Node48")?;
        let mut new_nodes = try_empty_node_slots(
            scope,
            cx,
            Self::NODE48_CAPACITY,
            "promoting Node16 to Node48",
        )?;
        let placeholder = Self::empty(scope)?;
        let old = core::mem::replace(self, placeholder);
        let (keys, mut nodes) = match old {
            Self::Node16 { keys, nodes } => (keys, nodes),
            other => {
                *self = other;
                return Err(ArtError::InvariantViolation {
                    operation: "promoting Node16 to Node48",
                });
            }
        };
        let old_len = keys.len();
        for (slot, &old_edge) in keys.iter().enumerate() {
            *new_index
                .get_mut(usize::from(old_edge))
                .ok_or(ArtError::InvariantViolation {
                    operation: "materializing a Node48 edge index",
                })? = encode_node48_slot(slot);
            *new_nodes
                .get_mut(slot)
                .ok_or(ArtError::InvariantViolation {
                    operation: "materializing a Node48 child slot",
                })? = nodes.remove(0);
        }
        *new_nodes
            .get_mut(old_len)
            .ok_or(ArtError::InvariantViolation {
                operation: "adding the promoted Node48 edge",
            })? = Some(node);
        *new_index
            .get_mut(usize::from(edge))
            .ok_or(ArtError::InvariantViolation {
                operation: "indexing the promoted Node48 edge",
            })? = encode_node48_slot(old_len);
        *self = Self::Node48 {
            index: new_index,
            nodes: new_nodes,
            len: old_len + 1,
        };
        Ok(())
    }

    fn try_grow_48_to_256(
        &mut self,
        scope: &'region RegionScope,
        cx: &QueryCx,
        edge: u8,
        node: Node<'region, V>,
    ) -> Result<(), ArtError> {
        let mut new_nodes = try_empty_node_slots(
            scope,
            cx,
            Self::NODE256_CAPACITY,
            "promoting Node48 to Node256",
        )?;
        let placeholder = Self::empty(scope)?;
        let old = core::mem::replace(self, placeholder);
        let (index, mut nodes, old_len) = match old {
            Self::Node48 { index, nodes, len } => (index, nodes, len),
            other => {
                *self = other;
                return Err(ArtError::InvariantViolation {
                    operation: "promoting Node48 to Node256",
                });
            }
        };
        for old_edge in u8::MIN..=u8::MAX {
            let slot = *index
                .get(usize::from(old_edge))
                .ok_or(ArtError::InvariantViolation {
                    operation: "reading a Node48 edge during promotion",
                })?;
            if slot != 0 {
                let old_node = nodes
                    .get_mut(usize::from(slot - 1))
                    .and_then(Option::take)
                    .ok_or(ArtError::InvariantViolation {
                        operation: "moving a Node48 child during promotion",
                    })?;
                *new_nodes.get_mut(usize::from(old_edge)).ok_or(
                    ArtError::InvariantViolation {
                        operation: "writing a Node256 child during promotion",
                    },
                )? = Some(old_node);
            }
        }
        *new_nodes
            .get_mut(usize::from(edge))
            .ok_or(ArtError::InvariantViolation {
                operation: "adding the promoted Node256 edge",
            })? = Some(node);
        *self = Self::Node256 {
            nodes: new_nodes,
            len: old_len + 1,
        };
        Ok(())
    }

    fn remove(
        &mut self,
        scope: &'region RegionScope,
        cx: &QueryCx,
        edge: u8,
    ) -> Option<Node<'region, V>> {
        let removed = match self {
            Self::Node4 { keys, nodes } | Self::Node16 { keys, nodes } => {
                let slot = keys.as_slice().binary_search(&edge).ok()?;
                keys.remove(slot);
                nodes.remove(slot)
            }
            Self::Node48 { index, nodes, len } => {
                let encoded_slot = *index.get(usize::from(edge))?;
                if encoded_slot == 0 {
                    return None;
                }
                let slot = usize::from(encoded_slot - 1);
                *index.get_mut(usize::from(edge))? = 0;
                *len -= 1;
                nodes.get_mut(slot)?.take()
            }
            Self::Node256 { nodes, len } => {
                let removed = nodes.get_mut(usize::from(edge))?.take()?;
                *len -= 1;
                Some(removed)
            }
        };
        self.shrink_best_effort(scope, cx);
        removed
    }

    fn shrink_best_effort(&mut self, scope: &'region RegionScope, cx: &QueryCx) {
        if matches!(self, Self::Node16 { keys, .. } if keys.len() <= Self::NODE4_CAPACITY) {
            let Ok(placeholder) = Self::empty(scope) else {
                return;
            };
            let old = core::mem::replace(self, placeholder);
            if let Self::Node16 { keys, nodes } = old {
                *self = Self::Node4 { keys, nodes };
            }
            return;
        }

        if matches!(self, Self::Node48 { len, .. } if *len <= Self::NODE16_CAPACITY) {
            let Ok(mut new_keys) = RegionVec::new_in(scope) else {
                return;
            };
            if Self::try_reserve(
                &mut new_keys,
                cx,
                Self::NODE16_CAPACITY,
                "demoting Node48 to Node16",
            )
            .is_err()
            {
                return;
            }
            let Ok(mut new_nodes) = try_empty_node_slots(
                scope,
                cx,
                Self::NODE16_CAPACITY,
                "demoting Node48 to Node16",
            ) else {
                return;
            };
            let Self::Node48 { index, len, .. } = self else {
                return;
            };
            for edge in u8::MIN..=u8::MAX {
                if index.get(usize::from(edge)).is_some_and(|slot| *slot != 0)
                    && new_keys.try_push(cx, edge).is_err()
                {
                    return;
                }
            }
            debug_assert_eq!(new_keys.len(), *len);
            let Ok(placeholder) = Self::empty(scope) else {
                return;
            };
            let old = core::mem::replace(self, placeholder);
            if let Self::Node48 {
                index,
                mut nodes,
                len,
            } = old
            {
                for (ordinal, &edge) in new_keys.iter().enumerate() {
                    let slot = usize::from(index.as_slice()[usize::from(edge)]) - 1;
                    new_nodes.as_mut_slice()[ordinal] = nodes.get_mut(slot).and_then(Option::take);
                }
                new_nodes.truncate(len);
                *self = Self::Node16 {
                    keys: new_keys,
                    nodes: new_nodes,
                };
            }
            return;
        }

        if matches!(self, Self::Node256 { len, .. } if *len <= Self::NODE48_CAPACITY) {
            let Ok(mut compact_nodes) = try_empty_node_slots(
                scope,
                cx,
                Self::NODE48_CAPACITY,
                "demoting Node256 to Node48",
            ) else {
                return;
            };
            let Ok(mut index) = try_zeroed_node48_index(scope, cx, "demoting Node256 to Node48")
            else {
                return;
            };
            let Ok(placeholder) = Self::empty(scope) else {
                return;
            };
            let old = core::mem::replace(self, placeholder);
            if let Self::Node256 { mut nodes, len } = old {
                let mut compact_slot = 0usize;
                for edge in 0..Self::NODE256_CAPACITY {
                    if let Some(node) = nodes.get_mut(edge).and_then(Option::take) {
                        compact_nodes.as_mut_slice()[compact_slot] = Some(node);
                        index.as_mut_slice()[edge] = encode_node48_slot(compact_slot);
                        compact_slot += 1;
                    }
                }
                debug_assert_eq!(compact_slot, len);
                *self = Self::Node48 {
                    index,
                    nodes: compact_nodes,
                    len,
                };
                self.shrink_best_effort(scope, cx);
            }
        }
    }

    fn take_only_child(
        &mut self,
        scope: &'region RegionScope,
        cx: &QueryCx,
    ) -> Option<(u8, Node<'region, V>)> {
        let edge = self.only_child_ref()?.0;
        self.remove(scope, cx, edge).map(|node| (edge, node))
    }
}

impl<'region, V> Entry<'region, V> {
    fn try_new(
        scope: &'region RegionScope,
        cx: &QueryCx,
        key: &[u8],
        value: V,
    ) -> Result<Self, ArtError> {
        Ok(Self {
            key: try_copy_bytes(scope, cx, key, "copying an ART key")?,
            value,
        })
    }
}

impl<'region, V> Node<'region, V> {
    fn try_leaf(
        scope: &'region RegionScope,
        cx: &QueryCx,
        prefix: &[u8],
        full_key: &[u8],
        value: V,
    ) -> Result<Self, ArtError> {
        Ok(Self {
            prefix: try_copy_bytes(scope, cx, prefix, "copying an ART leaf prefix")?,
            entry: Some(Entry::try_new(scope, cx, full_key, value)?),
            children: Children::empty(scope)?,
        })
    }

    fn get(&self, remaining: &[u8]) -> Option<&V> {
        if !remaining.starts_with(self.prefix.as_slice()) {
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
        if !remaining.starts_with(self.prefix.as_slice()) {
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
        let common = common_prefix_len(self.prefix.as_slice(), remaining);
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
        scope: &'region RegionScope,
        cx: &QueryCx,
        remaining: &[u8],
        full_key: &[u8],
        value: V,
    ) -> Result<Option<V>, ArtError> {
        let common = common_prefix_len(self.prefix.as_slice(), remaining);
        if common != self.prefix.len() {
            return self.try_split_and_insert(scope, cx, common, remaining, full_key, value);
        }

        let remaining = &remaining[common..];
        if remaining.is_empty() {
            if let Some(entry) = &mut self.entry {
                return Ok(Some(core::mem::replace(&mut entry.value, value)));
            }
            self.entry = Some(Entry::try_new(scope, cx, full_key, value)?);
            return Ok(None);
        }

        let edge = remaining[0];
        if let Some(child) = self.children.get_mut(edge) {
            return child.try_insert(scope, cx, &remaining[1..], full_key, value);
        }

        let leaf = Self::try_leaf(scope, cx, &remaining[1..], full_key, value)?;
        self.children.try_insert(scope, cx, edge, leaf)?;
        Ok(None)
    }

    fn try_split_and_insert(
        &mut self,
        scope: &'region RegionScope,
        cx: &QueryCx,
        common: usize,
        remaining: &[u8],
        full_key: &[u8],
        value: V,
    ) -> Result<Option<V>, ArtError> {
        debug_assert!(common < self.prefix.len());
        let old_edge = self.prefix.as_slice()[common];
        let parent_prefix = try_copy_bytes(
            scope,
            cx,
            &self.prefix.as_slice()[..common],
            "splitting an ART prefix",
        )?;
        let old_suffix = try_copy_bytes(
            scope,
            cx,
            &self.prefix.as_slice()[common + 1..],
            "copying a split ART suffix",
        )?;

        let (parent_entry, new_child) = if common == remaining.len() {
            (Some(Entry::try_new(scope, cx, full_key, value)?), None)
        } else {
            let new_edge = remaining[common];
            let leaf = Self::try_leaf(scope, cx, &remaining[common + 1..], full_key, value)?;
            (None, Some((new_edge, leaf)))
        };

        // Reserve all storage before moving the existing node. After this
        // point the split cannot fail and insertion remains logically atomic.
        let child_count = 1 + usize::from(new_child.is_some());
        let mut keys = RegionVec::new_in(scope).map_err(|source| ArtError::Region {
            operation: "allocating split edges",
            source,
        })?;
        Children::<V>::try_reserve(&mut keys, cx, child_count, "allocating split edges")?;
        let mut nodes = try_empty_node_slots(scope, cx, child_count, "allocating split nodes")?;

        let old_slot;
        if let Some((new_edge, leaf)) = new_child {
            if new_edge < old_edge {
                keys.try_push(cx, new_edge)
                    .map_err(|source| ArtError::Region {
                        operation: "materializing split edges",
                        source,
                    })?;
                keys.try_push(cx, old_edge)
                    .map_err(|source| ArtError::Region {
                        operation: "materializing split edges",
                        source,
                    })?;
                nodes.as_mut_slice()[0] = Some(leaf);
                old_slot = 1;
            } else {
                keys.try_push(cx, old_edge)
                    .map_err(|source| ArtError::Region {
                        operation: "materializing split edges",
                        source,
                    })?;
                keys.try_push(cx, new_edge)
                    .map_err(|source| ArtError::Region {
                        operation: "materializing split edges",
                        source,
                    })?;
                nodes.as_mut_slice()[1] = Some(leaf);
                old_slot = 0;
            }
        } else {
            keys.try_push(cx, old_edge)
                .map_err(|source| ArtError::Region {
                    operation: "materializing split edges",
                    source,
                })?;
            old_slot = 0;
        }

        let parent = Self {
            prefix: parent_prefix,
            entry: parent_entry,
            children: Children::Node4 { keys, nodes },
        };
        let mut old_node = core::mem::replace(self, parent);
        if !matches!(self.children, Children::Node4 { .. }) {
            *self = old_node;
            return Err(ArtError::InvariantViolation {
                operation: "materializing a split Node4 parent",
            });
        }
        old_node.prefix = old_suffix;
        let Children::Node4 { nodes, .. } = &mut self.children else {
            unreachable!("the split parent variant was checked immediately above")
        };
        nodes.as_mut_slice()[old_slot] = Some(old_node);
        Ok(None)
    }

    fn remove(&mut self, scope: &'region RegionScope, cx: &QueryCx, remaining: &[u8]) -> Option<V> {
        if !remaining.starts_with(self.prefix.as_slice()) {
            return None;
        }
        let remaining = &remaining[self.prefix.len()..];
        let removed = if remaining.is_empty() {
            self.entry.take().map(|entry| entry.value)
        } else {
            let edge = remaining[0];
            let (removed, child_became_empty) = {
                let child = self.children.get_mut(edge)?;
                let removed = child.remove(scope, cx, &remaining[1..]);
                let empty = child.entry.is_none() && child.children.len() == 0;
                (removed, empty)
            };
            if child_became_empty {
                let _ = self.children.remove(scope, cx, edge);
            }
            removed
        };

        if removed.is_some() {
            self.recompress_unary_best_effort(scope, cx);
        }
        removed
    }

    fn recompress_unary_best_effort(&mut self, scope: &'region RegionScope, cx: &QueryCx) {
        let Some((_, child)) = self.children.only_child_ref() else {
            return;
        };
        if self.entry.is_some() {
            return;
        }
        let prefix_len = self.prefix.len();
        let child_prefix_len = child.prefix.len();
        let Some(combined_len) = prefix_len
            .checked_add(1)
            .and_then(|len| len.checked_add(child_prefix_len))
        else {
            return;
        };
        if self.prefix.try_resize(cx, combined_len, 0).is_err() {
            // Compression is a representation optimization. Keeping a valid
            // unary node is preferable to making successful deletion fallible
            // after the logical value has already been removed.
            return;
        }
        let Some((edge, child_ref)) = self.children.only_child_ref() else {
            return;
        };
        let prefix = self.prefix.as_mut_slice();
        prefix[prefix_len] = edge;
        prefix[prefix_len + 1..combined_len].copy_from_slice(child_ref.prefix.as_slice());
        let Some((_, child)) = self.children.take_only_child(scope, cx) else {
            return;
        };
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

fn try_zeroed_node48_index<'region>(
    scope: &'region RegionScope,
    cx: &QueryCx,
    operation: &'static str,
) -> Result<RegionVec<'region, u8>, ArtError> {
    let mut index = RegionVec::with_capacity_in(scope, cx, 256)
        .map_err(|source| ArtError::Region { operation, source })?;
    index
        .try_resize(cx, 256, 0)
        .map_err(|source| ArtError::Region { operation, source })?;
    Ok(index)
}

fn try_empty_node_slots<'region, V>(
    scope: &'region RegionScope,
    cx: &QueryCx,
    capacity: usize,
    operation: &'static str,
) -> Result<RegionVec<'region, Option<Node<'region, V>>>, ArtError> {
    let mut nodes = RegionVec::with_capacity_in(scope, cx, capacity)
        .map_err(|source| ArtError::Region { operation, source })?;
    nodes
        .try_resize_with(cx, capacity, || None)
        .map_err(|source| ArtError::Region { operation, source })?;
    Ok(nodes)
}

fn try_copy_bytes<'region>(
    scope: &'region RegionScope,
    cx: &QueryCx,
    bytes: &[u8],
    operation: &'static str,
) -> Result<RegionVec<'region, u8>, ArtError> {
    let mut copy = RegionVec::with_capacity_in(scope, cx, bytes.len())
        .map_err(|source| ArtError::Region { operation, source })?;
    for &byte in bytes {
        copy.try_push(cx, byte)
            .map_err(|source| ArtError::Region { operation, source })?;
    }
    Ok(copy)
}

/// A safe, generic adaptive radix map keyed by arbitrary bytes.
pub struct AdaptiveRadixTree<'region, V> {
    scope: &'region RegionScope,
    root: Option<Node<'region, V>>,
    len: usize,
    limits: ArtLimits,
}

/// Concise alias for callers that prefer a map-shaped name.
pub type ArtMap<'region, V> = AdaptiveRadixTree<'region, V>;

impl<'region, V> AdaptiveRadixTree<'region, V> {
    /// Creates an empty tree with conservative default resource limits.
    #[must_use]
    pub const fn new_in(scope: &'region RegionScope) -> Self {
        Self {
            scope,
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
    pub const fn with_limits_in(scope: &'region RegionScope, limits: ArtLimits) -> Self {
        Self {
            scope,
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
    pub fn insert<K>(&mut self, cx: &QueryCx, key: K, value: V) -> Result<Option<V>, ArtError>
    where
        K: AsRef<[u8]>,
    {
        self.try_insert(cx, key, value)
    }

    /// Explicitly named alias for [`Self::insert`].
    pub fn try_insert<K>(&mut self, cx: &QueryCx, key: K, value: V) -> Result<Option<V>, ArtError>
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
        cx.checkpoint().map_err(|_| ArtError::Region {
            operation: "admitting an ART insertion",
            source: RegionVecError::CheckpointRefused,
        })?;

        let replaced = match &mut self.root {
            Some(root) => root.try_insert(self.scope, cx, key, key, value)?,
            None => {
                self.root = Some(Node::try_leaf(self.scope, cx, key, key, value)?);
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
    pub fn remove<K>(&mut self, cx: &QueryCx, key: K) -> Result<Option<V>, ArtError>
    where
        K: AsRef<[u8]>,
    {
        cx.checkpoint().map_err(|_| ArtError::Region {
            operation: "admitting an ART removal",
            source: RegionVecError::CheckpointRefused,
        })?;
        let Some(root) = self.root.as_mut() else {
            return Ok(None);
        };
        let removed = root.remove(self.scope, cx, key.as_ref());
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
        Ok(removed)
    }

    /// Iterates over every mapping in lexicographic byte-key order.
    #[must_use]
    pub fn iter(&self) -> ArtIter<'_, 'region, V> {
        ArtIter::new(self.root.as_ref())
    }

    /// Iterates over mappings whose keys start with `prefix`.
    #[must_use]
    pub fn prefix<K>(&self, prefix: K) -> ArtIter<'_, 'region, V>
    where
        K: AsRef<[u8]>,
    {
        self.prefix_iter(prefix)
    }

    /// Explicitly named alias for [`Self::prefix`].
    #[must_use]
    pub fn prefix_iter<K>(&self, prefix: K) -> ArtIter<'_, 'region, V>
    where
        K: AsRef<[u8]>,
    {
        let node = self
            .root
            .as_ref()
            .and_then(|root| root.find_prefix_node(prefix.as_ref()));
        ArtIter::new(node)
    }

    /// Starts a bounded, lexicographically ordered ART/Levenshtein product
    /// traversal.
    ///
    /// Automaton state advances through the root prefix and then each
    /// `(edge, compressed_prefix)` pair directly. Candidate strings are never
    /// assembled as traversal scratch, and a dead automaton state prunes its
    /// entire ART subtree.
    pub fn try_levenshtein_iter<'tree, 'automaton>(
        &'tree self,
        automaton: &'automaton LevenshteinAutomaton,
        limits: ArtLevenshteinLimits,
    ) -> Result<ArtLevenshteinIter<'tree, 'automaton, 'region, V>, ArtLevenshteinError> {
        ArtLevenshteinIter::try_new(self.root.as_ref(), automaton, limits)
    }

    /// Iterates over mappings in a standard lexicographic byte range.
    ///
    /// Byte-slice bounds can be supplied without allocating:
    ///
    /// ```
    /// use core::ops::Bound;
    /// use fgdb_collections::art::AdaptiveRadixTree;
    /// use fgdb_types::QueryCx;
    /// use fgdb_unsafe_arena::RegionScope;
    ///
    /// # fn example(scope: &RegionScope, cx: &QueryCx) {
    /// let mut tree = AdaptiveRadixTree::new_in(scope);
    /// assert_eq!(tree.insert(cx, b"ant", 1), Ok(None));
    /// assert_eq!(tree.insert(cx, b"bee", 2), Ok(None));
    /// let entries: Vec<_> = tree
    ///     .range((
    ///         Bound::Included(&b"ant"[..]),
    ///         Bound::Excluded(&b"cat"[..]),
    ///     ))
    ///     .collect();
    /// assert_eq!(entries.len(), 2);
    /// # }
    /// ```
    pub fn range<R>(&self, bounds: R) -> ArtRange<'_, 'region, V, R>
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

impl<V: fmt::Debug> fmt::Debug for AdaptiveRadixTree<'_, V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map().entries(self.iter()).finish()
    }
}

impl<'tree, 'region, V> IntoIterator for &'tree AdaptiveRadixTree<'region, V> {
    type Item = (&'tree [u8], &'tree V);
    type IntoIter = ArtIter<'tree, 'region, V>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

struct IterFrame<'tree, 'region, V> {
    node: &'tree Node<'region, V>,
    yielded_entry: bool,
    next_child: usize,
}

/// Lexicographically ordered iterator over ART mappings.
pub struct ArtIter<'tree, 'region, V> {
    stack: Vec<IterFrame<'tree, 'region, V>>,
}

impl<'tree, 'region, V> ArtIter<'tree, 'region, V> {
    fn new(root: Option<&'tree Node<'region, V>>) -> Self {
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

impl<'tree, V> Iterator for ArtIter<'tree, '_, V> {
    type Item = (&'tree [u8], &'tree V);

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let frame = self.stack.last_mut()?;
            if !frame.yielded_entry {
                frame.yielded_entry = true;
                if let Some(entry) = &frame.node.entry {
                    return Some((entry.key.as_slice(), &entry.value));
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
pub struct ArtRange<'tree, 'region, V, R> {
    inner: ArtIter<'tree, 'region, V>,
    bounds: R,
    exhausted: bool,
}

impl<'tree, V, R> Iterator for ArtRange<'tree, '_, V, R>
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

/// One accepted key/value pair from an ART/Levenshtein product walk.
#[derive(Debug)]
pub struct ArtLevenshteinMatch<'tree, V> {
    /// Existing canonical key storage owned by the tree.
    pub key: &'tree [u8],
    /// Value associated with `key`.
    pub value: &'tree V,
    /// Exact byte-level edit distance, bounded by the automaton's ceiling.
    pub distance: u16,
}

struct ArtLevenshteinFrame<'tree, 'automaton, 'region, V> {
    node: &'tree Node<'region, V>,
    state: LevenshteinState<'automaton>,
    yielded_entry: bool,
    next_child: usize,
}

/// Fallible, ordered iterator over an ART/Levenshtein product.
///
/// A traversal error is yielded once and then the iterator is fused.
pub struct ArtLevenshteinIter<'tree, 'automaton, 'region, V> {
    automaton: &'automaton LevenshteinAutomaton,
    limits: ArtLevenshteinLimits,
    stack: Vec<ArtLevenshteinFrame<'tree, 'automaton, 'region, V>>,
    visited_nodes: usize,
    pruned_subtrees: usize,
    exhausted: bool,
}

impl<'tree, 'automaton, 'region, V> ArtLevenshteinIter<'tree, 'automaton, 'region, V> {
    fn try_new(
        root: Option<&'tree Node<'region, V>>,
        automaton: &'automaton LevenshteinAutomaton,
        limits: ArtLevenshteinLimits,
    ) -> Result<Self, ArtLevenshteinError> {
        let mut stack = Vec::new();
        stack
            .try_reserve_exact(limits.max_stack_frames)
            .map_err(
                |_: TryReserveError| ArtLevenshteinError::TraversalAllocationFailed {
                    requested_frames: limits.max_stack_frames,
                },
            )?;
        let mut traversal = Self {
            automaton,
            limits,
            stack,
            visited_nodes: 0,
            pruned_subtrees: 0,
            exhausted: false,
        };

        let Some(root) = root else {
            return Ok(traversal);
        };
        if limits.max_stack_frames == 0 {
            return Err(ArtLevenshteinError::TraversalDepthLimitExceeded {
                max_stack_frames: 0,
            });
        }

        let initial = automaton.initial_state()?;
        let root_state = if root.prefix.is_empty() {
            initial
        } else {
            automaton.try_advance_bytes(&initial, root.prefix.as_slice())?
        };
        traversal.visited_nodes = 1;
        if root_state.can_match_descendant() {
            traversal.stack.push(ArtLevenshteinFrame {
                node: root,
                state: root_state,
                yielded_entry: false,
                next_child: 0,
            });
        } else {
            traversal.pruned_subtrees = 1;
        }
        Ok(traversal)
    }

    /// Number of ART nodes whose compressed path was evaluated so far.
    #[must_use]
    pub const fn visited_nodes(&self) -> usize {
        self.visited_nodes
    }

    /// Number of whole subtrees rejected by a dead automaton state so far.
    #[must_use]
    pub const fn pruned_subtrees(&self) -> usize {
        self.pruned_subtrees
    }

    fn fail(
        &mut self,
        error: ArtLevenshteinError,
    ) -> Option<Result<ArtLevenshteinMatch<'tree, V>, ArtLevenshteinError>> {
        self.exhausted = true;
        self.stack.clear();
        Some(Err(error))
    }
}

impl<'tree, V> Iterator for ArtLevenshteinIter<'tree, '_, '_, V> {
    type Item = Result<ArtLevenshteinMatch<'tree, V>, ArtLevenshteinError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.exhausted {
            return None;
        }

        loop {
            let Some(frame) = self.stack.last_mut() else {
                self.exhausted = true;
                return None;
            };

            if !frame.yielded_entry {
                frame.yielded_entry = true;
                if let Some(entry) = &frame.node.entry {
                    let is_match = match self.automaton.is_match(&frame.state) {
                        Ok(is_match) => is_match,
                        Err(error) => return self.fail(error.into()),
                    };
                    if is_match {
                        let Some(distance) = frame.state.terminal_distance() else {
                            return self.fail(ArtLevenshteinError::RepresentationInvariant {
                                operation: "reading a terminal automaton distance",
                            });
                        };
                        return Some(Ok(ArtLevenshteinMatch {
                            key: entry.key.as_slice(),
                            value: &entry.value,
                            distance,
                        }));
                    }
                }
            }

            if frame.next_child >= frame.node.children.len() {
                self.stack.pop();
                continue;
            }

            let ordinal = frame.next_child;
            frame.next_child += 1;
            let Some((edge, child)) = frame.node.children.edge_at(ordinal) else {
                return self.fail(ArtLevenshteinError::RepresentationInvariant {
                    operation: "reading an ordered child",
                });
            };
            let child_state = match self.automaton.try_advance_edge_and_prefix(
                &frame.state,
                edge,
                child.prefix.as_slice(),
            ) {
                Ok(state) => state,
                Err(error) => return self.fail(error.into()),
            };
            self.visited_nodes = self.visited_nodes.saturating_add(1);
            if !child_state.can_match_descendant() {
                self.pruned_subtrees = self.pruned_subtrees.saturating_add(1);
                continue;
            }
            if self.stack.len() >= self.limits.max_stack_frames {
                return self.fail(ArtLevenshteinError::TraversalDepthLimitExceeded {
                    max_stack_frames: self.limits.max_stack_frames,
                });
            }
            self.stack.push(ArtLevenshteinFrame {
                node: child,
                state: child_state,
                yielded_entry: false,
                next_child: 0,
            });
        }
    }
}

impl<V> core::iter::FusedIterator for ArtLevenshteinIter<'_, '_, '_, V> {}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(miri)]
    use asupersync::Cx;
    #[cfg(miri)]
    use asupersync::cx::cap;
    #[cfg(not(miri))]
    use asupersync::lab::run_async_under_lab;
    use fgdb_types::PurposeContexts;
    use std::collections::BTreeMap;

    #[cfg(not(miri))]
    fn query_cx() -> QueryCx {
        let (query, report) = run_async_under_lab(0xa47a_47a1, |root| async move {
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
        RegionScope::with_capacity(1 << 20, 1 << 29)
    }

    fn owned_entries(tree: &AdaptiveRadixTree<'_, u64>) -> Vec<(Vec<u8>, u64)> {
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

    fn assert_single_level_fanout_matches(
        tree: &AdaptiveRadixTree<'_, u64>,
        reference: &BTreeMap<Vec<u8>, u64>,
        child_count: usize,
    ) {
        assert_eq!(tree.len(), child_count);
        assert_eq!(owned_entries(tree), reference_entries(reference));
        for edge in u8::MIN..=u8::MAX {
            assert_eq!(
                tree.get([edge]),
                reference.get([edge].as_slice()),
                "edge={edge} child_count={child_count}"
            );
        }

        let expected_kind = match child_count {
            0 => None,
            1..=4 => Some(NodeKind::Node4),
            5..=16 => Some(NodeKind::Node16),
            17..=48 => Some(NodeKind::Node48),
            49..=256 => Some(NodeKind::Node256),
            _ => unreachable!("one-byte keys cannot exceed 256 children"),
        };
        assert_eq!(
            tree.root.as_ref().map(|root| root.children.kind()),
            expected_kind
        );
        assert_eq!(
            tree.node_kind_histogram().total(),
            usize::from(child_count >= 2)
        );
        if child_count >= 2 {
            let kind = expected_kind.expect("multiple keys require a fanout node");
            assert_eq!(tree.node_kind_histogram().count(kind), 1);
        }
    }

    #[test]
    fn empty_key_replacement_and_mutation_are_map_compatible() {
        let scope = test_scope();
        let cx = query_cx();
        let mut tree = AdaptiveRadixTree::new_in(&scope);
        assert!(tree.is_empty());
        assert_eq!(tree.insert(&cx, [], 7), Ok(None));
        assert_eq!(tree.insert(&cx, b"a", 11), Ok(None));
        assert_eq!(tree.insert(&cx, b"ab", 13), Ok(None));
        assert_eq!(tree.insert(&cx, [], 17), Ok(Some(7)));
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
        let scope = test_scope();
        let cx = query_cx();
        let mut tree = AdaptiveRadixTree::new_in(&scope);
        let mut key = b"stable-key".to_vec();
        assert_eq!(tree.insert(&cx, &key, 1), Ok(None));
        key.fill(b'x');
        assert_eq!(tree.get(b"stable-key"), Some(&1));
        assert_eq!(tree.get(&key), None);
        assert_eq!(tree.node_kind_histogram(), NodeKindHistogram::default());
    }

    #[test]
    fn fanout_grows_and_shrinks_through_every_art_representation() {
        let scope = test_scope();
        let cx = query_cx();
        let mut tree = AdaptiveRadixTree::new_in(&scope);
        for edge in u8::MIN..=u8::MAX {
            assert_eq!(tree.insert(&cx, [edge], u64::from(edge)), Ok(None));
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
            assert_eq!(tree.remove(&cx, [edge]), Ok(Some(u64::from(edge))));
        }
        assert_eq!(tree.len(), 48);
        assert_eq!(
            tree.root.as_ref().map(|root| root.children.kind()),
            Some(NodeKind::Node48)
        );

        for edge in (16u8..48).rev() {
            assert_eq!(tree.remove(&cx, [edge]), Ok(Some(u64::from(edge))));
        }
        assert_eq!(
            tree.root.as_ref().map(|root| root.children.kind()),
            Some(NodeKind::Node16)
        );

        for edge in (4u8..16).rev() {
            assert_eq!(tree.remove(&cx, [edge]), Ok(Some(u64::from(edge))));
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
    fn adversarial_edge_order_preserves_every_fanout_boundary() {
        let scope = test_scope();
        let cx = query_cx();
        let insertion_order = (0_u16..=255)
            .map(|ordinal| ((ordinal * 197 + 101) & 0xff) as u8)
            .collect::<Vec<_>>();
        let checkpoints = [1_usize, 4, 5, 16, 17, 48, 49, 255, 256];
        let mut tree = AdaptiveRadixTree::new_in(&scope);
        let mut reference = BTreeMap::new();

        for (ordinal, &edge) in insertion_order.iter().enumerate() {
            let value = u64::from(edge) * 17 + 5;
            assert_eq!(tree.insert(&cx, [edge], value), Ok(None));
            reference.insert(vec![edge], value);
            let child_count = ordinal + 1;
            if checkpoints.contains(&child_count) {
                assert_single_level_fanout_matches(&tree, &reference, child_count);
            }
        }

        for removed in 0..insertion_order.len() {
            let edge = insertion_order[(removed * 149 + 73) & 0xff];
            assert_eq!(
                tree.remove(&cx, [edge]),
                Ok(reference.remove([edge].as_slice()))
            );
            let child_count = 255 - removed;
            if checkpoints.contains(&child_count) || child_count == 0 {
                assert_single_level_fanout_matches(&tree, &reference, child_count);
            }
        }
        assert!(tree.is_empty());
    }

    #[test]
    fn node48_swap_removal_preserves_every_other_edge_mapping() {
        let scope = test_scope();
        let cx = query_cx();
        let mut tree = AdaptiveRadixTree::new_in(&scope);
        for edge in 0u8..40 {
            assert_eq!(tree.insert(&cx, [edge], u64::from(edge) * 3), Ok(None));
        }
        for edge in [7u8, 0, 31, 18, 39, 5, 22] {
            assert_eq!(tree.remove(&cx, [edge]), Ok(Some(u64::from(edge) * 3)));
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
        let scope = test_scope();
        let cx = query_cx();
        let mut tree = AdaptiveRadixTree::new_in(&scope);
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
            assert_eq!(tree.insert(&cx, key, value), Ok(None));
        }
        for (key, value) in fixtures {
            assert_eq!(tree.get(key), Some(&value));
        }

        assert_eq!(tree.remove(&cx, b"prefix"), Ok(Some(3)));
        assert_eq!(tree.remove(&cx, b"prefix-alpha"), Ok(Some(1)));
        assert_eq!(tree.remove(&cx, b"prefix-alpine"), Ok(Some(2)));
        assert_eq!(tree.remove(&cx, b"prefix-beta"), Ok(Some(4)));
        assert_eq!(tree.remove(&cx, b"prefix-betamax"), Ok(Some(5)));
        assert_eq!(tree.remove(&cx, b"prefix-\0"), Ok(Some(6)));
        assert_eq!(tree.len(), 1);
        assert_eq!(tree.get(b"prefix-\xff"), Some(&7));
        assert_eq!(
            tree.root.as_ref().map(|root| root.prefix.as_slice()),
            Some(&b"prefix-\xff"[..])
        );
        assert_eq!(tree.remove(&cx, b"prefix-\xff"), Ok(Some(7)));
        assert!(tree.is_empty());
        assert_eq!(tree.node_kind_histogram(), NodeKindHistogram::default());
    }

    #[test]
    fn prefix_iteration_handles_prefixes_ending_inside_compressed_paths() {
        let scope = test_scope();
        let cx = query_cx();
        let mut tree = AdaptiveRadixTree::new_in(&scope);
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
            assert_eq!(tree.insert(&cx, key, index as u64), Ok(None));
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
        let scope = test_scope();
        let cx = query_cx();
        let mut tree = AdaptiveRadixTree::new_in(&scope);
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
            assert_eq!(tree.insert(&cx, key, index as u64), Ok(None));
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
        let scope = test_scope();
        let cx = query_cx();
        let mut tree = AdaptiveRadixTree::with_limits_in(
            &scope,
            ArtLimits {
                max_key_bytes: 3,
                max_entries: 2,
            },
        );
        assert_eq!(tree.insert(&cx, b"a", 1), Ok(None));
        assert_eq!(tree.insert(&cx, b"bbb", 2), Ok(None));
        assert_eq!(
            tree.insert(&cx, b"long", 3),
            Err(ArtError::KeyTooLong { len: 4, max: 3 })
        );
        assert_eq!(
            tree.insert(&cx, b"cc", 3),
            Err(ArtError::EntryLimitReached { max: 2 })
        );
        assert_eq!(tree.insert(&cx, b"a", 4), Ok(Some(1)));
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

    fn simple_edit_distance(left: &[u8], right: &[u8]) -> usize {
        let mut previous: Vec<usize> = (0..=right.len()).collect();
        let mut current = vec![0; right.len() + 1];
        for (left_index, &left_byte) in left.iter().enumerate() {
            current[0] = left_index + 1;
            for (right_index, &right_byte) in right.iter().enumerate() {
                current[right_index + 1] = (current[right_index] + 1)
                    .min(previous[right_index + 1] + 1)
                    .min(previous[right_index] + usize::from(left_byte != right_byte));
            }
            core::mem::swap(&mut previous, &mut current);
        }
        previous[right.len()]
    }

    #[test]
    fn generated_token_product_walk_matches_simple_distance_oracle() {
        const SEED: u64 = 0x4f3c_2a19_d781_b605;
        let scope = test_scope();
        let cx = query_cx();
        let mut rng = DeterministicRng(SEED);
        let mut reference = BTreeMap::<Vec<u8>, u64>::new();
        let stems: [&[u8]; 8] = [
            b"branch/",
            b"chronicle/",
            b"commit/",
            b"graph/",
            b"grant/",
            b"strata/",
            b"subscription/",
            b"warden/",
        ];
        for ordinal in 0u64..768 {
            let stem = stems[usize::from(rng.next().to_le_bytes()[0]) % stems.len()];
            let suffix_len = 2 + usize::from(rng.next().to_le_bytes()[0] % 12);
            let mut token = Vec::with_capacity(stem.len() + suffix_len);
            token.extend_from_slice(stem);
            for _ in 0..suffix_len {
                token.push(b'a' + (rng.next().to_le_bytes()[0] % 12));
            }
            reference.insert(token, ordinal);
        }

        let mut tree = AdaptiveRadixTree::new_in(&scope);
        for (key, value) in &reference {
            assert_eq!(tree.insert(&cx, key, *value), Ok(None));
        }

        let patterns: [&[u8]; 7] = [
            b"branch/agent",
            b"chronicle/marker",
            b"graph/memory",
            b"grant/access",
            b"strata/run",
            b"warden/policy",
            b"zzzzzzzzzz",
        ];
        let mut total_pruned = 0usize;
        for pattern in patterns {
            for maximum in 0..=2 {
                let automaton = LevenshteinAutomaton::try_new(pattern, maximum, 64);
                assert!(
                    automaton.is_ok(),
                    "bounded generated pattern must construct"
                );
                let Ok(automaton) = automaton else {
                    return;
                };
                let traversal = tree.try_levenshtein_iter(
                    &automaton,
                    ArtLevenshteinLimits {
                        max_stack_frames: 128,
                    },
                );
                assert!(
                    traversal.is_ok(),
                    "bounded generated traversal must construct"
                );
                let Ok(mut traversal) = traversal else {
                    return;
                };
                let actual: Result<Vec<_>, _> = traversal
                    .by_ref()
                    .map(|item| {
                        item.map(|matched| {
                            (
                                matched.key.to_vec(),
                                *matched.value,
                                usize::from(matched.distance),
                            )
                        })
                    })
                    .collect();
                let expected: Vec<_> = reference
                    .iter()
                    .filter_map(|(key, value)| {
                        let distance = simple_edit_distance(pattern, key);
                        (distance <= usize::from(maximum)).then(|| (key.clone(), *value, distance))
                    })
                    .collect();
                assert_eq!(
                    actual,
                    Ok(expected),
                    "seed={SEED} pattern={pattern:?} maximum={maximum}"
                );
                total_pruned = total_pruned.saturating_add(traversal.pruned_subtrees());
                assert!(
                    traversal.visited_nodes() <= tree.node_kind_histogram().total() + tree.len()
                );
            }
        }
        assert!(total_pruned > 0);
    }

    #[test]
    fn product_walk_enforces_typed_frame_and_allocation_limits() {
        let scope = test_scope();
        let cx = query_cx();
        let automaton_result = LevenshteinAutomaton::try_new(b"aaaaaaaaaa", 2, 16);
        assert!(automaton_result.is_ok(), "bounded pattern must construct");
        let Ok(automaton) = automaton_result else {
            return;
        };
        let mut tree = AdaptiveRadixTree::new_in(&scope);
        for length in 1..=10 {
            assert_eq!(tree.insert(&cx, vec![b'a'; length], length), Ok(None));
        }

        let traversal = tree.try_levenshtein_iter(
            &automaton,
            ArtLevenshteinLimits {
                max_stack_frames: 2,
            },
        );
        assert!(traversal.is_ok(), "root fits the frame limit");
        let Ok(traversal) = traversal else {
            return;
        };
        let result: Result<Vec<_>, _> = traversal.collect();
        assert!(matches!(
            result,
            Err(ArtLevenshteinError::TraversalDepthLimitExceeded {
                max_stack_frames: 2
            })
        ));

        let zero_limit = tree.try_levenshtein_iter(
            &automaton,
            ArtLevenshteinLimits {
                max_stack_frames: 0,
            },
        );
        assert!(matches!(
            zero_limit,
            Err(ArtLevenshteinError::TraversalDepthLimitExceeded {
                max_stack_frames: 0
            })
        ));

        let empty = AdaptiveRadixTree::<u8>::new_in(&scope);
        let impossible_reservation = empty.try_levenshtein_iter(
            &automaton,
            ArtLevenshteinLimits {
                max_stack_frames: usize::MAX,
            },
        );
        assert!(matches!(
            impossible_reservation,
            Err(ArtLevenshteinError::TraversalAllocationFailed {
                requested_frames: usize::MAX
            })
        ));
    }

    #[test]
    fn deterministic_operation_sequences_match_btree_map() {
        const SEED: u64 = 0xd1b5_4a32_d192_ed03;
        let scope = test_scope();
        let cx = query_cx();
        let mut rng = DeterministicRng(SEED);
        let mut tree = AdaptiveRadixTree::new_in(&scope);
        let mut reference = BTreeMap::<Vec<u8>, u64>::new();

        for step in 0u64..10_000 {
            let key = rng.key();
            match rng.next() % 5 {
                0 | 1 => {
                    let value = rng.next() ^ step;
                    let expected = reference.insert(key.clone(), value);
                    assert_eq!(
                        tree.insert(&cx, &key, value),
                        Ok(expected),
                        "seed={SEED} step={step}"
                    );
                }
                2 => {
                    assert_eq!(
                        tree.remove(&cx, &key),
                        Ok(reference.remove(&key)),
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
