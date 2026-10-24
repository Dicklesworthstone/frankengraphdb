//! Ordered factorization of the SAME covered-group join program.
//!
//! Removing a bound prefix may disconnect the remaining query hypergraph.
//! Contiguous independent components become Product nodes and are evaluated
//! once, not once per sibling occurrence. Interleaved components deliberately
//! stay together: physical variable order is not silently changed by factoring.
//! Node IDs are private and only refer backwards, so cycles and dangling IDs
//! cannot enter an executable batch. This is an owned Flat/Product/Union subset;
//! it does not manufacture RunSlice leases, path DAGs, or spill capabilities.

use super::execute::{GroupScan, driver, probe};
use super::{
    FreeJoin, FreeJoinCursor, FreeJoinPlan, JoinVariable, MultiplicityOverflow, TrieCursor,
    TrieRelation,
};
use crate::GlaExecutionEvent;
use core::num::NonZeroUsize;
use core::ops::Range;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FactorNodeKind {
    Empty,
    Unit,
    Flat,
    Product,
    Union,
}

#[derive(Clone)]
enum Value<K> {
    Empty,
    Unit,
    Flat {
        start: usize,
        values: Vec<K>,
    },
    Product {
        children: Vec<usize>,
        suffixes: Vec<Option<u128>>,
    },
    Union {
        children: Vec<usize>,
        cumulative: Vec<Option<u128>>,
    },
}

#[derive(Clone)]
struct Node<K> {
    value: Value<K>,
    // None is explicit arithmetic overflow, never a saturated cardinality.
    count: Option<u128>,
}

/// An immutable owned factorized relation with the physical plan's columns.
/// Flattening follows deterministic factor-tree order, not an implicit ORDER BY.
/// In particular, repeated equal prefixes do not establish a sorted row stream.
/// Equal occurrences are retained through Unit weights, never deduplicated.
/// Values and physical generation tags are owned; no source cursor escapes.
#[derive(Clone)]
pub struct FactorizedBatch<K> {
    variables: Vec<JoinVariable>,
    generations: Vec<u64>,
    nodes: Vec<Node<K>>,
    root: usize,
}

impl<K> core::fmt::Debug for FactorizedBatch<K> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("FactorizedBatch")
            .field("variables", &self.variables)
            .field("nodes", &self.nodes.len())
            .field("values", &"[REDACTED]")
            .finish()
    }
}

impl<K> FactorizedBatch<K> {
    pub fn cardinality(&self) -> Result<u128, MultiplicityOverflow> {
        self.nodes[self.root].count.ok_or(MultiplicityOverflow)
    }

    #[must_use]
    pub fn variables(&self) -> &[JoinVariable] {
        &self.variables
    }

    #[must_use]
    pub fn generations(&self) -> &[u64] {
        &self.generations
    }

    #[must_use]
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    #[must_use]
    pub fn stored_values(&self) -> usize {
        self.nodes
            .iter()
            .map(|node| match &node.value {
                Value::Flat { values, .. } => values.len(),
                _ => 0,
            })
            .sum()
    }

