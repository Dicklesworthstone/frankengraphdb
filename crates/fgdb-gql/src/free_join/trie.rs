use super::{JoinPlanError, JoinVariable, MAX_JOIN_VARIABLES};
use crate::GlaExecutionEvent;
use core::cmp::Ordering;
use std::collections::BTreeSet;

#[derive(Debug, PartialEq, Eq)]
pub enum TrieBuildError<E> {
    Control(E),
    Schema(JoinPlanError),
    RowArity {
        row: usize,
        expected: usize,
        actual: usize,
    },
    ZeroMultiplicity {
        row: usize,
    },
    MultiplicityOverflow,
}
impl<E: core::fmt::Display> core::fmt::Display for TrieBuildError<E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Control(error) => error.fmt(f),
            Self::Schema(error) => error.fmt(f),
            Self::RowArity {
                row,
                expected,
                actual,
            } => {
                write!(
                    f,
                    "FreeJoin row {row} has {actual} columns, expected {expected}"
                )
            }
            Self::ZeroMultiplicity { row } => {
                write!(f, "FreeJoin row {row} has zero multiplicity")
            }
            Self::MultiplicityOverflow => f.write_str("FreeJoin input multiplicity exceeds usize"),
        }
    }
}
impl<E: core::error::Error + 'static> core::error::Error for TrieBuildError<E> {}

