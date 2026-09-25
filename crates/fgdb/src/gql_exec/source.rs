//! Borrowed scans of an already admitted immutable Snapshot generation.
//! These helpers do not validate raw blocks or bypass snapshot admission.

mod aggregation;

use crate::Snapshot;
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_strata::AdjacencyEntry;
use fgdb_strata::vertex::{VertexPatchRows, VertexRow};
use fgdb_types::{CanonicalScalar, CommitSeq, EId, VId};
use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SourceEvent {
    Work,
    ScratchEntry,
    SnapshotRecord,
}
/// Admitted topology keeps edge identity so captured paths name real edges.
type IdentifiedEdge = (EId, VId, RelationId, VId);
type VertexCursor = Reverse<(VId, CommitSeq, usize, usize)>;

/// Immutable AVL nodes: updating a generation copies only the search path.
/// Values stored here must themselves have cheap clones (coordinates or roots).
#[derive(Clone, Debug)]
struct IndexMap<K, V>(Option<std::sync::Arc<IndexNode<K, V>>>);

#[derive(Clone, Debug)]
struct IndexNode<K, V> {
    key: K,
    value: V,
    left: IndexMap<K, V>,
    right: IndexMap<K, V>,
    height: u16,
    len: usize,
}

impl<K: Ord + Clone, V: Clone> IndexMap<K, V> {
    fn new() -> Self {
        Self(None)
    }
    fn height(&self) -> u16 {
        self.0.as_ref().map_or(0, |n| n.height)
    }
    fn len(&self) -> usize {
        self.0.as_ref().map_or(0, |n| n.len)
    }
    fn node(key: K, value: V, left: Self, right: Self, work: &mut u64) -> Self {
        *work += 1;
        let height = 1 + left.height().max(right.height());
        let len = 1 + left.len() + right.len();
        Self(Some(std::sync::Arc::new(IndexNode {
            key,
            value,
            left,
            right,
            height,
            len,
        })))
    }
    fn balanced(key: K, value: V, mut left: Self, mut right: Self, work: &mut u64) -> Self {
        if left.height() > right.height() + 1 {
            let child = left.0.as_ref().expect("left-heavy tree");
            if child.right.height() > child.left.height() {
                let pivot = child.right.0.as_ref().expect("inner-heavy child");
                let lower = Self::node(
                    child.key.clone(),
                    child.value.clone(),
                    child.left.clone(),
                    pivot.left.clone(),
                    work,
                );
                left = Self::node(
                    pivot.key.clone(),
                    pivot.value.clone(),
                    lower,
                    pivot.right.clone(),
                    work,
                );
            }
            let pivot = left.0.as_ref().expect("left rotation pivot");
            let lower = Self::node(key, value, pivot.right.clone(), right, work);
            return Self::node(
                pivot.key.clone(),
                pivot.value.clone(),
                pivot.left.clone(),
                lower,
                work,
            );
        }
        if right.height() > left.height() + 1 {
            let child = right.0.as_ref().expect("right-heavy tree");
            if child.left.height() > child.right.height() {
                let pivot = child.left.0.as_ref().expect("inner-heavy child");
                let lower = Self::node(
                    child.key.clone(),
                    child.value.clone(),
                    pivot.right.clone(),
                    child.right.clone(),
                    work,
                );
                right = Self::node(
                    pivot.key.clone(),
                    pivot.value.clone(),
                    pivot.left.clone(),
                    lower,
                    work,
                );
            }
            let pivot = right.0.as_ref().expect("right rotation pivot");
            let lower = Self::node(key, value, left, pivot.left.clone(), work);
            return Self::node(
                pivot.key.clone(),
                pivot.value.clone(),
                lower,
                pivot.right.clone(),
                work,
            );
        }
        Self::node(key, value, left, right, work)
    }
    fn get(&self, key: &K) -> Option<&V> {
        let mut cursor = self.0.as_deref();
        while let Some(node) = cursor {
            match key.cmp(&node.key) {
                std::cmp::Ordering::Less => cursor = node.left.0.as_deref(),
                std::cmp::Ordering::Greater => cursor = node.right.0.as_deref(),
                std::cmp::Ordering::Equal => return Some(&node.value),
            }
        }
        None
    }
    fn insert(&self, key: K, value: V, work: &mut u64) -> Self {
        *work += 1;
        let Some(node) = self.0.as_ref() else {
            return Self::node(key, value, Self::new(), Self::new(), work);
        };
        match key.cmp(&node.key) {
            std::cmp::Ordering::Less => Self::balanced(
                node.key.clone(),
                node.value.clone(),
                node.left.insert(key, value, work),
                node.right.clone(),
                work,
            ),
            std::cmp::Ordering::Greater => Self::balanced(
                node.key.clone(),
                node.value.clone(),
                node.left.clone(),
                node.right.insert(key, value, work),
                work,
            ),
            std::cmp::Ordering::Equal => {
                Self::node(key, value, node.left.clone(), node.right.clone(), work)
            }
        }
    }
    #[cfg(test)]
    fn remove(&self, key: &K, work: &mut u64) -> Self {
        *work += 1;
        let Some(node) = self.0.as_ref() else {
            return self.clone();
        };
        match key.cmp(&node.key) {
            std::cmp::Ordering::Less => Self::balanced(
                node.key.clone(),
                node.value.clone(),
                node.left.remove(key, work),
                node.right.clone(),
                work,
            ),
            std::cmp::Ordering::Greater => Self::balanced(
                node.key.clone(),
                node.value.clone(),
                node.left.clone(),
                node.right.remove(key, work),
                work,
            ),
            std::cmp::Ordering::Equal => {
                if node.left.0.is_none() {
                    return node.right.clone();
                }
                if node.right.0.is_none() {
                    return node.left.clone();
                }
                let mut successor = node.right.0.as_deref().expect("nonempty right subtree");
                while let Some(next) = successor.left.0.as_deref() {
                    *work += 1;
                    successor = next;
                }
                Self::balanced(
                    successor.key.clone(),
                    successor.value.clone(),
                    node.left.clone(),
                    node.right.remove(&successor.key, work),
                    work,
                )
            }
        }
    }
    fn iter(&self) -> IndexIter<'_, K, V> {
        let mut iter = IndexIter { stack: Vec::new() };
        iter.descend(self.0.as_deref());
        iter
    }
    /// Seek once, then traverse only the requested ordered suffix.
    fn iter_from(&self, lower: &K, inclusive: bool) -> IndexIter<'_, K, V> {
        let mut iter = IndexIter { stack: Vec::new() };
        let mut cursor = self.0.as_deref();
        while let Some(node) = cursor {
            if node.key > *lower || (inclusive && node.key == *lower) {
                iter.stack.push(node);
                cursor = node.left.0.as_deref();
            } else {
                cursor = node.right.0.as_deref();
            }
        }
        iter
    }
    fn at(&self, mut rank: usize) -> Option<(&K, &V)> {
        let mut cursor = self.0.as_deref();
        while let Some(node) = cursor {
            let left = node.left.len();
            if rank < left {
                cursor = node.left.0.as_deref();
            } else if rank == left {
                return Some((&node.key, &node.value));
            } else {
                rank -= left + 1;
                cursor = node.right.0.as_deref();
            }
        }
        None
    }
}

pub(crate) struct IndexIter<'a, K, V> {
    stack: Vec<&'a IndexNode<K, V>>,
}
impl<'a, K, V> IndexIter<'a, K, V> {
    fn descend(&mut self, mut node: Option<&'a IndexNode<K, V>>) {
        while let Some(next) = node {
            self.stack.push(next);
            node = next.left.0.as_deref();
        }
    }
}
impl<'a, K, V> Iterator for IndexIter<'a, K, V> {
    type Item = (&'a K, &'a V);
    fn next(&mut self) -> Option<Self::Item> {
        let node = self.stack.pop()?;
        self.descend(node.right.0.as_deref());
        Some((&node.key, &node.value))
    }
}