    pub fn node_kinds(&self) -> impl Iterator<Item = FactorNodeKind> + '_ {
        self.nodes.iter().map(|node| match &node.value {
            Value::Empty => FactorNodeKind::Empty,
            Value::Unit => FactorNodeKind::Unit,
            Value::Flat { .. } => FactorNodeKind::Flat,
            Value::Product { .. } => FactorNodeKind::Product,
            Value::Union { .. } => FactorNodeKind::Union,
        })
    }

    pub fn cursor(&self) -> Result<FactorizedCursor<'_, K>, MultiplicityOverflow> {
        let cardinality = self.cardinality()?;
        Ok(FactorizedCursor {
            batch: self,
            position: 0,
            cardinality,
            failed: false,
        })
    }

    fn push<E>(
        &mut self,
        value: Value<K>,
        count: Option<u128>,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<usize, E> {
        control(GlaExecutionEvent::ScratchEntry)?;
        let id = self.nodes.len();
        self.nodes.push(Node { value, count });
        Ok(id)
    }

    fn product<E>(
        &mut self,
        children: Vec<usize>,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<usize, E> {
        let mut suffix = Some(1_u128);
        let mut suffixes = Vec::new();
        for &child in children.iter().rev() {
            control(GlaExecutionEvent::Work)?;
            if self.nodes[child].count == Some(0) {
                return Ok(0);
            }
            control(GlaExecutionEvent::ScratchEntry)?;
            suffixes.push(suffix);
            suffix = suffix
                .zip(self.nodes[child].count)
                .and_then(|(a, b)| a.checked_mul(b));
        }
        suffixes.reverse();
        if children.is_empty() {
            return self.push(Value::Unit, Some(1), control);
        }
        if children.len() == 1 {
            return Ok(children[0]);
        }
        self.push(Value::Product { children, suffixes }, suffix, control)
    }

    fn union<E>(
        &mut self,
        children: Vec<usize>,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<usize, E> {
        if children.is_empty() {
            return Ok(0);
        }
        if children.len() == 1 {
            return Ok(children[0]);
        }
        let mut count = Some(0_u128);
        let mut cumulative = Vec::new();
        for &child in &children {
            control(GlaExecutionEvent::ScratchEntry)?;
            count = count
                .zip(self.nodes[child].count)
                .and_then(|(a, b)| a.checked_add(b));
            cumulative.push(count);
        }
        self.push(
            Value::Union {
                children,
                cumulative,
            },
            count,
            control,
        )
    }
}

struct StageTree {
    at: usize,
    children: Vec<StageTree>,
    closes: Vec<usize>,
}

// A relation spanning a proposed cut joins the components. Already-bound
// attributes do not span it. Cuts retain the original order even when abstract
// connected components interleave; merging their intervals is conservative.
fn forest(plan: &FreeJoinPlan, range: Range<usize>, last: &[Option<usize>]) -> Vec<StageTree> {
    let mut result = Vec::new();
    let mut start = range.start;
    while start < range.end {
        let mut end = start + 1;
        let mut scan = start;
        while scan < end {
            for access in &plan.stages[scan].probes {
                end = end.max(last[access.relation].expect("a probe has a last stage") + 1);
            }
            scan += 1;
        }
        debug_assert!(end <= range.end, "a component cannot escape its parent");
        let closes = plan.stages[start]
            .probes
            .iter()
            .filter_map(|access| (last[access.relation] == Some(start)).then_some(access.relation))
            .collect();
        result.push(StageTree {
            at: start,
            children: forest(plan, start + 1..end, last),
            closes,
        });
        start = end;
    }
    result
}

impl<K: Ord + Clone, T: TrieRelation<K>> FreeJoin<'_, K, T> {
    /// Evaluate independent suffix components into an owned factorized value.
    /// This uses the same trie driver/probe operators as for_each_binding.
    /// No flat join results are collected and then retroactively compressed.
    pub fn factorize<E>(
        &self,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<FactorizedBatch<K>, E> {
        // Pure plan metadata is bounded by MAX_JOIN_*; charge its storage.
        for _ in &self.plan.stages {
            control(GlaExecutionEvent::ScratchEntry)?;
        }
        let mut last = vec![None; self.plan.relation_count()];
        for (at, stage) in self.plan.stages.iter().enumerate() {
            for access in &stage.probes {
                control(GlaExecutionEvent::Work)?;
                last[access.relation] = Some(at);
            }
        }
        let program = forest(self.plan, 0..self.plan.stages.len(), &last);
        let mut batch = FactorizedBatch {
            variables: Vec::new(),
            generations: Vec::new(),
            nodes: Vec::new(),
            root: 0,
        };
        for &variable in &self.plan.order {
            control(GlaExecutionEvent::ScratchEntry)?;
            batch.variables.push(variable);
        }
        batch.push(Value::Empty, Some(0), control)?;
        let mut cursors = Vec::new();
        let mut roots = Vec::new();
        let mut empty = false;
        for relation in self.relations {
            control(GlaExecutionEvent::ScratchEntry)?;
            let cursor = relation.cursor();
            control(GlaExecutionEvent::ScratchEntry)?;
            batch.generations.push(cursor.generation());
            if let Some(count) = cursor.multiplicity() {
                empty |= count == 0;
                let node = batch.push(Value::Unit, Some(count as u128), control)?;
                control(GlaExecutionEvent::ScratchEntry)?;
                roots.push(node);
            } else {
                empty |= cursor.distinct_prefixes(1) == 0;
            }
            cursors.push(cursor);
        }
        if empty {
            return Ok(batch);
        }
        let root = evaluate_forest(self.plan, &program, &mut cursors, &mut batch, control)?;
        control(GlaExecutionEvent::ScratchEntry)?;
        roots.push(root);
        batch.root = batch.product(roots, control)?;
        Ok(batch)
    }
}

fn evaluate_forest<K: Ord + Clone, T: TrieCursor<K>, E>(
    plan: &FreeJoinPlan,
    program: &[StageTree],
    cursors: &mut [T],
    batch: &mut FactorizedBatch<K>,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<usize, E> {
    let mut children = Vec::new();
    for stage in program {
        let child = evaluate_stage(plan, stage, cursors, batch, control)?;
        if child == 0 {
            return Ok(0);
        }
        control(GlaExecutionEvent::ScratchEntry)?;
        children.push(child);
    }
    batch.product(children, control)
}

fn evaluate_stage<K: Ord + Clone, T: TrieCursor<K>, E>(
    plan: &FreeJoinPlan,
    tree: &StageTree,
    cursors: &mut [T],
    batch: &mut FactorizedBatch<K>,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<usize, E> {
    let stage = &plan.stages[tree.at];
    let proposer = driver(stage, cursors, control)?;
    let mut candidates = GroupScan::new(cursors[proposer].clone(), stage.variables.len(), control)?;
    let mut alternatives = Vec::new();
    let mut saved = Vec::new();
    for _ in &stage.probes {
        control(GlaExecutionEvent::ScratchEntry)?;
    }
    while let Some(candidate) = candidates.next(control)? {
        let mut accepted = true;
        for access in &stage.probes {
            let original = cursors[access.relation].clone();
            let Some(child) = probe(original.clone(), &access.key_positions, candidate, control)?
            else {
                accepted = false;
                break;
            };
            saved.push((access.relation, original));
            cursors[access.relation] = child;
        }
        if accepted {
            let mut values = Vec::new();
            for key in candidate {
                control(GlaExecutionEvent::ScratchEntry)?;
                values.push(key.clone());
            }
            let flat = batch.push(
                Value::Flat {
                    start: stage.start,
                    values,
                },
                Some(1),
                control,
            )?;
            control(GlaExecutionEvent::ScratchEntry)?;
            let mut children = vec![flat];
            for &relation in &tree.closes {
                control(GlaExecutionEvent::Work)?;
                let count = cursors[relation]
                    .multiplicity()
                    .expect("last relation attribute is bound");
                let node = batch.push(Value::Unit, Some(count as u128), control)?;
                control(GlaExecutionEvent::ScratchEntry)?;
                children.push(node);
            }
            let tail = evaluate_forest(plan, &tree.children, cursors, batch, control)?;
            control(GlaExecutionEvent::ScratchEntry)?;
            children.push(tail);
            let product = batch.product(children, control)?;
            if product != 0 {
                control(GlaExecutionEvent::ScratchEntry)?;
                alternatives.push(product);
            }
        }
        for (relation, original) in saved.drain(..).rev() {
            cursors[relation] = original;
        }
    }
    batch.union(alternatives, control)
}

/// A column batch produced ONLY at an explicit flattening boundary.
/// Columns have equal lengths and retain every bag occurrence. Canonical
/// logical ORDER BY remains the surrounding GLA operator's responsibility.
pub struct FactorizedColumns<K> {
    variables: Vec<JoinVariable>,
    columns: Vec<Vec<K>>,
    rows: usize,
}
impl<K> FactorizedColumns<K> {
    #[must_use]
    pub fn variables(&self) -> &[JoinVariable] {
        &self.variables
    }
    #[must_use]
    pub fn columns(&self) -> &[Vec<K>] {
        &self.columns
    }
    #[must_use]
    pub fn row_count(&self) -> usize {
        self.rows
    }
}
impl<K> core::fmt::Debug for FactorizedColumns<K> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("FactorizedColumns")
            .field("rows", &self.rows)
            .field("values", &"[REDACTED]")
            .finish()
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum FactorizedReadError<E> {
    Control(E),
    Failed,
}
impl<E: core::fmt::Display> core::fmt::Display for FactorizedReadError<E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Control(error) => error.fmt(f),
            Self::Failed => f.write_str("factorized cursor cannot resume after refusal"),
        }
    }
}
impl<E: core::error::Error + 'static> core::error::Error for FactorizedReadError<E> {}

/// Rank-addressable flattening. Seeking skips products arithmetically rather
/// than enumerating their occurrences. A refused batch publishes nothing and
/// permanently poisons this cursor; callers may create a fresh cursor to replay.
pub struct FactorizedCursor<'a, K> {
    batch: &'a FactorizedBatch<K>,
    position: u128,
    cardinality: u128,
    failed: bool,
}
impl<K> FactorizedCursor<'_, K> {
    #[must_use]
    pub fn position(&self) -> u128 {
        self.position
    }
    #[must_use]
    pub fn is_failed(&self) -> bool {
        self.failed
    }

    /// Monotone positioning in the declared physical order. No values are read.
    pub fn seek(&mut self, rank: u128) {
        if !self.failed {
            self.position = self.position.max(rank.min(self.cardinality));
        }
    }
}
impl<K: Clone> FactorizedCursor<'_, K> {
    pub fn next_batch<E>(
        &mut self,
        max_rows: usize,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<FactorizedColumns<K>, FactorizedReadError<E>> {
        if self.failed {
            return Err(FactorizedReadError::Failed);
        }
        let result = self.read_batch(max_rows, control);
        if result.is_err() {
            self.failed = true;
        }
        result.map_err(FactorizedReadError::Control)
    }

    fn read_batch<E>(
        &mut self,
        max_rows: usize,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<FactorizedColumns<K>, E> {
        let count = (self.cardinality - self.position).min(max_rows as u128) as usize;
        let mut variables = Vec::new();
        let mut columns = Vec::new();
        for &variable in &self.batch.variables {
            control(GlaExecutionEvent::ScratchEntry)?;
            variables.push(variable);
            control(GlaExecutionEvent::ScratchEntry)?;
            columns.push(Vec::new());
        }
        for offset in 0..count {
            control(GlaExecutionEvent::Work)?;
            let mut row = Vec::new();
            for _ in &variables {
                control(GlaExecutionEvent::ScratchEntry)?;
                row.push(None);
            }
            fill(
                self.batch,
                self.batch.root,
                self.position + offset as u128,
                &mut row,
                control,
            )?;
            for (column, value) in columns.iter_mut().zip(row) {
                control(GlaExecutionEvent::ScratchEntry)?;
                column.push(value.expect("factor forest binds every variable exactly once"));
            }
        }
        self.position += count as u128;
        Ok(FactorizedColumns {
            variables,
            columns,
            rows: count,
        })
    }
}

fn fill<K: Clone, E>(
    batch: &FactorizedBatch<K>,
    id: usize,
    rank: u128,
    row: &mut [Option<K>],
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<(), E> {
    control(GlaExecutionEvent::Work)?;
    let node = &batch.nodes[id];
    debug_assert!(rank < node.count.expect("exact-cardinality cursor"));
    match &node.value {
        Value::Empty => unreachable!("an empty node has no rank"),
        Value::Unit => {}
        Value::Flat { start, values } => {
            for (offset, value) in values.iter().enumerate() {
                control(GlaExecutionEvent::ScratchEntry)?;
                row[start + offset] = Some(value.clone());
            }
        }
        Value::Product { children, suffixes } => {
            for (&child, &suffix) in children.iter().zip(suffixes) {
                control(GlaExecutionEvent::Work)?;
                let count = batch.nodes[child]
                    .count
                    .expect("exact nonempty product child");
                let digit = (rank / suffix.expect("exact product suffix")) % count;
                fill(batch, child, digit, row, control)?;
            }
        }
        Value::Union {
            children,
            cumulative,
        } => {
            let (mut low, mut high) = (0, children.len());
            while low < high {
                control(GlaExecutionEvent::Work)?;
                let middle = low + (high - low) / 2;
                if cumulative[middle].expect("exact union prefix") <= rank {
                    low = middle + 1;
                } else {
                    high = middle;
                }
            }
            let before = if low == 0 {
                0
            } else {
                cumulative[low - 1].expect("exact union prefix")
            };
            fill(batch, children[low], rank - before, row, control)?;
        }
    }
    Ok(())
}

/// Bounded columnar flattening directly from a live join producer. Unlike
/// [`FactorizedCursor`], this does not first build a complete factor tree or
/// require its total cardinality to fit in u128. It retains one binding and a
/// plan-sized mixed-radix occurrence counter between calls.
///
/// A successful batch owns at most the requested number of rows. A refusal
/// publishes none of that batch, releases the producer, and permanently
/// poisons this cursor; previously returned batches remain valid. These are
/// the existing logical GLA admissions, not allocator-byte or spill guarantees.
pub struct FreeJoinBatchCursor<'a, K: Ord + Clone + 'a, T: TrieRelation<K> + 'a> {
    input: FreeJoinCursor<'a, K, T>,
    variables: &'a [JoinVariable],
    positions: Vec<usize>,
    pending: bool,
    failed: bool,
}

impl<K: Ord + Clone, T: TrieRelation<K>> core::fmt::Debug for FreeJoinBatchCursor<'_, K, T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("FreeJoinBatchCursor")
            .field("pending", &self.pending)
            .field("failed", &self.failed)
            .field("values", &"[REDACTED]")
            .finish()
    }
}