/// A cursor is a borrowed, immutable-generation prefix plus a monotone key
/// position. Cloning copies ONLY navigation state, never relation rows.
///
/// Keys in one level are strictly increasing. `seek` never moves backwards;
/// `open` returns a child prefix without changing its parent (discarding the
/// child is `up`). A leaf has a positive multiplicity, except an empty nullary
/// relation, whose multiplicity is zero. `distinct_prefixes(n)` counts distinct
/// n-attribute extensions of this prefix, not bag occurrences or suffix rows.
/// Implementations must bound every operation and honor the control callback.
/// A Strata adapter may implement this contract only for its advertised order.
pub trait TrieCursor<K: Ord + Clone>: Clone {
    fn remaining_order(&self) -> &[JoinVariable];
    fn generation(&self) -> u64;
    fn key(&self) -> Option<&K>;
    fn multiplicity(&self) -> Option<usize>;
    fn distinct_prefixes(&self, attributes: usize) -> usize;
    fn advance<E>(
        &mut self,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<(), E>;
    fn seek<E>(
        &mut self,
        target: &K,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<(), E>;
    fn open<E>(
        &self,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Option<Self>, E>;
}

pub trait TrieRelation<K: Ord + Clone> {
    type Cursor<'a>: TrieCursor<K>
    where
        Self: 'a,
        K: 'a;

    fn attribute_order(&self) -> &[JoinVariable];
    fn cursor(&self) -> Self::Cursor<'_>;
}

#[derive(Clone, Debug)]
struct Column<K> {
    keys: Vec<K>,
    // One child start per key plus a terminal sentinel. Last-level keys use
    // weights instead; no per-tuple tree node or boxed child is allocated.
    child_starts: Vec<usize>,
}

/// A compact ordered columnar trie for an unavailable physical attribute order.
/// Construction is explicit and fully charged; it is NOT a zero-copy run view.
/// Duplicate full tuples share one leaf and retain their exact occurrence count.
#[derive(Clone)]
pub struct ColumnarTrie<K> {
    order: Vec<JoinVariable>,
    columns: Vec<Column<K>>,
    weights: Vec<usize>,
    nullary_weight: usize,
    generation: u64,
}

impl<K> core::fmt::Debug for ColumnarTrie<K> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ColumnarTrie")
            .field("order", &self.order)
            .field("generation", &self.generation)
            .field("keys", &"[REDACTED]")
            .finish()
    }
}

impl<K: Ord + Clone> ColumnarTrie<K> {
    pub fn new<E>(
        schema: &[JoinVariable],
        order: &[JoinVariable],
        rows: impl IntoIterator<Item = Vec<K>>,
        generation: u64,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Self, TrieBuildError<E>> {
        Self::new_weighted(
            schema,
            order,
            rows.into_iter().map(|values| (values, 1)),
            generation,
            control,
        )
    }

    /// Build directly from positive occurrence counts without expanding them.
    /// Equal tuples share a leaf whose weight is added with checked arithmetic.
    /// Input weights obey the existing `TrieCursor` usize contract; joined
    /// products and factorized cardinalities retain their checked u128 boundary.
    /// Construction work depends on supplied tuples, not their multiplicities.
    pub fn new_weighted<E>(
        schema: &[JoinVariable],
        order: &[JoinVariable],
        rows: impl IntoIterator<Item = (Vec<K>, usize)>,
        generation: u64,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Self, TrieBuildError<E>> {
        if schema.len() > MAX_JOIN_VARIABLES || order.len() > MAX_JOIN_VARIABLES {
            return Err(TrieBuildError::Schema(JoinPlanError::TooManyVariables));
        }
        let mut declared = BTreeSet::new();
        for &variable in schema {
            if !declared.insert(variable) {
                return Err(TrieBuildError::Schema(JoinPlanError::DuplicateVariable(
                    variable,
                )));
            }
        }
        let mut ordered = BTreeSet::new();
        let mut permutation = Vec::new();
        for &variable in order {
            if !declared.contains(&variable) {
                return Err(TrieBuildError::Schema(JoinPlanError::UnknownVariable(
                    variable,
                )));
            }
            if !ordered.insert(variable) {
                return Err(TrieBuildError::Schema(JoinPlanError::DuplicateVariable(
                    variable,
                )));
            }
            permutation.push(
                schema
                    .iter()
                    .position(|v| *v == variable)
                    .expect("declared variable"),
            );
        }
        if let Some(&missing) = declared.difference(&ordered).next() {
            return Err(TrieBuildError::Schema(JoinPlanError::MissingVariable(
                missing,
            )));
        }
        let mut meter = |event| control(event).map_err(TrieBuildError::Control);
        for _ in 0..schema.len() {
            meter(GlaExecutionEvent::ScratchEntry)?;
        }
        let mut admitted = Vec::new();
        for (row, (values, multiplicity)) in rows.into_iter().enumerate() {
            meter(GlaExecutionEvent::Work)?;
            if values.len() != schema.len() {
                return Err(TrieBuildError::RowArity {
                    row,
                    expected: schema.len(),
                    actual: values.len(),
                });
            }
            if multiplicity == 0 {
                return Err(TrieBuildError::ZeroMultiplicity { row });
            }
            meter(GlaExecutionEvent::ScratchEntry)?;
            for _ in &values {
                meter(GlaExecutionEvent::ScratchEntry)?;
            }
            admitted.push((values, multiplicity));
        }
        if !order.is_empty() {
            sort_rows(&mut admitted, &permutation, &mut meter)?;
        }
        let mut columns: Vec<_> = order
            .iter()
            .map(|_| Column {
                keys: Vec::new(),
                child_starts: Vec::new(),
            })
            .collect();
        let mut weights: Vec<usize> = Vec::new();
        let mut nullary_weight = 0_usize;
        for (row_index, (row, multiplicity)) in admitted.iter().enumerate() {
            meter(GlaExecutionEvent::Work)?;
            let mut common = 0;
            if row_index != 0 {
                while common < order.len() {
                    meter(GlaExecutionEvent::Work)?;
                    if row[permutation[common]] != admitted[row_index - 1].0[permutation[common]] {
                        break;
                    }
                    common += 1;
                }
            }
            if common == order.len() {
                if order.is_empty() {
                    nullary_weight = nullary_weight
                        .checked_add(*multiplicity)
                        .ok_or(TrieBuildError::MultiplicityOverflow)?;
                } else {
                    let weight = weights.last_mut().expect("a duplicate has an earlier leaf");
                    *weight = weight
                        .checked_add(*multiplicity)
                        .ok_or(TrieBuildError::MultiplicityOverflow)?;
                }
                continue;
            }
            for depth in common..order.len() {
                meter(GlaExecutionEvent::ScratchEntry)?;
                columns[depth].keys.push(row[permutation[depth]].clone());
                meter(GlaExecutionEvent::ScratchEntry)?;
                if depth + 1 == order.len() {
                    weights.push(*multiplicity);
                } else {
                    let start = columns[depth + 1].keys.len();
                    columns[depth].child_starts.push(start);
                }
            }
        }
        for depth in 0..order.len().saturating_sub(1) {
            meter(GlaExecutionEvent::ScratchEntry)?;
            let end = columns[depth + 1].keys.len();
            columns[depth].child_starts.push(end);
        }
        Ok(Self {
            order: order.to_vec(),
            columns,
            weights,
            nullary_weight,
            generation,
        })
    }

    /// Number of retained distinct prefix keys, excluding metadata and weights.
    #[must_use]
    pub fn stored_keys(&self) -> usize {
        self.columns.iter().map(|column| column.keys.len()).sum()
    }
}

fn compare_rows<K: Ord, E>(
    left: &[K],
    right: &[K],
    order: &[usize],
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<Ordering, E> {
    for &column in order {
        control(GlaExecutionEvent::Work)?;
        let comparison = left[column].cmp(&right[column]);
        if comparison != Ordering::Equal {
            return Ok(comparison);
        }
    }
    Ok(Ordering::Equal)
}

fn sort_rows<K: Ord, E>(
    rows: &mut [(Vec<K>, usize)],
    order: &[usize],
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<(), E> {
    fn sift<K: Ord, E>(
        rows: &mut [(Vec<K>, usize)],
        order: &[usize],
        mut root: usize,
        end: usize,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<(), E> {
        while root < end / 2 {
            let mut child = root * 2 + 1;
            if child + 1 < end
                && compare_rows(&rows[child].0, &rows[child + 1].0, order, control)?
                    == Ordering::Less
            {
                child += 1;
            }
            if compare_rows(&rows[root].0, &rows[child].0, order, control)? != Ordering::Less {
                break;
            }
            control(GlaExecutionEvent::Work)?;
            rows.swap(root, child);
            root = child;
        }
        Ok(())
    }
    let len = rows.len();
    for root in (0..len / 2).rev() {
        sift(rows, order, root, len, control)?;
    }
    for end in (1..len).rev() {
        control(GlaExecutionEvent::Work)?;
        rows.swap(0, end);
        sift(rows, order, 0, end, control)?;
    }
    Ok(())
}

/// Navigation state borrows one immutable trie generation. Parents remain
/// usable after a child is exhausted; there is no mutable global cursor stack.
#[derive(Clone)]
pub struct ColumnarCursor<'a, K> {
    trie: &'a ColumnarTrie<K>,
    depth: usize,
    low: usize,
    high: usize,
    position: usize,
    leaf_weight: usize,
}

impl<K> core::fmt::Debug for ColumnarCursor<'_, K> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ColumnarCursor")
            .field("depth", &self.depth)
            .field("generation", &self.trie.generation)
            .field("keys", &"[REDACTED]")
            .finish()
    }
}

impl<K: Ord + Clone> TrieRelation<K> for ColumnarTrie<K> {
    type Cursor<'a>
        = ColumnarCursor<'a, K>
    where
        K: 'a;

    fn attribute_order(&self) -> &[JoinVariable] {
        &self.order
    }

    fn cursor(&self) -> Self::Cursor<'_> {
        ColumnarCursor {
            trie: self,
            depth: 0,
            low: 0,
            high: self.columns.first().map_or(0, |column| column.keys.len()),
            position: 0,
            leaf_weight: self.nullary_weight,
        }
    }
}

impl<K: Ord + Clone> TrieCursor<K> for ColumnarCursor<'_, K> {
    fn remaining_order(&self) -> &[JoinVariable] {
        &self.trie.order[self.depth..]
    }