#[cfg(test)]
mod persistent_index_tests {
    use super::IndexMap;
    #[test]
    fn updates_preserve_pinned_predecessor_and_order() {
        let mut map = IndexMap::new();
        let mut work = 0;
        for key in 0..400 {
            map = map.insert(key, key * 2, &mut work);
        }
        let pinned = map.clone();
        work = 0;
        map = map.insert(401, 802, &mut work);
        map = map.remove(&199, &mut work);
        assert!(work < 100, "path copying charged {work} nodes");
        assert_eq!(pinned.get(&199), Some(&398));
        assert_eq!(pinned.get(&401), None);
        assert_eq!(map.get(&199), None);
        assert_eq!(map.get(&401), Some(&802));
        assert_eq!(
            map.iter().map(|(k, _)| *k).collect::<Vec<_>>(),
            (0..400)
                .filter(|k| *k != 199)
                .chain([401])
                .collect::<Vec<_>>()
        );
        for key in (0..400).rev() {
            map = map.remove(&key, &mut work);
        }
        assert_eq!(map.at(0), Some((&401, &802)));
        assert_eq!(map.len(), 1);
        assert_eq!(pinned.len(), 400);
    }

    #[test]
    fn ordered_seek_respects_present_absent_and_exclusive_bounds() {
        let mut map = IndexMap::new();
        let mut work = 0;
        for key in [8, 2, 14, 0, 6, 10, 18, 4, 12, 16] {
            map = map.insert(key, (), &mut work);
        }
        for lower in -1..=20 {
            for inclusive in [false, true] {
                let expected: Vec<_> = (0..=18)
                    .step_by(2)
                    .filter(|key| *key > lower || (inclusive && *key == lower))
                    .collect();
                assert_eq!(
                    map.iter_from(&lower, inclusive)
                        .map(|(key, ())| *key)
                        .collect::<Vec<_>>(),
                    expected
                );
            }
        }
    }
}

/// Rebuildable, structurally shared coordinates into an admitted generation.
/// Publishing a suffix copies search paths, never a pinned predecessor's tree.
type History = IndexMap<(CommitSeq, usize, usize), ()>;
type Incidence = IndexMap<EId, ()>;

/// The `(block, row)` of the latest statement in `history` created at or
/// before `as_of`. Keys order `(created_at, block, row)`, so among one
/// statement's restatements the later block wins, as in the merge.
fn latest_statement(history: &History, as_of: CommitSeq) -> Option<(usize, usize)> {
    let mut low = 0;
    let mut high = history.len();
    while low < high {
        let middle = low + (high - low) / 2;
        if history.at(middle).expect("history rank").0.0 <= as_of {
            low = middle + 1;
        } else {
            high = middle;
        }
    }
    low.checked_sub(1)
        .and_then(|at| history.at(at))
        .map(|(&(_, block, row), _)| (block, row))
}

#[derive(Clone, Debug)]
pub(crate) struct AdjacencyIndex {
    histories: IndexMap<EId, History>,
    outgoing: IndexMap<VId, Incidence>,
    incoming: IndexMap<VId, Incidence>,
    work: u64,
}

impl AdjacencyIndex {
    pub(crate) fn build(blocks: &[Vec<AdjacencyEntry>]) -> Self {
        let mut index = Self {
            histories: IndexMap::new(),
            outgoing: IndexMap::new(),
            incoming: IndexMap::new(),
            work: 0,
        };
        index.apply_added(blocks, 0);
        index
    }

    /// The retained writer appends sealed objects in publication order.
    /// Compaction/open replace the writer and use `build` on their replacement
    /// generation. No predecessor tree is mutated, even with a pinned reader.
    pub(crate) fn extend(&self, blocks: &[Vec<AdjacencyEntry>], carried: usize) -> Self {
        let mut next = self.clone();
        next.apply_added(blocks, carried);
        next
    }

    pub(crate) fn maintenance_work(&self) -> u64 {
        self.work
    }

    pub(crate) fn equivalent(&self, other: &Self) -> bool {
        fn same<K: Ord + Clone + PartialEq, I: Ord + Clone + PartialEq>(
            a: &IndexMap<K, IndexMap<I, ()>>,
            b: &IndexMap<K, IndexMap<I, ()>>,
        ) -> bool {
            a.len() == b.len()
                && a.iter().zip(b.iter()).all(|((ak, av), (bk, bv))| {
                    ak == bk && av.len() == bv.len() && av.iter().eq(bv.iter())
                })
        }
        same(&self.histories, &other.histories)
            && same(&self.outgoing, &other.outgoing)
            && same(&self.incoming, &other.incoming)
    }

    pub(crate) fn apply_added(&mut self, blocks: &[Vec<AdjacencyEntry>], added_from: usize) -> u64 {
        self.work = 0;
        for (block, entries) in blocks.iter().enumerate().skip(added_from) {
            for (row, entry) in entries.iter().enumerate() {
                self.work += 1;
                let history = self
                    .histories
                    .get(&entry.eid)
                    .cloned()
                    .unwrap_or_else(IndexMap::new);
                let history = history.insert((entry.created_at, block, row), (), &mut self.work);
                self.histories = self.histories.insert(entry.eid, history, &mut self.work);
                for (face, endpoint) in [
                    (&mut self.outgoing, entry.src),
                    (&mut self.incoming, entry.dst),
                ] {
                    let incidence = face.get(&endpoint).cloned().unwrap_or_else(IndexMap::new);
                    let incidence = incidence.insert(entry.eid, (), &mut self.work);
                    *face = face.insert(endpoint, incidence, &mut self.work);
                }
            }
        }
        self.work
    }

    /// The distinct live neighbours of `endpoint` over `relation` at `as_of`,
    /// ascending: destinations for `Forward`, sources for `Reverse`.
    ///
    /// Every block in an admitted generation already passed the whole-history
    /// validator at publication or reopen, so this resolves each incident
    /// identity by the same rule the merge applies (latest statement at or
    /// before `as_of`, later block wins) without re-collapsing the history:
    /// O(incident identities · log versions) instead of O(history).
    pub(crate) fn neighbours_at(
        &self,
        blocks: &[Vec<AdjacencyEntry>],
        endpoint: VId,
        relation: RelationId,
        direction: fgdb_gql::algebra::GlaDirection,
        as_of: CommitSeq,
    ) -> Vec<VId> {
        use fgdb_gql::algebra::GlaDirection;
        let mut found = std::collections::BTreeSet::new();
        let mut control = |_: SourceEvent| Ok::<(), core::convert::Infallible>(());
        let Ok(()) = self.visit(
            blocks,
            endpoint,
            direction,
            as_of,
            &mut control,
            |entry, _, _, _| {
                if entry.relation == relation {
                    found.insert(match direction {
                        GlaDirection::Reverse => entry.src,
                        _ => entry.dst,
                    });
                }
                Ok(())
            },
        );
        found.into_iter().collect()
    }

    /// The `(block, row)` of `eid`'s statement visible at `as_of`, if any —
    /// the point-lookup face of the same resolution rule.
    pub(crate) fn statement_at(
        &self,
        blocks: &[Vec<AdjacencyEntry>],
        eid: EId,
        as_of: CommitSeq,
    ) -> Option<(usize, usize)> {
        let (block, row) = latest_statement(self.histories.get(&eid)?, as_of)?;
        blocks[block][row].visible_at(as_of).then_some((block, row))
    }

    /// Visit the surviving versions of every incident identity at `as_of`,
    /// merging both faces in EId order; self loops appear once.
    fn visit<'a, E, C>(
        &'a self,
        blocks: &'a [Vec<AdjacencyEntry>],
        endpoint: VId,
        direction: fgdb_gql::algebra::GlaDirection,
        as_of: CommitSeq,
        control: &mut C,
        mut visit: impl FnMut(&'a AdjacencyEntry, usize, usize, &mut C) -> Result<(), E>,
    ) -> Result<(), E>
    where
        C: FnMut(SourceEvent) -> Result<(), E>,
    {
        use fgdb_gql::algebra::GlaDirection;
        control(SourceEvent::Work)?;
        let face = |set: &'a Incidence| set.iter().map(|(key, _)| key).peekable();
        let outgoing = self.outgoing.get(&endpoint).map(face);
        let incoming = self.incoming.get(&endpoint).map(face);
        let (mut left, mut right) = match direction {
            GlaDirection::Forward => (outgoing, None),
            GlaDirection::Reverse => (incoming, None),
            GlaDirection::Undirected => (outgoing, incoming),
        };
        loop {
            let id = match (&mut left, &mut right) {
                (Some(a), Some(b)) => match (a.peek(), b.peek()) {
                    (Some(x), Some(y)) => Some((*x).min(*y)),
                    (Some(x), None) => Some(*x),
                    (None, Some(y)) => Some(*y),
                    (None, None) => break,
                },
                (Some(a), None) => a.peek().copied(),
                (None, Some(b)) => b.peek().copied(),
                (None, None) => break,
            };
            let Some(id) = id else { break };
            control(SourceEvent::Work)?;
            if let Some(a) = left.as_mut()
                && a.peek() == Some(&id)
            {
                a.next();
            }
            if let Some(b) = right.as_mut()
                && b.peek() == Some(&id)
            {
                b.next();
            }
            let history = self.histories.get(id).expect("incidence has a history");
            let Some((block, row)) = latest_statement(history, as_of) else {
                continue;
            };
            let entry = &blocks[block][row];
            let incident = match direction {
                GlaDirection::Forward => entry.src == endpoint,
                GlaDirection::Reverse => entry.dst == endpoint,
                GlaDirection::Undirected => entry.src == endpoint || entry.dst == endpoint,
            };
            if incident && entry.visible_at(as_of) {
                visit(entry, block, row, control)?;
            }
        }
        Ok(())
    }
}

