use super::plan::Stage;
use super::{FreeJoinPlan, JoinPlanError, JoinVariable, TrieCursor, TrieRelation};
use crate::GlaExecutionEvent;
use core::marker::PhantomData;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MultiplicityOverflow;
impl core::fmt::Display for MultiplicityOverflow {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("FreeJoin multiplicity exceeds u128")
    }
}
impl core::error::Error for MultiplicityOverflow {}

/// One distinct full assignment, with a factored bag multiplicity. Neither
/// projection nor DISTINCT is implicit. Factors avoid overflow during joining;
/// requesting a numeric cardinality is separately checked.
#[derive(Clone, Copy)]
pub struct JoinBinding<'a, K> {
    pub(super) variables: &'a [JoinVariable],
    pub(super) values: &'a [Option<K>],
    pub(super) factors: &'a [usize],
}
impl<K> JoinBinding<'_, K> {
    #[must_use]
    pub fn variables(&self) -> &[JoinVariable] {
        self.variables
    }

    #[must_use]
    pub fn get(&self, variable: JoinVariable) -> Option<&K> {
        self.variables
            .iter()
            .position(|v| *v == variable)
            .and_then(|at| self.values[at].as_ref())
    }

    pub fn values(&self) -> impl Iterator<Item = &K> {
        self.values
            .iter()
            .map(|value| value.as_ref().expect("complete join binding"))
    }

    #[must_use]
    pub fn multiplicity_factors(&self) -> &[usize] {
        self.factors
    }

    pub fn multiplicity(&self) -> Result<u128, MultiplicityOverflow> {
        self.factors.iter().try_fold(1_u128, |count, &factor| {
            count
                .checked_mul(factor as u128)
                .ok_or(MultiplicityOverflow)
        })
    }

    /// Explicit flattening at a semantic boundary. The mixed-radix counter
    /// never multiplies factors and checkpoints before every occurrence. A
    /// caller exposing results must own its normal transactional output guard.
    pub fn for_each_occurrence<E, C>(
        &self,
        control: &mut C,
        mut emit: impl FnMut(&Self, &mut C) -> Result<(), E>,
    ) -> Result<(), E>
    where
        C: FnMut(GlaExecutionEvent) -> Result<(), E>,
    {
        if self.factors.contains(&0) {
            return Ok(());
        }
        let mut positions = Vec::new();
        for _ in self.factors {
            control(GlaExecutionEvent::ScratchEntry)?;
            positions.push(0_usize);
        }
        loop {
            control(GlaExecutionEvent::Work)?;
            emit(self, control)?;
            let mut digit = positions.len();
            loop {
                if digit == 0 {
                    return Ok(());
                }
                digit -= 1;
                control(GlaExecutionEvent::Work)?;
                positions[digit] += 1;
                if positions[digit] < self.factors[digit] {
                    break;
                }
                positions[digit] = 0;
            }
        }
    }
}

/// Binds a physical plan to immutable-generation trie access paths. Source
/// order mismatches are rejected before any key is observed.
pub struct FreeJoin<'a, K, T> {
    pub(super) plan: &'a FreeJoinPlan,
    pub(super) relations: &'a [T],
    marker: PhantomData<K>,
}
impl<'a, K: Ord + Clone, T: TrieRelation<K>> FreeJoin<'a, K, T> {
    pub fn new(plan: &'a FreeJoinPlan, relations: &'a [T]) -> Result<Self, JoinPlanError> {
        if relations.len() != plan.relation_count() {
            return Err(JoinPlanError::RelationCount {
                expected: plan.relation_count(),
                actual: relations.len(),
            });
        }
        for (relation, source) in relations.iter().enumerate() {
            if source.attribute_order() != plan.required_orders[relation].as_slice() {
                return Err(JoinPlanError::AttributeOrder { relation });
            }
        }
        Ok(Self {
            plan,
            relations,
            marker: PhantomData,
        })
    }