impl<K: Ord + Clone, T: TrieRelation<K>> FreeJoin<'_, K, T> {
    /// Prepare a source-free columnar producer in this join's physical order.
    /// Each call to `next_batch` is an explicit bag-flattening boundary. SQL/GQL
    /// ordering, byte admission and publication guards remain the host's job.
    #[must_use]
    pub fn batch_cursor(&self) -> FreeJoinBatchCursor<'_, K, T> {
        FreeJoinBatchCursor {
            input: self.cursor(),
            variables: self.plan.variables(),
            positions: Vec::new(),
            pending: false,
            failed: false,
        }
    }
}

impl<K: Ord + Clone, T: TrieRelation<K>> FreeJoinBatchCursor<'_, K, T> {
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.input.is_finished() && !self.pending
    }

    #[must_use]
    pub fn is_failed(&self) -> bool {
        self.failed
    }

    /// Release the current binding and occurrence counter without looking
    /// ahead, even when the current bag has an overflowing total cardinality.
    pub fn close(&mut self) {
        self.input.close();
        self.positions = Vec::new();
        self.pending = false;
    }

    /// Produce at most `max_rows` occurrences, carrying a partly consumed bag
    /// into the next call. A nonzero request distinguishes exhaustion (`None`)
    /// from a valid zero-column batch. No next binding is requested merely to
    /// discover whether a full batch happens to be the last one.
    pub fn next_batch<E>(
        &mut self,
        max_rows: NonZeroUsize,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Option<FactorizedColumns<K>>, FactorizedReadError<E>> {
        if self.failed {
            return Err(FactorizedReadError::Failed);
        }
        if self.is_finished() {
            return Ok(None);
        }
        match self.read_batch(max_rows.get(), control) {
            Ok(batch) => {
                if self.input.is_finished() {
                    self.close();
                }
                Ok(batch)
            }
            Err(error) => {
                self.failed = true;
                self.close();
                Err(FactorizedReadError::Control(error))
            }
        }
    }

    fn read_batch<E>(
        &mut self,
        max_rows: usize,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Option<FactorizedColumns<K>>, E> {
        control(GlaExecutionEvent::Work)?;
        let mut batch = FactorizedColumns {
            variables: Vec::new(),
            columns: Vec::new(),
            rows: 0,
        };
        while batch.rows < max_rows {
            if !self.pending {
                let Some(binding) = self.input.next_binding(control)? else {
                    break;
                };
                let factors = binding.multiplicity_factors();
                if factors.contains(&0) {
                    continue;
                }
                // Every binding has one factor per declared relation. Once
                // admitted, these digits are reused; a completed bag leaves
                // them all at zero. No keys or factors are cloned here.
                for _ in self.positions.len()..factors.len() {
                    control(GlaExecutionEvent::ScratchEntry)?;
                    self.positions.push(0);
                }
                debug_assert_eq!(self.positions.len(), factors.len());
                self.pending = true;
            }
            let binding = self.input.current_binding();
            if batch.rows == 0 {
                for &variable in self.variables {
                    control(GlaExecutionEvent::ScratchEntry)?;
                    batch.variables.push(variable);
                    control(GlaExecutionEvent::ScratchEntry)?;
                    batch.columns.push(Vec::new());
                }
            }
            control(GlaExecutionEvent::Work)?;
            for (column, value) in batch.columns.iter_mut().zip(binding.values()) {
                control(GlaExecutionEvent::ScratchEntry)?;
                column.push(value.clone());
            }
            batch.rows += 1;
            self.pending = false;
            for (position, &factor) in self
                .positions
                .iter_mut()
                .zip(binding.multiplicity_factors())
                .rev()
            {
                control(GlaExecutionEvent::Work)?;
                // A live digit is below its positive usize factor, so the
                // increment cannot overflow even when the factor is MAX.
                *position += 1;
                if *position < factor {
                    self.pending = true;
                    break;
                }
                *position = 0;
            }
        }
        Ok((batch.rows != 0).then_some(batch))
    }
}

#[cfg(test)]
mod batch_tests {
    use super::*;
    use crate::free_join::{ColumnarCursor, ColumnarTrie, JoinVariable as V};

    fn limit(rows: usize) -> NonZeroUsize {
        NonZeroUsize::new(rows).unwrap()
    }

    fn rows(batch: &FactorizedColumns<i64>) -> Vec<Vec<i64>> {
        (0..batch.row_count())
            .map(|at| batch.columns().iter().map(|column| column[at]).collect())
            .collect()
    }

    fn inputs(plan: &FreeJoinPlan, data: &[Vec<Vec<i64>>]) -> Vec<ColumnarTrie<i64>> {
        data.iter()
            .enumerate()
            .map(|(at, rows)| {
                ColumnarTrie::new(
                    &plan.schemas[at],
                    plan.required_order(at).unwrap(),
                    rows.clone(),
                    19,
                    &mut |_| Ok::<_, ()>(()),
                )
                .unwrap()
            })
            .collect()
    }

    #[test]
    fn every_batch_size_preserves_bag_occurrences_and_physical_columns() {
        let plan = FreeJoinPlan::binary(vec![V(1), V(0)], vec![V(2), V(1)]).unwrap();
        let left = vec![vec![2, 10], vec![2, 10], vec![3, 11]];
        let right = vec![
            vec![20, 2],
            vec![20, 2],
            vec![20, 2],
            vec![21, 2],
            vec![22, 3],
        ];
        // Independent occurrence-by-occurrence natural join, not a trie or
        // multiplicity counter. The physical order here is [1, 0, 2].
        let mut expected = Vec::new();
        for a in &left {
            for b in &right {
                if a[0] == b[1] {
                    expected.push(vec![a[0], a[1], b[0]]);
                }
            }
        }
        expected.sort();
        let data = inputs(&plan, &[left, right]);
        let join = FreeJoin::new(&plan, &data).unwrap();
        for size in 1..=12 {
            let mut cursor = join.batch_cursor();
            let mut actual = Vec::new();
            while let Some(batch) = cursor
                .next_batch(limit(size), &mut |_| Ok::<_, ()>(()))
                .unwrap()
            {
                assert_eq!(batch.variables(), &[V(1), V(0), V(2)]);
                assert!((1..=size).contains(&batch.row_count()));
                assert!(
                    batch
                        .columns()
                        .iter()
                        .all(|column| column.len() == batch.row_count())
                );
                actual.extend(rows(&batch));
            }
            assert_eq!(actual, expected, "batch size {size}");
            assert!(cursor.is_finished());
            assert!(!cursor.is_failed());
            assert_eq!(cursor.positions.capacity(), 0);
            assert!(
                cursor
                    .next_batch(limit(1), &mut |_| -> Result<(), ()> { panic!("exhausted") })
                    .unwrap()
                    .is_none()
            );
        }
    }

    #[test]
    fn overflowing_nullary_bags_can_still_produce_finite_batches() {
        let plan = FreeJoinPlan::generic(vec![Vec::new(); 5], Vec::new()).unwrap();
        let data: Vec<ColumnarTrie<i64>> = (0..5)
            .map(|_| {
                ColumnarTrie::new_weighted(
                    &[],
                    &[],
                    vec![(Vec::new(), usize::MAX)],
                    19,
                    &mut |_| Ok::<_, ()>(()),
                )
                .unwrap()
            })
            .collect();
        let join = FreeJoin::new(&plan, &data).unwrap();
        let materialized = join.factorize(&mut |_| Ok::<_, ()>(())).unwrap();
        assert_eq!(materialized.cardinality(), Err(MultiplicityOverflow));
        assert!(matches!(materialized.cursor(), Err(MultiplicityOverflow)));
        let mut cursor = join.batch_cursor();
        for count in [3, 1, 7] {
            let batch = cursor
                .next_batch(limit(count), &mut |_| Ok::<_, ()>(()))
                .unwrap()
                .unwrap();
            assert_eq!(batch.row_count(), count);
            assert!(batch.variables().is_empty());
            assert!(batch.columns().is_empty());
        }
        assert!(!cursor.is_finished());
        cursor.close();
        assert_eq!(cursor.positions.capacity(), 0);
        assert!(
            cursor
                .next_batch(limit(1), &mut |_| -> Result<(), ()> { panic!("closed") })
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn late_batch_refusals_publish_nothing_and_keep_earlier_batches_valid() {
        let plan = FreeJoinPlan::binary(vec![V(0)], vec![V(1)]).unwrap();
        let data = inputs(
            &plan,
            &[
                vec![vec![1], vec![1], vec![2]],
                vec![vec![7], vec![8], vec![8]],
            ],
        );
        let join = FreeJoin::new(&plan, &data).unwrap();
        let mut complete = join.batch_cursor();
        let first = complete
            .next_batch(limit(4), &mut |_| Ok::<_, ()>(()))
            .unwrap()
            .unwrap();
        let expected_first = vec![vec![1, 7], vec![1, 7], vec![1, 8], vec![1, 8]];
        assert_eq!(rows(&first), expected_first);
        let mut events = 0;
        let second = complete
            .next_batch(limit(4), &mut |_| {
                events += 1;
                Ok::<_, ()>(())
            })
            .unwrap()
            .unwrap();
        assert_eq!(
            rows(&second),
            vec![vec![1, 8], vec![1, 8], vec![2, 7], vec![2, 8]]
        );
        let last = complete
            .next_batch(limit(4), &mut |_| Ok::<_, ()>(()))
            .unwrap()
            .unwrap();
        assert_eq!(rows(&last), vec![vec![2, 8]]);
        for refusal in 0..events {
            let mut cursor = join.batch_cursor();
            let delivered = cursor
                .next_batch(limit(4), &mut |_| Ok::<_, ()>(()))
                .unwrap()
                .unwrap();
            let mut at = 0;
            let result = cursor.next_batch(limit(4), &mut |_| {
                let current = at;
                at += 1;
                if current == refusal { Err(()) } else { Ok(()) }
            });
            assert!(matches!(result, Err(FactorizedReadError::Control(()))));
            assert_eq!(rows(&delivered), expected_first);
            assert!(cursor.is_failed());
            assert!(cursor.is_finished());
            assert_eq!(cursor.positions.capacity(), 0);
            assert!(matches!(
                cursor.next_batch(limit(1), &mut |_| -> Result<(), ()> { panic!("poisoned") }),
                Err(FactorizedReadError::Failed)
            ));
        }
    }

    #[test]
    fn first_batch_refusal_precedes_sources_and_unit_is_not_end_of_stream() {
        struct PoisonSource;
        impl TrieRelation<i64> for PoisonSource {
            type Cursor<'a> = ColumnarCursor<'a, i64>;

            fn attribute_order(&self) -> &[V] {
                &[V(0)]
            }

            fn cursor(&self) -> Self::Cursor<'_> {
                panic!("a refused batch must not open a source")
            }
        }
        let plan = FreeJoinPlan::generic(vec![vec![V(0)]], vec![V(0)]).unwrap();
        let data = [PoisonSource];
        let join = FreeJoin::new(&plan, &data).unwrap();
        let mut cursor = join.batch_cursor();
        let mut events = Vec::new();
        assert!(matches!(
            cursor.next_batch(limit(5), &mut |event| {
                events.push(event);
                Err::<(), _>("cancelled")
            }),
            Err(FactorizedReadError::Control("cancelled"))
        ));
        assert_eq!(events, vec![GlaExecutionEvent::Work]);
        assert_eq!(cursor.positions.capacity(), 0);

        let unit = FreeJoinPlan::generic(Vec::new(), Vec::new()).unwrap();
        let data: Vec<ColumnarTrie<i64>> = Vec::new();
        let join = FreeJoin::new(&unit, &data).unwrap();
        let mut cursor = join.batch_cursor();
        let batch = cursor
            .next_batch(limit(3), &mut |_| Ok::<_, ()>(()))
            .unwrap()
            .unwrap();
        assert_eq!(rows(&batch), vec![Vec::<i64>::new()]);
        assert!(
            cursor
                .next_batch(limit(3), &mut |_| Ok::<_, ()>(()))
                .unwrap()
                .is_none()
        );

        let empty_plan = FreeJoinPlan::generic(vec![vec![V(0)]], vec![V(0)]).unwrap();
        let data = inputs(&empty_plan, &[Vec::new()]);
        let join = FreeJoin::new(&empty_plan, &data).unwrap();
        assert!(
            join.batch_cursor()
                .next_batch(limit(3), &mut |_| Ok::<_, ()>(()))
                .unwrap()
                .is_none()
        );
    }
}