/// Rebuildable equality candidates over one admitted generation, never an
/// authority beside its patches (FG-INV-18). Keys are (property key, canonical
/// scalar transcript) seen in ANY version of a vertex's history; candidates
/// are a sorted, deduped superset. The visible winner row at `as_of` is the
/// only authority — the caller re-checks the predicate against it.
#[derive(Clone, Debug)]
pub(crate) struct PropertyEqualityIndex {
    candidates: IndexMap<(PropertyKeyId, std::sync::Arc<[u8]>), IndexMap<VId, ()>>,
    histories: IndexMap<VId, History>,
    work: u64,
}

impl PropertyEqualityIndex {
    pub(crate) fn build(patches: &[VertexPatchRows]) -> Self {
        let mut index = Self {
            candidates: IndexMap::new(),
            histories: IndexMap::new(),
            work: 0,
        };
        index.apply_added(patches, 0);
        index
    }

    pub(crate) fn extend(&self, patches: &[VertexPatchRows], carried: usize) -> Self {
        let mut next = self.clone();
        next.apply_added(patches, carried);
        next
    }

    pub(crate) fn maintenance_work(&self) -> u64 {
        self.work
    }

    pub(crate) fn equivalent(&self, other: &Self) -> bool {
        self.histories.len() == other.histories.len()
            && self
                .histories
                .iter()
                .zip(other.histories.iter())
                .all(|((ak, av), (bk, bv))| {
                    ak == bk && av.len() == bv.len() && av.iter().eq(bv.iter())
                })
            && self.candidates.len() == other.candidates.len()
            && self
                .candidates
                .iter()
                .zip(other.candidates.iter())
                .all(|((ak, av), (bk, bv))| {
                    ak == bk && av.len() == bv.len() && av.iter().eq(bv.iter())
                })
    }

    pub(crate) fn apply_added(&mut self, patches: &[VertexPatchRows], added_from: usize) -> u64 {
        self.work = 0;
        for (patch, rows) in patches.iter().enumerate().skip(added_from) {
            for (row_at, row) in rows.iter().enumerate() {
                self.work += 1;
                let history = self
                    .histories
                    .get(&row.vid)
                    .cloned()
                    .unwrap_or_else(IndexMap::new);
                let history = history.insert((row.created_at, patch, row_at), (), &mut self.work);
                self.histories = self.histories.insert(row.vid, history, &mut self.work);
                for (key, value) in &row.props {
                    self.work += 1;
                    if matches!(value, CanonicalScalar::Null) {
                        continue;
                    }
                    let Ok(encoded) = value.encode() else {
                        continue;
                    };
                    self.work += encoded.len() as u64;
                    let key = (*key, std::sync::Arc::from(encoded));
                    let candidates = self
                        .candidates
                        .get(&key)
                        .cloned()
                        .unwrap_or_else(IndexMap::new);
                    let candidates = candidates.insert(row.vid, (), &mut self.work);
                    self.candidates = self.candidates.insert(key, candidates, &mut self.work);
                }
            }
        }
        self.work
    }

    pub(crate) fn lookup(&self, key: PropertyKeyId, value: &CanonicalScalar) -> Candidates<'_> {
        let Ok(encoded) = value.encode() else {
            return Candidates(None);
        };
        Candidates(self.candidates.get(&(key, std::sync::Arc::from(encoded))))
    }

    /// Ordered scalar bytes have the same order as CanonicalScalar::cmp.
    /// Bounds stay within one type tag: predicate.rs::accepts_scalar_pair
    /// rejects heterogeneous pairs, rather than comparing their type ranks.
    fn range_candidates<E>(
        &self,
        range: &PropertyRange,
        control: &mut impl FnMut(SourceEvent) -> Result<(), E>,
    ) -> Result<std::collections::BTreeSet<VId>, E> {
        let mut ids = std::collections::BTreeSet::new();
        if range.empty {
            return Ok(ids);
        }
        let lower = (range.key, std::sync::Arc::from(range.lower.as_slice()));
        for ((key, encoded), candidates) in self.candidates.iter_from(&lower, range.lower_inclusive)
        {
            if *key != range.key
                || encoded.as_ref() > range.upper.as_slice()
                || (!range.upper_inclusive && encoded.as_ref() == range.upper.as_slice())
            {
                break;
            }
            for (vid, ()) in candidates.iter() {
                if !ids.contains(vid) {
                    control(SourceEvent::ScratchEntry)?;
                    ids.insert(*vid);
                }
            }
        }
        Ok(ids)
    }

    /// Latest statement at the cut, with later patches winning equal creation
    /// sequences exactly as in visit_vertices. Retirements remain authoritative.
    fn visible_row<'a, E>(
        &self,
        patches: &'a [VertexPatchRows],
        vid: VId,
        as_of: CommitSeq,
        control: &mut impl FnMut(SourceEvent) -> Result<(), E>,
    ) -> Result<Option<&'a VertexRow>, E> {
        let Some(history) = self.histories.get(&vid) else {
            return Ok(None);
        };
        let (mut low, mut high) = (0, history.len());
        while low < high {
            control(SourceEvent::Work)?;
            let middle = low + (high - low) / 2;
            if history.at(middle).expect("history rank").0.0 <= as_of {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        Ok(low.checked_sub(1).and_then(|at| {
            let (&(_, patch, row), _) = history.at(at).expect("history rank");
            let row = &patches[patch][row];
            row.visible_at(as_of).then_some(row)
        }))
    }
}

#[derive(Clone, Copy)]
pub(crate) struct Candidates<'a>(Option<&'a IndexMap<VId, ()>>);
impl Candidates<'_> {
    fn len(self) -> usize {
        self.0.map_or(0, IndexMap::len)
    }
    #[cfg(test)]
    fn to_vec(self) -> Vec<VId> {
        self.into_iter().copied().collect()
    }
}
impl<'a> IntoIterator for Candidates<'a> {
    type Item = &'a VId;
    type IntoIter = std::iter::Map<IndexIter<'a, VId, ()>, fn((&'a VId, &'a ())) -> &'a VId>;
    fn into_iter(self) -> Self::IntoIter {
        let iter = self
            .0
            .map_or(IndexIter { stack: Vec::new() }, IndexMap::iter);
        iter.map(|(vid, _)| vid)
    }
}

/// Intersection of constraints on one property and one canonical scalar kind.
struct PropertyRange {
    key: PropertyKeyId,
    lower: Vec<u8>,
    upper: Vec<u8>,
    lower_inclusive: bool,
    upper_inclusive: bool,
    tag: u8,
    empty: bool,
}

impl PropertyRange {
    fn new(key: PropertyKeyId, encoded: &[u8]) -> Self {
        let tag = encoded[0];
        Self {
            key,
            lower: vec![tag],
            upper: vec![tag + 1],
            lower_inclusive: true,
            upper_inclusive: false,
            tag,
            empty: false,
        }
    }