    /// Push complete distinct bindings without retaining a result relation.
    /// All intermediate state is bounded by the plan, not the output size.
    /// Multiplicity is carried to the sink instead of enumerated in prefixes.
    /// Push and pull consumers share one resumable physical execution kernel.
    pub fn for_each_binding<E, C>(
        &self,
        control: &mut C,
        mut emit: impl FnMut(JoinBinding<'_, K>, &mut C) -> Result<(), E>,
    ) -> Result<(), E>
    where
        C: FnMut(GlaExecutionEvent) -> Result<(), E>,
    {
        let mut cursor = self.cursor();
        while let Some(binding) = cursor.next_binding(control)? {
            emit(binding, control)?;
        }
        Ok(())
    }
}

/// A resumable execution of the same covered-group join used by
/// [`FreeJoin::for_each_binding`]. Construction does not open a source. Each
/// pull stops at one complete distinct binding and retains only navigation
/// state and one binding, never the result relation. Bag multiplicities remain
/// factored until the consumer explicitly asks to flatten them.
///
/// A control error is returned once and permanently fuses the cursor. Closing,
/// exhausting or dropping it releases every source cursor without traversing
/// the unused suffix. Admission events are the existing logical work/scratch
/// units, not a new allocator-byte accounting or spill guarantee.
pub struct FreeJoinCursor<'a, K: Ord + Clone + 'a, T: TrieRelation<K> + 'a> {
    plan: &'a FreeJoinPlan,
    sources: Option<&'a [T]>,
    state: Option<BindingState<K, T::Cursor<'a>>>,
    finished: bool,
    failed: bool,
}

struct BindingState<K, C> {
    cursors: Vec<C>,
    values: Vec<Option<K>>,
    factors: Vec<usize>,
    levels: Vec<BindingLevel<K, C>>,
    depth: usize,
    yielded: bool,
}

struct BindingLevel<K, C> {
    candidates: GroupScan<K, C>,
    saved: Vec<(usize, C)>,
}

impl<K: Ord + Clone, T: TrieRelation<K>> core::fmt::Debug for FreeJoinCursor<'_, K, T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("FreeJoinCursor")
            .field("finished", &self.finished)
            .field("failed", &self.failed)
            .field("values", &"[REDACTED]")
            .finish()
    }
}

impl<K: Ord + Clone, T: TrieRelation<K>> FreeJoin<'_, K, T> {
    /// Prepare a pull producer without allocating execution state, opening a
    /// source, cloning a key or requesting any candidate. The checked physical
    /// plan and input orders have already been validated by [`FreeJoin::new`].
    #[must_use]
    pub fn cursor(&self) -> FreeJoinCursor<'_, K, T> {
        FreeJoinCursor {
            plan: self.plan,
            sources: Some(self.relations),
            state: None,
            finished: false,
            failed: false,
        }
    }
}