    fn generation(&self) -> u64 {
        self.trie.generation
    }

    fn key(&self) -> Option<&K> {
        (self.position < self.high).then(|| &self.trie.columns[self.depth].keys[self.position])
    }

    fn multiplicity(&self) -> Option<usize> {
        (self.depth == self.trie.order.len()).then_some(self.leaf_weight)
    }

    fn distinct_prefixes(&self, attributes: usize) -> usize {
        if attributes == 0 {
            return usize::from(self.multiplicity().is_none_or(|weight| weight != 0));
        }
        if attributes > self.remaining_order().len() {
            return 0;
        }
        let (mut low, mut high) = (self.low, self.high);
        for depth in self.depth..self.depth + attributes - 1 {
            let starts = &self.trie.columns[depth].child_starts;
            low = starts[low];
            high = starts[high];
        }
        high - low
    }

    fn advance<E>(
        &mut self,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<(), E> {
        control(GlaExecutionEvent::Work)?;
        if self.position < self.high {
            self.position += 1;
        }
        Ok(())
    }

    fn seek<E>(
        &mut self,
        target: &K,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<(), E> {
        if self.position >= self.high {
            return Ok(());
        }
        let keys = &self.trie.columns[self.depth].keys;
        control(GlaExecutionEvent::Work)?;
        if keys[self.position] >= *target {
            return Ok(());
        }
        let start = self.position;
        let mut low = start + 1;
        let mut stride = 1_usize;
        let mut high;
        loop {
            let probe = start.saturating_add(stride);
            if probe >= self.high {
                high = self.high;
                break;
            }
            control(GlaExecutionEvent::Work)?;
            if keys[probe] >= *target {
                high = probe;
                break;
            }
            low = probe + 1;
            stride = stride.saturating_mul(2);
        }
        while low < high {
            let middle = low + (high - low) / 2;
            control(GlaExecutionEvent::Work)?;
            if keys[middle] < *target {
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
        if self.position >= self.high {
            return Ok(None);
        }
        let depth = self.depth + 1;
        if depth == self.trie.order.len() {
            return Ok(Some(Self {
                trie: self.trie,
                depth,
                low: 0,
                high: 0,
                position: 0,
                leaf_weight: self.trie.weights[self.position],
            }));
        }
        let starts = &self.trie.columns[self.depth].child_starts;
        let low = starts[self.position];
        Ok(Some(Self {
            trie: self.trie,
            depth,
            low,
            high: starts[self.position + 1],
            position: low,
            leaf_weight: 0,
        }))
    }
}