    fn constrain(&mut self, encoded: Vec<u8>, comparison: fgdb_gql::algebra::IntegerComparison) {
        use fgdb_gql::algebra::IntegerComparison as C;
        if encoded[0] != self.tag {
            self.empty = true;
            return;
        }
        let inclusive = matches!(comparison, C::Equal | C::LessOrEqual | C::GreaterOrEqual);
        if matches!(comparison, C::Equal | C::Greater | C::GreaterOrEqual) {
            match encoded.cmp(&self.lower) {
                std::cmp::Ordering::Greater => {
                    self.lower = encoded.clone();
                    self.lower_inclusive = inclusive;
                }
                std::cmp::Ordering::Equal => self.lower_inclusive &= inclusive,
                std::cmp::Ordering::Less => {}
            }
        }
        if matches!(comparison, C::Equal | C::Less | C::LessOrEqual) {
            match encoded.cmp(&self.upper) {
                std::cmp::Ordering::Less => {
                    self.upper = encoded;
                    self.upper_inclusive = inclusive;
                }
                std::cmp::Ordering::Equal => self.upper_inclusive &= inclusive,
                std::cmp::Ordering::Greater => {}
            }
        }
        self.empty |= self.lower > self.upper
            || (self.lower == self.upper && !(self.lower_inclusive && self.upper_inclusive));
    }
}

/// UCS_BASIC ordering is UTF-8 lexicographic order, hence scalar-value order.
/// Carry past maximal scalars and skip the surrogate gap. An exhausted prefix
/// has no text successor; the binding boundary is its exclusive upper bound.
fn prefix_successor(prefix: &str) -> Option<String> {
    for (at, scalar) in prefix.char_indices().rev() {
        if scalar == char::MAX {
            continue;
        }
        let next = if scalar == '\u{d7ff}' {
            '\u{e000}'
        } else {
            char::from_u32(u32::from(scalar) + 1).expect("nonmaximal nonsurrogate successor")
        };
        let mut successor = String::with_capacity(at + next.len_utf8());
        successor.push_str(&prefix[..at]);
        successor.push(next);
        return Some(successor);
    }
    None
}

fn prefix_range(key: PropertyKeyId, prefix: &str) -> Option<PropertyRange> {
    let lower = CanonicalScalar::ucs_basic_text(prefix)
        .ok()?
        .encode()
        .ok()?;
    let mut range = PropertyRange::new(key, &lower);
    // Restrict even the empty and all-maximal prefix to UCS_BASIC, not other
    // collations: STARTS WITH evaluates spelling, never a collation sort key.
    range.upper = vec![lower[0], lower[1] + 1];
    range.lower = lower;
    if let Some(successor) = prefix_successor(prefix) {
        range.upper = CanonicalScalar::ucs_basic_text(&successor)
            .ok()?
            .encode()
            .ok()?;
    }
    Some(range)
}

impl PropertyEqualityIndex {
    /// Refuse pruning if any historical property could raise NonText, or uses
    /// nonbinary ordering. Two ordered seeks inspect type/binding boundaries;
    /// no whole-property scan and no additional maintained index are needed.
    fn prefix_types_are_safe(&self, range: &PropertyRange) -> bool {
        let null = CanonicalScalar::Null.encode().expect("null encoding");
        let after_null = (range.key, std::sync::Arc::from(null));
        if let Some(((key, encoded), _)) = self.candidates.iter_from(&after_null, false).next()
            && *key == range.key
            && encoded.as_ref() < &range.lower[..2]
        {
            return false;
        }
        let after_binary = (
            range.key,
            std::sync::Arc::from(vec![range.lower[0], range.lower[1] + 1]),
        );
        !matches!(self.candidates.iter_from(&after_binary, true).next(), Some(((key, _), _)) if *key == range.key)
    }
}

/// Whether property-bound single-domain scans use the maintained index.
const PROPERTY_INDEX_SERVING: bool = true;

/// Serve equality or range predicates over a single vertex domain.
/// The admitted rows also supply every nested scan and property lookup, so a
/// root predicate cannot prune that shared table when later operators introduce
/// other bindings. Such plans retain the complete scan source.
fn bound_vertices<'a, E, Row>(
    snapshot: &'a Snapshot,
    logical: &fgdb_gql::algebra::GlaPlan<Row>,
    as_of: CommitSeq,
    control: &mut impl FnMut(SourceEvent) -> Result<(), E>,
) -> Result<Option<(Vec<&'a VertexRow>, u64)>, E> {
    use fgdb_gql::algebra::{GlaOperator, IntegerComparison, VertexPredicate};
    if !matches!(logical.operators().first(), Some(GlaOperator::ScanVertices)) {
        return Ok(None);
    }
    let prefix = &logical.operators()[1..];
    if !prefix.iter().all(|op| match op {
        GlaOperator::Select { slot, .. } => slot.ordinal() == 0,
        GlaOperator::SelectBoolean { expression } => expression.starts_with_conjunct().is_some(),
        GlaOperator::Project { .. }
        | GlaOperator::Distinct
        | GlaOperator::OrderByVertexId
        | GlaOperator::ProjectBindings { .. }
        | GlaOperator::OrderByBindings
        | GlaOperator::ProjectValues { .. }
        | GlaOperator::OrderByValues
        | GlaOperator::OrderByValueColumns { .. }
        | GlaOperator::Limit { .. } => true,
        _ => false,
    }) {
        return Ok(None);
    }
    if let Some((key, text)) = prefix.iter().find_map(|operator| match operator {
        GlaOperator::SelectBoolean { expression } => expression.starts_with_conjunct(),
        _ => None,
    }) {
        let Some(range) = prefix_range(key, text) else {
            return Ok(None);
        };
        if !snapshot.property_index.prefix_types_are_safe(&range) {
            return Ok(None);
        }
        let candidates = snapshot.property_index.range_candidates(&range, control)?;
        let mut rows = Vec::new();
        for vid in &candidates {
            control(SourceEvent::Work)?;
            control(SourceEvent::SnapshotRecord)?;
            if let Some(row) =
                snapshot
                    .property_index
                    .visible_row(&snapshot.patches, *vid, as_of, control)?
            {
                // Historical membership is only a candidate superset. Recheck
                // spelling on the visible row; retain all original GLA filters.
                if row.props.iter().any(|(property, value)| {
                    *property == key && matches!(value, CanonicalScalar::Text(value) if value.as_str().starts_with(text))
                }) {
                    control(SourceEvent::ScratchEntry)?;
                    rows.push(row);
                }
            }
        }
        return Ok(Some((rows, candidates.len() as u64)));
    }
    let mut equality: Option<(PropertyKeyId, CanonicalScalar)> = None;
    let mut bound_predicates: &[VertexPredicate] = &[];
    for op in prefix {
        match op {
            GlaOperator::Select { slot, predicates } if slot.ordinal() == 0 => {
                let mut found = None;
                for predicate in predicates {
                    let (key, value) = match predicate {
                        VertexPredicate::IntegerProperty {
                            key,
                            comparison: IntegerComparison::Equal,
                            value,
                        } => (*key, CanonicalScalar::Int(*value)),
                        VertexPredicate::ScalarProperty { key, predicate }
                            if predicate.comparison() == IntegerComparison::Equal =>
                        {
                            // Equality on a non-integer canonical scalar; a
                            // stored Null is never an equality candidate.
                            match predicate.value() {
                                CanonicalScalar::Null => continue,
                                scalar => (*key, scalar.clone()),
                            }
                        }
                        _ => continue,
                    };
                    found = Some((key, value));
                    break;
                }
                if let Some((key, value)) = found {
                    equality = Some((key, value));
                    bound_predicates = predicates;
                    break;
                }
            }
            // Preserve equality preference. Other allowed output operators
            // need no equality extraction; the range fallback handles them.
            GlaOperator::Project { .. } | GlaOperator::Distinct | GlaOperator::OrderByVertexId => {}
            _ => break,
        }
    }
    let Some((key, value)) = equality else {
        return range_vertices(snapshot, prefix, as_of, control);
    };
    let mut rows = Vec::new();
    let candidates = snapshot.property_index.lookup(key, &value);
    for vid in candidates {
        control(SourceEvent::Work)?;
        // History candidates consume admission even when their visible row
        // has changed value or retired. Charge before resolving that history.
        control(SourceEvent::SnapshotRecord)?;
        // Resolve only this candidate's history, not every snapshot patch.
        // The visible row remains the authority for the complete predicate.
        if let Some(row) =
            snapshot
                .property_index
                .visible_row(&snapshot.patches, *vid, as_of, control)?
        {
            if bound_predicates
                .iter()
                .all(|predicate| predicate.matches(&row.labels, &row.props))
            {
                control(SourceEvent::ScratchEntry)?;
                rows.push(row);
            }
        }
    }
    Ok(Some((rows, candidates.len() as u64)))
}