impl<K: Ord + Clone, T: TrieRelation<K>> FreeJoinCursor<'_, K, T> {
    // A flattening producer may retain this complete binding between batches
    // without copying its keys or advancing the join. No borrow survives the
    // next mutable cursor operation.
    pub(super) fn current_binding(&self) -> JoinBinding<'_, K> {
        let state = self.state.as_ref().expect("an active binding has state");
        debug_assert!(state.yielded && !self.finished);
        JoinBinding {
            variables: &self.plan.order,
            values: &state.values,
            factors: &state.factors,
        }
    }

    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.finished
    }

    #[must_use]
    pub fn is_failed(&self) -> bool {
        self.failed
    }

    /// Release navigation state immediately. This neither polls sources nor
    /// calls the control callback, including when invoked before the first pull.
    pub fn close(&mut self) {
        self.sources = None;
        self.state = None;
        self.finished = true;
    }

    /// Borrow the next complete binding until the following mutable cursor
    /// operation. There is no lookahead into the next binding after this one
    /// succeeds. Exhaustion and all calls after a refusal return `Ok(None)`.
    pub fn next_binding<E>(
        &mut self,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Option<JoinBinding<'_, K>>, E> {
        if self.finished {
            return Ok(None);
        }
        match self.advance(control) {
            Ok(true) => {
                let state = self.state.as_ref().expect("a yielded binding has state");
                Ok(Some(JoinBinding {
                    variables: &self.plan.order,
                    values: &state.values,
                    factors: &state.factors,
                }))
            }
            Ok(false) => {
                self.close();
                Ok(None)
            }
            Err(error) => {
                self.failed = true;
                self.close();
                Err(error)
            }
        }
    }

    fn advance<E>(
        &mut self,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<bool, E> {
        // In particular, a cancelled/zero-work first pull cannot open a
        // source or allocate arrays proportional to the checked plan.
        control(GlaExecutionEvent::Work)?;
        if let Some(sources) = self.sources.take() {
            let mut cursors = Vec::new();
            for source in sources {
                control(GlaExecutionEvent::Work)?;
                control(GlaExecutionEvent::ScratchEntry)?;
                let cursor = source.cursor();
                if cursor.multiplicity() == Some(0)
                    || (!cursor.remaining_order().is_empty() && cursor.distinct_prefixes(1) == 0)
                {
                    return Ok(false);
                }
                cursors.push(cursor);
            }
            let mut values = Vec::new();
            for _ in &self.plan.order {
                control(GlaExecutionEvent::ScratchEntry)?;
                values.push(None);
            }
            let mut factors = Vec::new();
            for _ in sources {
                control(GlaExecutionEvent::ScratchEntry)?;
                factors.push(1_usize);
            }
            self.state = Some(BindingState {
                cursors,
                values,
                factors,
                levels: Vec::new(),
                depth: 0,
                yielded: false,
            });
        }
        self.state
            .as_mut()
            .expect("an unfinished cursor has sources or execution state")
            .advance(self.plan, control)
    }
}