fn range_vertices<'a, E>(
    snapshot: &'a Snapshot,
    operators: &[fgdb_gql::algebra::GlaOperator],
    as_of: CommitSeq,
    control: &mut impl FnMut(SourceEvent) -> Result<(), E>,
) -> Result<Option<(Vec<&'a VertexRow>, u64)>, E> {
    use fgdb_gql::algebra::{GlaOperator, IntegerComparison, VertexPredicate};
    let predicates = || {
        operators
            .iter()
            .filter_map(|operator| match operator {
                GlaOperator::Select { predicates, .. } => Some(predicates.as_slice()),
                _ => None,
            })
            .flatten()
    };
    let mut range: Option<PropertyRange> = None;
    for predicate in predicates() {
        let (key, comparison, encoded) = match predicate {
            VertexPredicate::IntegerProperty {
                key,
                comparison,
                value,
            } => (
                *key,
                *comparison,
                CanonicalScalar::Int(*value)
                    .encode()
                    .expect("integer encoding"),
            ),
            VertexPredicate::ScalarProperty { key, predicate } => {
                if matches!(predicate.value(), CanonicalScalar::Null) {
                    continue;
                }
                (
                    *key,
                    predicate.comparison(),
                    predicate.canonical_value_bytes().to_vec(),
                )
            }
            _ => continue,
        };
        if comparison == IntegerComparison::NotEqual {
            continue;
        }
        let selected = range.get_or_insert_with(|| PropertyRange::new(key, &encoded));
        if selected.key == key {
            selected.constrain(encoded, comparison);
        }
    }
    let Some(range) = range else {
        return Ok(None);
    };
    let candidates = snapshot.property_index.range_candidates(&range, control)?;
    let mut rows = Vec::new();
    for vid in &candidates {
        control(SourceEvent::Work)?;
        control(SourceEvent::SnapshotRecord)?;
        if let Some(row) =
            snapshot
                .property_index
                .visible_row(&snapshot.patches, *vid, as_of, control)?
        {
            if predicates().all(|predicate| predicate.matches(&row.labels, &row.props)) {
                control(SourceEvent::ScratchEntry)?;
                rows.push(row);
            }
        }
    }
    Ok(Some((rows, candidates.len() as u64)))
}

#[cfg(test)]
mod indexed_tests {
    use super::*;
    use fgdb_gql::algebra::GlaDirection;

    #[test]
    fn indexed_history_matches_scan_before_and_after_retirement() {
        for seed in [3_u128, 17, 91] {
            let mut blocks = Vec::new();
            for seq in 1..=6 {
                let mut block = Vec::new();
                for id in 1..=30 {
                    block.push(AdjacencyEntry {
                        src: VId((id * seed) % 7),
                        dst: VId((id * seed + id / 3) % 7),
                        relation: RelationId(1),
                        eid: EId(id),
                        created_at: CommitSeq(seq),
                        retired_at: (id % 4 == 0 && seq >= 4).then_some(CommitSeq(4)),
                    });
                }
                blocks.push(block);
            }
            let index = AdjacencyIndex::build(&blocks);
            let mut nonempty = false;
            let mut deleted = false;
            for at in 0..=7 {
                for endpoint in (0..7).map(VId) {
                    for direction in [
                        GlaDirection::Forward,
                        GlaDirection::Reverse,
                        GlaDirection::Undirected,
                    ] {
                        let mut expected = Vec::new();
                        visit_edges(
                            &blocks,
                            CommitSeq(at),
                            &mut |_| Ok::<_, ()>(()),
                            |entry, _| {
                                let incident = match direction {
                                    GlaDirection::Forward => entry.src == endpoint,
                                    GlaDirection::Reverse => entry.dst == endpoint,
                                    GlaDirection::Undirected => {
                                        entry.src == endpoint || entry.dst == endpoint
                                    }
                                };
                                if incident {
                                    expected.push(entry);
                                }
                                Ok(())
                            },
                        )
                        .unwrap();
                        let mut actual = Vec::new();
                        index
                            .visit(
                                &blocks,
                                endpoint,
                                direction,
                                CommitSeq(at),
                                &mut |_| Ok::<_, ()>(()),
                                |entry, _, _, _| {
                                    actual.push(entry);
                                    Ok(())
                                },
                            )
                            .unwrap();
                        assert_eq!(
                            actual, expected,
                            "seed={seed} at={at} endpoint={endpoint:?} direction={direction:?}"
                        );
                        nonempty |= !actual.is_empty();
                        if at >= 4 {
                            assert!(actual.iter().all(|entry| entry.eid.0 % 4 != 0));
                            deleted = true;
                        }
                    }
                }
            }
            assert!(nonempty && deleted);
        }
    }

    #[test]
    fn indexed_lookup_refuses_at_every_source_event() {
        let blocks = vec![vec![AdjacencyEntry {
            src: VId(1),
            dst: VId(2),
            relation: RelationId(1),
            eid: EId(1),
            created_at: CommitSeq(1),
            retired_at: None,
        }]];
        let index = AdjacencyIndex::build(&blocks);
        let run = |stop| {
            let mut seen = 0;
            let result = index.visit(
                &blocks,
                VId(1),
                GlaDirection::Forward,
                CommitSeq(1),
                &mut |_| {
                    seen += 1;
                    if seen == stop { Err(stop) } else { Ok(()) }
                },
                |_, _, _, control| {
                    control(SourceEvent::SnapshotRecord)?;
                    control(SourceEvent::ScratchEntry)
                },
            );
            (result, seen)
        };
        let (result, total) = run(usize::MAX);
        assert_eq!(result, Ok(()));
        assert!(total >= 4);
        for stop in 1..=total {
            assert_eq!(run(stop), (Err(stop), stop));
        }
    }

    #[test]
    fn property_index_candidates_are_a_superset_of_visible_winners() {
        let row = |vid: u64, created: u64, retired: Option<u64>, value: i64| VertexRow {
            vid: VId(vid as u128),
            birth_ordinal: vid,
            created_at: CommitSeq(created),
            retired_at: retired.map(CommitSeq),
            labels: vec![fgdb_delta_types::LabelId(1)],
            props: vec![(PropertyKeyId(1), CanonicalScalar::Int(value))],
        };
        let patch = |rows: &[VertexRow]| {
            let bytes = fgdb_strata::vertex::encode_patch(rows).unwrap();
            fgdb_strata::vertex::decode_patch(&bytes).unwrap()
        };
        let patches = vec![
            // v1: value 7 at creation, changed to 8; v2 created with 7 later.
            patch(&[VertexRow {
                ..row(1, 1, None, 7)
            }]),
            patch(&[
                VertexRow {
                    retired_at: Some(CommitSeq(2)),
                    ..row(1, 1, None, 7)
                },
                row(1, 2, None, 8),
                row(2, 2, None, 7),
            ]),
            // v2 deleted at seq 3.
            patch(&[VertexRow {
                retired_at: Some(CommitSeq(3)),
                ..row(2, 2, None, 7)
            }]),
        ];
        let index = PropertyEqualityIndex::build(&patches);
        // Candidates cover every history carrier of the value; they are a
        // superset of the visible winners at any single cut.
        let candidates: Vec<VId> = index
            .lookup(PropertyKeyId(1), &CanonicalScalar::Int(7))
            .to_vec();
        assert_eq!(candidates, vec![VId(1), VId(2)]);
        let mut ever_nonempty = false;
        for at in 0..=4 {
            let visible = scan_vertices(&patches, CommitSeq(at), &mut |_| Ok::<_, ()>(()))
                .unwrap()
                .into_iter()
                .filter(|row| {
                    row.props.iter().any(|(key, value)| {
                        *key == PropertyKeyId(1) && *value == CanonicalScalar::Int(7)
                    })
                })
                .map(|row| row.vid)
                .collect::<Vec<_>>();
            // At every cut the re-checked winner set equals the scan answer.
            let mut winners = Vec::new();
            for vid in index.lookup(PropertyKeyId(1), &CanonicalScalar::Int(7)) {
                if let Some(row) = index
                    .visible_row(&patches, *vid, CommitSeq(at), &mut |_| Ok::<_, ()>(()))
                    .unwrap()
                {
                    if row.props.iter().any(|(key, value)| {
                        *key == PropertyKeyId(1) && *value == CanonicalScalar::Int(7)
                    }) {
                        winners.push(row.vid);
                    }
                }
            }
            assert_eq!(winners, visible, "at={at}: recheck must equal the scan");
            ever_nonempty |= !visible.is_empty();
        }
        assert!(ever_nonempty);
    }
}