impl<K: Ord + Clone, C: TrieCursor<K>> BindingState<K, C> {
    fn advance<E>(
        &mut self,
        plan: &FreeJoinPlan,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<bool, E> {
        if self.yielded {
            self.yielded = false;
            if self.depth == 0 {
                // The zero-variable natural join produces exactly one
                // distinct empty binding, with its nullary input factors.
                return Ok(false);
            }
            self.depth -= 1;
        }
        loop {
            control(GlaExecutionEvent::Work)?;
            let Some(stage) = plan.stages.get(self.depth) else {
                for (relation, cursor) in self.cursors.iter().enumerate() {
                    control(GlaExecutionEvent::Work)?;
                    self.factors[relation] = cursor
                        .multiplicity()
                        .expect("a checked plan binds every attribute");
                }
                self.yielded = true;
                return Ok(true);
            };
            if self.levels.len() == self.depth {
                let proposer = driver(stage, &self.cursors, control)?;
                control(GlaExecutionEvent::ScratchEntry)?;
                for _ in &stage.probes {
                    control(GlaExecutionEvent::ScratchEntry)?;
                }
                let saved = Vec::with_capacity(stage.probes.len());
                let candidates = GroupScan::new(
                    self.cursors[proposer].clone(),
                    stage.variables.len(),
                    control,
                )?;
                self.levels.push(BindingLevel { candidates, saved });
            }
            let level = self.levels.last_mut().expect("an active stage has a level");
            // Undo both a previously yielded candidate and a partially
            // successful probe before proposing the next prefix. Descendant
            // levels have already been removed before a parent is resumed.
            for (relation, original) in level.saved.drain(..).rev() {
                control(GlaExecutionEvent::Work)?;
                self.cursors[relation] = original;
            }
            let Some(candidate) = level.candidates.next(control)? else {
                self.levels.pop();
                for value in &mut self.values[stage.start..stage.start + stage.variables.len()] {
                    control(GlaExecutionEvent::Work)?;
                    *value = None;
                }
                if self.depth == 0 {
                    return Ok(false);
                }
                self.depth -= 1;
                continue;
            };
            let mut accepted = true;
            for access in &stage.probes {
                // Cursor clones copy navigation only. The level's saved slots
                // were admitted before any candidate was requested.
                let original = self.cursors[access.relation].clone();
                let Some(child) =
                    probe(original.clone(), &access.key_positions, candidate, control)?
                else {
                    accepted = false;
                    break;
                };
                level.saved.push((access.relation, original));
                self.cursors[access.relation] = child;
            }
            if accepted {
                for (offset, key) in candidate.iter().enumerate() {
                    control(GlaExecutionEvent::ScratchEntry)?;
                    self.values[stage.start + offset] = Some(key.clone());
                }
                self.depth += 1;
            }
        }
    }
}

/// Iterate a covered multi-attribute projection without materializing it.
/// The stack is at most the admitted group arity; duplicate suffix tuples are
/// skipped by opening a prefix once rather than scanning underlying rows.
pub(super) struct GroupScan<K, T> {
    stack: Vec<T>,
    keys: Vec<K>,
    attributes: usize,
    yielded: bool,
}
impl<K: Ord + Clone, T: TrieCursor<K>> GroupScan<K, T> {
    pub(super) fn new<E>(
        root: T,
        attributes: usize,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Self, E> {
        for _ in 0..attributes {
            control(GlaExecutionEvent::ScratchEntry)?;
        }
        let mut stack = Vec::with_capacity(attributes);
        stack.push(root);
        Ok(Self {
            stack,
            keys: Vec::with_capacity(attributes),
            attributes,
            yielded: false,
        })
    }

    pub(super) fn next<E>(
        &mut self,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Option<&[K]>, E> {
        if self.yielded {
            self.keys.pop();
            self.stack
                .last_mut()
                .expect("yielded group has a level")
                .advance(control)?;
            self.yielded = false;
        }
        loop {
            let Some(cursor) = self.stack.last() else {
                return Ok(None);
            };
            let Some(key) = cursor.key() else {
                self.stack.pop();
                if let Some(parent) = self.stack.last_mut() {
                    self.keys.pop();
                    parent.advance(control)?;
                }
                continue;
            };
            control(GlaExecutionEvent::ScratchEntry)?;
            self.keys.push(key.clone());
            if self.stack.len() == self.attributes {
                self.yielded = true;
                return Ok(Some(&self.keys));
            }
            let child = cursor
                .open(control)?
                .expect("a current trie key has a child");
            self.stack.push(child);
        }
    }
}

pub(super) fn driver<K: Ord + Clone, T: TrieCursor<K>, E>(
    stage: &Stage,
    cursors: &[T],
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<usize, E> {
    let mut best = stage.covers[0];
    let mut smallest = usize::MAX;
    for &relation in &stage.covers {
        // Prefix counting is bounded by the declared group width.
        for _ in &stage.variables {
            control(GlaExecutionEvent::Work)?;
        }
        let count = cursors[relation].distinct_prefixes(stage.variables.len());
        if count < smallest {
            best = relation;
            smallest = count;
        }
    }
    Ok(best)
}

pub(super) fn probe<K: Ord + Clone, T: TrieCursor<K>, E>(
    mut cursor: T,
    positions: &[usize],
    candidate: &[K],
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<Option<T>, E> {
    for &position in positions {
        cursor.seek(&candidate[position], control)?;
        control(GlaExecutionEvent::Work)?;
        if cursor.key() != Some(&candidate[position]) {
            return Ok(None);
        }
        cursor = cursor
            .open(control)?
            .expect("matching trie key has a child");
    }
    Ok(Some(cursor))
}

#[cfg(test)]
mod cursor_tests {
    use super::*;
    use crate::free_join::{ColumnarCursor, ColumnarTrie};
    use std::cell::Cell;

    #[derive(Default)]
    struct Counts {
        opens: Cell<usize>,
        operations: Cell<usize>,
        live: Cell<usize>,
        peak: Cell<usize>,
    }

    struct Tracked<'a> {
        trie: ColumnarTrie<i64>,
        counts: &'a Counts,
    }

    struct TrackedCursor<'a> {
        cursor: ColumnarCursor<'a, i64>,
        counts: &'a Counts,
    }

    impl<'a> TrackedCursor<'a> {
        fn new(cursor: ColumnarCursor<'a, i64>, counts: &'a Counts) -> Self {
            let live = counts.live.get() + 1;
            counts.live.set(live);
            counts.peak.set(counts.peak.get().max(live));
            Self { cursor, counts }
        }

        fn observe(&self) {
            self.counts.operations.set(self.counts.operations.get() + 1);
        }
    }

    impl Clone for TrackedCursor<'_> {
        fn clone(&self) -> Self {
            Self::new(self.cursor.clone(), self.counts)
        }
    }

    impl Drop for TrackedCursor<'_> {
        fn drop(&mut self) {
            self.counts.live.set(self.counts.live.get() - 1);
        }
    }

    impl TrieRelation<i64> for Tracked<'_> {
        type Cursor<'a>
            = TrackedCursor<'a>
        where
            Self: 'a;

        fn attribute_order(&self) -> &[JoinVariable] {
            self.trie.attribute_order()
        }

        fn cursor(&self) -> Self::Cursor<'_> {
            self.counts.opens.set(self.counts.opens.get() + 1);
            TrackedCursor::new(self.trie.cursor(), self.counts)
        }
    }

    impl TrieCursor<i64> for TrackedCursor<'_> {
        fn remaining_order(&self) -> &[JoinVariable] {
            self.cursor.remaining_order()
        }

        fn generation(&self) -> u64 {
            self.cursor.generation()
        }

        fn key(&self) -> Option<&i64> {
            self.observe();
            self.cursor.key()
        }

        fn multiplicity(&self) -> Option<usize> {
            self.observe();
            self.cursor.multiplicity()
        }

        fn distinct_prefixes(&self, attributes: usize) -> usize {
            self.observe();
            self.cursor.distinct_prefixes(attributes)
        }

        fn advance<E>(
            &mut self,
            control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
        ) -> Result<(), E> {
            self.observe();
            self.cursor.advance(control)
        }

        fn seek<E>(
            &mut self,
            target: &i64,
            control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
        ) -> Result<(), E> {
            self.observe();
            self.cursor.seek(target, control)
        }

        fn open<E>(
            &self,
            control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
        ) -> Result<Option<Self>, E> {
            self.observe();
            Ok(self
                .cursor
                .open(control)?
                .map(|cursor| Self::new(cursor, self.counts)))
        }
    }

    fn sources<'a>(
        plan: &FreeJoinPlan,
        inputs: &[Vec<Vec<i64>>],
        counts: &'a Counts,
    ) -> Vec<Tracked<'a>> {
        inputs
            .iter()
            .enumerate()
            .map(|(at, rows)| Tracked {
                trie: ColumnarTrie::new(
                    &plan.schemas[at],
                    plan.required_order(at).unwrap(),
                    rows.clone(),
                    71,
                    &mut |_| Ok::<_, ()>(()),
                )
                .unwrap(),
                counts,
            })
            .collect()
    }

    #[test]
    fn first_refusal_and_unstarted_close_never_open_a_source() {
        let plan =
            FreeJoinPlan::generic(vec![vec![JoinVariable(0)]], vec![JoinVariable(0)]).unwrap();
        let counts = Counts::default();
        let data = sources(&plan, &[vec![vec![1], vec![2]]], &counts);
        let join = FreeJoin::new(&plan, &data).unwrap();
        let mut cursor = join.cursor();
        assert_eq!(counts.opens.get(), 0);
        let mut events = Vec::new();
        assert!(matches!(
            cursor.next_binding(&mut |event| {
                events.push(event);
                Err::<(), _>("cancelled")
            }),
            Err("cancelled")
        ));
        assert_eq!(events, vec![GlaExecutionEvent::Work]);
        assert_eq!(counts.opens.get(), 0);
        assert_eq!(counts.operations.get(), 0);
        assert_eq!(counts.live.get(), 0);
        assert!(cursor.is_finished());
        assert!(cursor.is_failed());
        assert!(
            cursor
                .next_binding(&mut |_| -> Result<(), ()> { panic!("fused cursor") })
                .unwrap()
                .is_none()
        );
        let mut closed = join.cursor();
        closed.close();
        assert!(!closed.is_failed());
        assert!(
            closed
                .next_binding(&mut |_| -> Result<(), ()> { panic!("closed cursor") })
                .unwrap()
                .is_none()
        );
        assert_eq!(counts.opens.get(), 0);
    }

    #[test]
    fn one_pull_and_close_do_not_drive_a_product_suffix() {
        let measure = |size: i64| {
            let plan = FreeJoinPlan::binary(vec![JoinVariable(0)], vec![JoinVariable(1)]).unwrap();
            let counts = Counts::default();
            let rows: Vec<_> = (0..size).map(|value| vec![value]).collect();
            let data = sources(&plan, &[rows.clone(), rows], &counts);
            let join = FreeJoin::new(&plan, &data).unwrap();
            let mut cursor = join.cursor();
            let binding = cursor
                .next_binding(&mut |_| Ok::<_, ()>(()))
                .unwrap()
                .unwrap();
            assert_eq!(binding.values().copied().collect::<Vec<_>>(), vec![0, 0]);
            assert_eq!(binding.multiplicity(), Ok(1));
            let operations = counts.operations.get();
            let peak = counts.peak.get();
            assert!(counts.live.get() > 0);
            cursor.close();
            assert_eq!(counts.live.get(), 0);
            assert_eq!(counts.operations.get(), operations);
            assert!(
                cursor
                    .next_binding(&mut |_| -> Result<(), ()> { panic!("closed cursor") })
                    .unwrap()
                    .is_none()
            );
            (operations, peak)
        };
        // The second input would have over sixteen million output bindings.
        // Neither a result relation nor the unrequested suffix is produced.
        assert_eq!(measure(16), measure(4096));
    }

    #[test]
    fn every_control_refusal_fuses_and_releases_suspended_join_state() {
        let schemas = vec![
            vec![JoinVariable(0), JoinVariable(1)],
            vec![JoinVariable(1), JoinVariable(2)],
            vec![JoinVariable(0), JoinVariable(2)],
        ];
        let inputs = vec![
            vec![vec![1, 2], vec![1, 2], vec![1, 4], vec![3, 2]],
            vec![vec![2, 5], vec![2, 6], vec![4, 7]],
            vec![vec![1, 5], vec![1, 7], vec![3, 6]],
        ];
        let expected = vec![(vec![1, 2, 5], 2), (vec![1, 4, 7], 1), (vec![3, 2, 6], 1)];
        let plans = [
            FreeJoinPlan::generic(
                schemas.clone(),
                vec![JoinVariable(0), JoinVariable(1), JoinVariable(2)],
            )
            .unwrap(),
            FreeJoinPlan::new(
                schemas,
                vec![
                    vec![JoinVariable(0), JoinVariable(1)],
                    vec![JoinVariable(2)],
                ],
            )
            .unwrap(),
        ];
        for plan in plans {
            let counts = Counts::default();
            let data = sources(&plan, &inputs, &counts);
            let join = FreeJoin::new(&plan, &data).unwrap();
            let mut events = 0;
            let mut observed = Vec::new();
            let mut cursor = join.cursor();
            while let Some(binding) = cursor
                .next_binding(&mut |_| {
                    events += 1;
                    Ok::<_, ()>(())
                })
                .unwrap()
            {
                observed.push((
                    binding.values().copied().collect::<Vec<_>>(),
                    binding.multiplicity().unwrap(),
                ));
            }
            assert_eq!(observed, expected);
            assert_eq!(counts.live.get(), 0);
            for refusal in 0..events {
                let mut cursor = join.cursor();
                let mut at = 0;
                let mut prefix = Vec::new();
                loop {
                    match cursor.next_binding(&mut |_| {
                        let current = at;
                        at += 1;
                        if current == refusal { Err(()) } else { Ok(()) }
                    }) {
                        Ok(Some(binding)) => prefix.push((
                            binding.values().copied().collect::<Vec<_>>(),
                            binding.multiplicity().unwrap(),
                        )),
                        Err(()) => break,
                        Ok(None) => panic!("control boundary {refusal} was not visited"),
                    }
                }
                assert_eq!(prefix, expected[..prefix.len()]);
                assert_eq!(counts.live.get(), 0, "refusal {refusal}");
                assert!(cursor.is_failed());
                let operations = counts.operations.get();
                assert!(
                    cursor
                        .next_binding(&mut |_| -> Result<(), ()> { panic!("refused cursor") })
                        .unwrap()
                        .is_none()
                );
                assert_eq!(counts.operations.get(), operations);
            }
            let result =
                join.for_each_binding(&mut |_| Ok::<_, &'static str>(()), |_, _| Err("sink"));
            assert_eq!(result, Err("sink"));
            assert_eq!(counts.live.get(), 0, "sink refusal releases the producer");
        }
    }

    #[test]
    fn nullary_identity_empty_inputs_and_overflowing_bags_remain_distinct() {
        let identity = FreeJoinPlan::generic(Vec::new(), Vec::new()).unwrap();
        let empty: Vec<ColumnarTrie<i64>> = Vec::new();
        let join = FreeJoin::new(&identity, &empty).unwrap();
        let mut cursor = join.cursor();
        let binding = cursor
            .next_binding(&mut |_| Ok::<_, ()>(()))
            .unwrap()
            .unwrap();
        assert_eq!(binding.values().count(), 0);
        assert_eq!(binding.multiplicity(), Ok(1));
        assert!(
            cursor
                .next_binding(&mut |_| Ok::<_, ()>(()))
                .unwrap()
                .is_none()
        );

        let plan = FreeJoinPlan::binary(Vec::new(), vec![JoinVariable(0)]).unwrap();
        let counts = Counts::default();
        let data = sources(&plan, &[Vec::new(), vec![vec![1]]], &counts);
        let join = FreeJoin::new(&plan, &data).unwrap();
        let mut cursor = join.cursor();
        assert!(
            cursor
                .next_binding(&mut |_| Ok::<_, ()>(()))
                .unwrap()
                .is_none()
        );
        assert_eq!(
            counts.opens.get(),
            1,
            "an empty input stops source preparation"
        );
        assert_eq!(counts.live.get(), 0);

        let plan = FreeJoinPlan::generic(vec![Vec::new(); 5], Vec::new()).unwrap();
        let data: Vec<ColumnarTrie<i64>> = (0..5)
            .map(|_| {
                ColumnarTrie::new_weighted(&[], &[], vec![(Vec::new(), usize::MAX)], 1, &mut |_| {
                    Ok::<_, ()>(())
                })
                .unwrap()
            })
            .collect();
        let join = FreeJoin::new(&plan, &data).unwrap();
        let mut cursor = join.cursor();
        let binding = cursor
            .next_binding(&mut |_| Ok::<_, ()>(()))
            .unwrap()
            .unwrap();
        assert_eq!(binding.multiplicity_factors(), &[usize::MAX; 5]);
        assert_eq!(binding.multiplicity(), Err(MultiplicityOverflow));
        assert!(
            cursor
                .next_binding(&mut |_| Ok::<_, ()>(()))
                .unwrap()
                .is_none()
        );
    }
}