type BorrowedEdge<'a> = (IdentifiedEdge, &'a [(PropertyKeyId, CanonicalScalar)]);

fn edge_properties_at(
    props: &[Option<fgdb_strata::edge_props::BlockProps>],
    block: usize,
    row: usize,
) -> &[(PropertyKeyId, CanonicalScalar)] {
    let Some(Some(props)) = props.get(block) else {
        return &[];
    };
    let locator = props.locators[row];
    if locator == 0 {
        &[]
    } else {
        &props.rows[usize::from(locator) - 1]
    }
}

/// Borrow properties from the exact historical row selected for the edge.
pub(crate) fn visit_edges_with_properties<'a, E, C>(
    snapshot: &'a Snapshot,
    as_of: CommitSeq,
    control: &mut C,
    mut visit: impl FnMut(
        &'a AdjacencyEntry,
        &'a [(PropertyKeyId, CanonicalScalar)],
        &mut C,
    ) -> Result<(), E>,
) -> Result<(), E>
where
    C: FnMut(SourceEvent) -> Result<(), E>,
{
    visit_edge_coordinates(
        &snapshot.blocks,
        as_of,
        control,
        |entry, block, row, control| {
            visit(
                entry,
                edge_properties_at(&snapshot.block_props, block, row),
                control,
            )
        },
    )
}

#[allow(dead_code)]
pub(crate) fn visit_edges<'a, E, C>(
    blocks: &'a [Vec<AdjacencyEntry>],
    as_of: CommitSeq,
    control: &mut C,
    mut visit: impl FnMut(&'a AdjacencyEntry, &mut C) -> Result<(), E>,
) -> Result<(), E>
where
    C: FnMut(SourceEvent) -> Result<(), E>,
{
    visit_edge_coordinates(blocks, as_of, control, |entry, _, _, control| {
        visit(entry, control)
    })
}

fn visit_edge_coordinates<'a, E, C>(
    blocks: &'a [Vec<AdjacencyEntry>],
    as_of: CommitSeq,
    control: &mut C,
    mut visit: impl FnMut(&'a AdjacencyEntry, usize, usize, &mut C) -> Result<(), E>,
) -> Result<(), E>
where
    C: FnMut(SourceEvent) -> Result<(), E>,
{
    let mut winners: BTreeMap<EId, (&AdjacencyEntry, usize, usize)> = BTreeMap::new();
    for (block_at, block) in blocks.iter().enumerate() {
        control(SourceEvent::Work)?;
        for (row_at, entry) in block.iter().enumerate() {
            control(SourceEvent::Work)?;
            if entry.created_at > as_of {
                continue;
            }
            match winners.get(&entry.eid) {
                Some((previous, _, _)) if previous.created_at > entry.created_at => continue,
                Some(_) => {}
                None => control(SourceEvent::ScratchEntry)?,
            }
            winners.insert(entry.eid, (entry, block_at, row_at));
        }
    }
    for (entry, block, row) in winners.into_values() {
        control(SourceEvent::Work)?;
        if entry.visible_at(as_of) {
            visit(entry, block, row, control)?;
        }
    }
    Ok(())
}
fn scan_edges<'a, E>(
    blocks: &[Vec<AdjacencyEntry>],
    props: &'a [Option<fgdb_strata::edge_props::BlockProps>],
    as_of: CommitSeq,
    control: &mut impl FnMut(SourceEvent) -> Result<(), E>,
) -> Result<Vec<BorrowedEdge<'a>>, E> {
    let mut rows = Vec::new();
    visit_edge_coordinates(blocks, as_of, control, |entry, block, row, control| {
        control(SourceEvent::SnapshotRecord)?;
        control(SourceEvent::ScratchEntry)?;
        rows.push((
            (entry.eid, entry.src, entry.relation, entry.dst),
            edge_properties_at(props, block, row),
        ));
        Ok(())
    })?;
    Ok(rows)
}

/// One reusable heap cursor per nonempty patch; winning rows remain borrowed.
pub(crate) fn visit_vertices<'a, E, C>(
    patches: &'a [VertexPatchRows],
    as_of: CommitSeq,
    control: &mut C,
    mut visit: impl FnMut(&'a VertexRow, &mut C) -> Result<(), E>,
) -> Result<(), E>
where
    C: FnMut(SourceEvent) -> Result<(), E>,
{
    let mut heap: BinaryHeap<VertexCursor> = BinaryHeap::new();
    for (patch_at, patch) in patches.iter().enumerate() {
        control(SourceEvent::Work)?;
        if let Some(row) = patch.first() {
            control(SourceEvent::ScratchEntry)?;
            heap.push(Reverse((row.vid, row.created_at, patch_at, 0)));
        }
    }
    let mut group = None;
    let mut winner: Option<&VertexRow> = None;
    while let Some(mut cursor) = heap.peek_mut() {
        let Reverse((vid, _, patch_at, row_at)) = *cursor;
        control(SourceEvent::Work)?;
        if group != Some(vid) {
            if let Some(row) = winner.take().filter(|row| row.visible_at(as_of)) {
                visit(row, control)?;
            }
            group = Some(vid);
        }
        let patch = &patches[patch_at];
        let row = &patch[row_at];
        if row.created_at <= as_of {
            winner = Some(row);
        }
        if let Some(next) = patch.get(row_at + 1) {
            // Advance this patch with one heap repair instead of pop plus push.
            *cursor = Reverse((next.vid, next.created_at, patch_at, row_at + 1));
        } else {
            std::collections::binary_heap::PeekMut::pop(cursor);
        }
    }
    if let Some(row) = winner.filter(|row| row.visible_at(as_of)) {
        visit(row, control)?;
    }
    Ok(())
}
pub(crate) fn scan_vertices<'a, E>(
    patches: &'a [VertexPatchRows],
    as_of: CommitSeq,
    control: &mut impl FnMut(SourceEvent) -> Result<(), E>,
) -> Result<Vec<&'a VertexRow>, E> {
    let mut rows = Vec::new();
    visit_vertices(patches, as_of, control, |row, control| {
        control(SourceEvent::SnapshotRecord)?;
        control(SourceEvent::ScratchEntry)?;
        rows.push(row);
        Ok(())
    })?;
    Ok(rows)
}

pub(crate) fn find_vertex<'a, E>(
    patches: &'a [VertexPatchRows],
    vid: VId,
    as_of: CommitSeq,
    control: &mut impl FnMut(SourceEvent) -> Result<(), E>,
) -> Result<Option<&'a VertexRow>, E> {
    let mut winner: Option<&'a VertexRow> = None;
    for patch in patches {
        control(SourceEvent::Work)?;
        let (mut low, mut high) = (0, patch.len());
        while low < high {
            control(SourceEvent::Work)?;
            let middle = low + (high - low) / 2;
            let row = &patch[middle];
            if (row.vid, row.created_at) <= (vid, as_of) {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        if low > 0 {
            let row = &patch[low - 1];
            if row.vid == vid && winner.is_none_or(|old| old.created_at <= row.created_at) {
                winner = Some(row);
            }
        }
    }
    Ok(winner.filter(|row| row.visible_at(as_of)))
}

pub(super) struct BorrowedTables<'a> {
    pub(super) vertices: Vec<&'a VertexRow>,
    pub(super) edges: Vec<BorrowedEdge<'a>>,
    pub(super) snapshot_records: u64,
}

/// A root-adjacency closure is complete only for a flat, fixed-hop plan.
/// Nested scopes may start from another component (including isolated
/// vertices), and a variable-length atom can consume more than one edge.
/// Counting only Expand in those plans would silently remove valid witnesses.
fn bound_edge_hops(operators: &[fgdb_gql::algebra::GlaOperator]) -> Option<usize> {
    use fgdb_gql::algebra::GlaOperator;
    let mut hops = 0;
    for operator in operators {
        match operator {
            GlaOperator::Expand { .. } => hops += 1,
            GlaOperator::ScanVertices
            | GlaOperator::ScanEdges { .. }
            | GlaOperator::VarLengthExpand { .. }
            | GlaOperator::Probe { .. }
            | GlaOperator::ProbeEnd { .. }
            | GlaOperator::Optional { .. }
            | GlaOperator::OptionalEnd { .. } => return None,
            _ => {}
        }
    }
    Some(hops)
}

/// Admit a conservative edge closure for a predicate-bound root. The algebra
/// still evaluates every predicate/join and owns multiplicity and ordering.
/// Unbound scans retain the original source and its accounting verbatim.
fn bound_edges<'a, E, Row>(
    snapshot: &'a Snapshot,
    logical: &fgdb_gql::algebra::GlaPlan<Row>,
    as_of: CommitSeq,
    control: &mut impl FnMut(SourceEvent) -> Result<(), E>,
) -> Result<Option<Vec<BorrowedEdge<'a>>>, E> {
    use fgdb_gql::algebra::{GlaDirection, GlaOperator};
    let Some(GlaOperator::ScanEdges {
        relation,
        direction,
    }) = logical.operators().first()
    else {
        return Ok(None);
    };
    let prefix = &logical.operators()[1..];
    let Some(hops) = bound_edge_hops(prefix) else {
        return Ok(None);
    };
    let predicate = prefix
        .iter()
        .take_while(|op| {
            !matches!(
                op,
                GlaOperator::Expand { .. }
                    | GlaOperator::VarLengthExpand { .. }
                    | GlaOperator::Probe { .. }
                    | GlaOperator::Optional { .. }
                    | GlaOperator::ScanVertices
            )
        })
        .find_map(|op| match op {
            GlaOperator::Select { slot, predicates } if slot.ordinal() < 2 => {
                Some((slot.ordinal(), predicates))
            }
            _ => None,
        });
    let Some((slot, predicates)) = predicate else {
        return Ok(None);
    };
    let lookup_direction = if slot == 0 {
        *direction
    } else {
        match direction {
            GlaDirection::Forward => GlaDirection::Reverse,
            GlaDirection::Reverse => GlaDirection::Forward,
            GlaDirection::Undirected => GlaDirection::Undirected,
        }
    };
    let mut selected = BTreeMap::<EId, BorrowedEdge<'a>>::new();
    let mut frontier = std::collections::BTreeSet::new();
    visit_vertices(&snapshot.patches, as_of, control, |row, control| {
        control(SourceEvent::Work)?;
        for predicate in predicates {
            for _ in 0..predicate.comparison_work_units() {
                control(SourceEvent::Work)?;
            }
        }
        if predicates
            .iter()
            .all(|p| p.matches(&row.labels, &row.props))
        {
            snapshot.adjacency_index.visit(
                &snapshot.blocks,
                row.vid,
                lookup_direction,
                as_of,
                control,
                |entry, block, row, control| {
                    if entry.relation == *relation && !selected.contains_key(&entry.eid) {
                        control(SourceEvent::SnapshotRecord)?;
                        control(SourceEvent::ScratchEntry)?;
                        selected.insert(
                            entry.eid,
                            (
                                (entry.eid, entry.src, entry.relation, entry.dst),
                                edge_properties_at(&snapshot.block_props, block, row),
                            ),
                        );
                        for endpoint in [entry.src, entry.dst] {
                            if !frontier.contains(&endpoint) {
                                control(SourceEvent::ScratchEntry)?;
                                frontier.insert(endpoint);
                            }
                        }
                    }
                    Ok(())
                },
            )?;
        }
        Ok(())
    })?;
    // Fixed-hop plans consume at most one new adjacency per Expand. Using
    // both endpoints and both directions is a superset even for correlations
    // and cycle closures; no source-level join can discard a valid witness.
    let mut visited = std::collections::BTreeSet::new();
    for _ in 0..hops {
        let current = std::mem::take(&mut frontier);
        for endpoint in current {
            control(SourceEvent::Work)?;
            if visited.contains(&endpoint) {
                continue;
            }
            control(SourceEvent::ScratchEntry)?;
            visited.insert(endpoint);
            snapshot.adjacency_index.visit(
                &snapshot.blocks,
                endpoint,
                GlaDirection::Undirected,
                as_of,
                control,
                |entry, block, row, control| {
                    if let std::collections::btree_map::Entry::Vacant(slot) =
                        selected.entry(entry.eid)
                    {
                        control(SourceEvent::SnapshotRecord)?;
                        control(SourceEvent::ScratchEntry)?;
                        slot.insert((
                            (entry.eid, entry.src, entry.relation, entry.dst),
                            edge_properties_at(&snapshot.block_props, block, row),
                        ));
                        for endpoint in [entry.src, entry.dst] {
                            if !frontier.contains(&endpoint) {
                                control(SourceEvent::ScratchEntry)?;
                                frontier.insert(endpoint);
                            }
                        }
                    }
                    Ok(())
                },
            )?;
        }
    }
    Ok(Some(selected.into_values().collect()))
}

/// Source selection is shared by scalar and tuple plans. Output columns do
/// not change what snapshot generation or topology is admitted.
pub(super) fn admit<'a, E, Row>(
    snapshot: &'a Snapshot,
    logical: &fgdb_gql::algebra::GlaPlan<Row>,
    as_of: CommitSeq,
    control: &mut impl FnMut(SourceEvent) -> Result<(), E>,
) -> Result<BorrowedTables<'a>, E> {
    use fgdb_gql::algebra::GlaOperator;
    if !logical.scans_edges() {
        // Equality-bound vertex scans serve from the per-generation index;
        // every other node shape keeps the O(|V|) scan verbatim.
        let served = if PROPERTY_INDEX_SERVING {
            bound_vertices(snapshot, logical, as_of, control)?
        } else {
            None
        };
        let (vertices, vertex_records) = match served {
            Some(admitted) => admitted,
            None => {
                let vertices = scan_vertices(&snapshot.patches, as_of, control)?;
                let count = vertices.len() as u64;
                (vertices, count)
            }
        };
        // A node-root semijoin needs both base tables. Keep isolated outer
        // vertices, admit topology once, and charge every base record to the
        // same allowance. Probe execution never rereads either source table.
        let edges = if logical.reads_edges() {
            scan_edges(&snapshot.blocks, &snapshot.block_props, as_of, control)?
        } else {
            Vec::new()
        };
        return Ok(BorrowedTables {
            snapshot_records: vertex_records + edges.len() as u64,
            vertices,
            edges,
        });
    }
    let edges = match bound_edges(snapshot, logical, as_of, control)? {
        Some(edges) => edges,
        None => scan_edges(&snapshot.blocks, &snapshot.block_props, as_of, control)?,
    };
    let mut vertices = Vec::new();
    let mut vertex_records = 0;
    // A nested vertex scan needs the full domain, including isolated vertices.
    // Endpoint hydration alone is insufficient even for identity-only output.
    if logical
        .operators()
        .iter()
        .any(|operator| matches!(operator, GlaOperator::ScanVertices))
    {
        vertices = scan_vertices(&snapshot.patches, as_of, control)?;
        vertex_records = vertices.len() as u64;
    } else if logical.needs_vertex_values() {
        // Projection-only properties need admitted rows even with no WHERE.
        let mut candidates = std::collections::BTreeSet::new();
        for &((_, src, relation, dst), _) in &edges {
            control(SourceEvent::Work)?;
            let requested = logical.operators().iter().any(|operator| match operator {
                GlaOperator::ScanEdges {
                    relation: required, ..
                }
                | GlaOperator::Expand {
                    relation: required, ..
                }
                | GlaOperator::VarLengthExpand {
                    relation: required, ..
                } => *required == relation,
                _ => false,
            });
            if !requested {
                continue;
            }
            for vid in [src, dst] {
                if !candidates.contains(&vid) {
                    control(SourceEvent::ScratchEntry)?;
                    candidates.insert(vid);
                }
            }
        }
        for vid in candidates {
            if let Some(row) = find_vertex(&snapshot.patches, vid, as_of, control)? {
                control(SourceEvent::ScratchEntry)?;
                vertices.push(row);
            }
        }
    }
    Ok(BorrowedTables {
        snapshot_records: vertex_records + edges.len() as u64,
        vertices,
        edges,
    })
}
impl BorrowedTables<'_> {
    pub(super) fn edge_property(&self, eid: EId, key: PropertyKeyId) -> Option<&CanonicalScalar> {
        let at = self
            .edges
            .binary_search_by_key(&eid, |(edge, _)| edge.0)
            .ok()?;
        let props = self.edges[at].1;
        props
            .binary_search_by_key(&key, |(key, _)| *key)
            .ok()
            .map(|at| &props[at].1)
    }
    pub(super) fn property(&self, vid: VId, key: PropertyKeyId) -> Option<&CanonicalScalar> {
        let row = self.vertices[self
            .vertices
            .binary_search_by_key(&vid, |row| row.vid)
            .ok()?];
        row.props
            .binary_search_by_key(&key, |(key, _)| *key)
            .ok()
            .map(|at| &row.props[at].1)
    }
    pub(super) fn matches(
        &self,
        vid: VId,
        predicates: &[fgdb_gql::algebra::VertexPredicate],
    ) -> bool {
        self.vertices
            .binary_search_by_key(&vid, |row| row.vid)
            .ok()
            .is_some_and(|at| {
                let row = self.vertices[at];
                predicates
                    .iter()
                    .all(|predicate| predicate.matches(&row.labels, &row.props))
            })
    }
}

/// Execute a bound `FOR SYSTEM_TIME AS OF SEQ ...` query through the same
/// historical snapshot admission path as the explicit `_at` API. The temporal
/// text layer selects only the sequence; it does not create another reader,
/// authorization boundary, budget meter or result contract.
impl<V: asupersync::fs::Vfs + Clone> crate::Database<V> {
    pub fn execute_temporal_graph_text_governed(
        &self,
        cx: &fgdb_types::QueryCx,
        query: &fgdb_gql::BoundTemporalGraphQuery,
        policy: fgdb_gql::GqlQueryPolicy,
    ) -> Result<
        fgdb_gql::GqlQueryExecution<fgdb_gql::algebra::GraphValueRow>,
        fgdb_gql::GqlQueryError<crate::GqlError, Box<asupersync::error::Error>>,
    > {
        self.execute_graph_pattern_governed_at(cx, query.pattern(), query.as_of(), policy)
    }
}

impl crate::EmbeddedReadView {
    pub fn execute_temporal_graph_text_governed(
        &self,
        cx: &fgdb_types::QueryCx,
        query: &fgdb_gql::BoundTemporalGraphQuery,
        policy: fgdb_gql::GqlQueryPolicy,
    ) -> Result<
        fgdb_gql::GqlQueryExecution<fgdb_gql::algebra::GraphValueRow>,
        fgdb_gql::GqlQueryError<crate::GqlError, Box<asupersync::error::Error>>,
    > {
        self.execute_graph_pattern_governed_at(cx, query.pattern(), query.as_of(), policy)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fgdb_delta_types::{LabelId, PropertyKeyId};
    use fgdb_types::CanonicalScalar;

    #[test]
    fn nested_scans_and_scopes_never_use_a_root_only_edge_closure() {
        use fgdb_gql::algebra::{GlaDirection, GlaOperator};

        for operator in [
            GlaOperator::ScanVertices,
            GlaOperator::ScanEdges {
                relation: RelationId(2),
                direction: GlaDirection::Forward,
            },
            GlaOperator::Probe {
                group: 1,
                end: 3,
                anti: false,
            },
            GlaOperator::Probe {
                group: 1,
                end: 3,
                anti: true,
            },
            GlaOperator::ProbeEnd { group: 1 },
            GlaOperator::Optional {
                group: 1,
                end: 3,
                slots: 2,
            },
            GlaOperator::OptionalEnd { group: 1 },
        ] {
            assert_eq!(bound_edge_hops(std::slice::from_ref(&operator)), None);
            assert_eq!(
                bound_edge_hops(&[
                    GlaOperator::Distinct,
                    operator,
                    GlaOperator::Limit {
                        offset: 0,
                        count: Some(1),
                    },
                ]),
                None
            );
        }
    }

    #[test]
    fn terminal_projection_operations_preserve_the_one_hop_fast_path() {
        use fgdb_gql::algebra::GlaOperator;

        assert_eq!(bound_edge_hops(&[]), Some(0));
        assert_eq!(
            bound_edge_hops(&[
                GlaOperator::Distinct,
                GlaOperator::OrderByVertexId,
                GlaOperator::Limit {
                    offset: 2,
                    count: Some(3),
                },
            ]),
            Some(0)
        );
    }

    fn row(id: u128, created: u64, retired: Option<u64>, value: i64) -> VertexRow {
        VertexRow {
            vid: VId(id),
            birth_ordinal: id as u64,
            created_at: CommitSeq(created),
            retired_at: retired.map(CommitSeq),
            labels: vec![LabelId(1)],
            props: vec![(PropertyKeyId(1), CanonicalScalar::Int(value))],
        }
    }
    fn patch(rows: &[VertexRow]) -> VertexPatchRows {
        let bytes = fgdb_strata::vertex::encode_patch(rows).unwrap();
        fgdb_strata::vertex::decode_patch(&bytes).unwrap()
    }
    #[test]
    fn borrowed_vertex_scan_and_lookup_match_the_independent_storage_merge() {
        let high = (1_u128 << 100) + 3;
        let patches = vec![
            patch(&[row(1, 1, None, 1), row(high, 1, None, 8)]),
            patch(&[row(1, 1, Some(2), 1), row(1, 2, None, 2)]),
            patch(&[row(1, 2, Some(4), 2), row(2, 3, None, 3)]),
            patch(&[row(high, 1, Some(5), 8)]),
        ];
        for at in 0..=6 {
            let expected = fgdb_strata::vertex::merge_all_vertices(&patches, CommitSeq(at));
            let actual = scan_vertices(&patches, CommitSeq(at), &mut |_| Ok::<_, ()>(())).unwrap();
            assert_eq!(actual, expected.iter().collect::<Vec<_>>());
            for vid in [VId(1), VId(2), VId(99), VId(high)] {
                let actual =
                    find_vertex(&patches, vid, CommitSeq(at), &mut |_| Ok::<_, ()>(())).unwrap();
                let expected = fgdb_strata::vertex::merge_vertex(&patches, vid, CommitSeq(at));
                assert_eq!(actual, expected.as_ref());
            }
            assert!(actual.iter().all(|found| {
                patches
                    .iter()
                    .any(|patch| patch.iter().any(|original| std::ptr::eq(*found, original)))
            }));
        }
    }
    fn edge(id: u128, created: u64, retired: Option<u64>) -> AdjacencyEntry {
        AdjacencyEntry {
            src: VId(1),
            relation: RelationId(1),
            dst: VId(2),
            eid: EId(id),
            created_at: CommitSeq(created),
            retired_at: retired.map(CommitSeq),
        }
    }
    #[test]
    fn edge_candidates_preserve_parallel_ids_and_retirements_without_properties() {
        let blocks = vec![
            vec![edge(1, 1, None), edge(2, 1, None)],
            vec![edge(1, 1, Some(2)), edge(1, 2, None)],
            vec![edge(1, 2, Some(3))],
        ];
        let props = vec![None; blocks.len()];
        for at in 0..=4 {
            let expected =
                fgdb_strata::root::merge_all_edges_with_props(&blocks, &props, CommitSeq(at))
                    .unwrap()
                    .into_iter()
                    .map(|(entry, _)| (entry.eid, entry.src, entry.relation, entry.dst))
                    .collect::<Vec<_>>();
            let actual =
                scan_edges(&blocks, &props, CommitSeq(at), &mut |_| Ok::<_, ()>(())).unwrap();
            assert_eq!(
                actual.into_iter().map(|(edge, _)| edge).collect::<Vec<_>>(),
                expected
            );
        }
        let mut allocations = 0;
        scan_edges(&blocks, &props, CommitSeq(3), &mut |event| {
            allocations += usize::from(event == SourceEvent::ScratchEntry);
            Ok::<_, ()>(())
        })
        .unwrap();
        assert_eq!(
            allocations, 3,
            "two EId candidates plus one visible triple, not one per version"
        );
    }
    #[test]
    fn every_scan_checkpoint_can_refuse_without_returning_partial_rows() {
        let patches = vec![patch(&[row(1, 1, None, 1), row(2, 1, None, 2)])];
        let blocks = vec![vec![edge(1, 1, None), edge(2, 1, None)]];
        for vertices in [false, true] {
            let run = |stop: usize| {
                let mut events = 0;
                let mut control = |_| {
                    events += 1;
                    if events == stop { Err(stop) } else { Ok(()) }
                };
                let result = if vertices {
                    scan_vertices(&patches, CommitSeq(1), &mut control).map(|rows| rows.len())
                } else {
                    scan_edges(&blocks, &[], CommitSeq(1), &mut control).map(|rows| rows.len())
                };
                (result, events)
            };
            let (success, total) = run(usize::MAX);
            assert_eq!(success, Ok(2));
            for stop in 1..=total {
                assert_eq!(run(stop), (Err(stop), stop));
            }
        }
    }
}
