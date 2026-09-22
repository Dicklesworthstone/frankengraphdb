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
    pub fn for_each_binding<E, C>(
        &self,
        control: &mut C,
        mut emit: impl FnMut(JoinBinding<'_, K>, &mut C) -> Result<(), E>,
    ) -> Result<(), E>
    where
        C: FnMut(GlaExecutionEvent) -> Result<(), E>,
    {
        let mut cursors = Vec::new();
        for relation in self.relations {
            control(GlaExecutionEvent::ScratchEntry)?;
            let cursor = relation.cursor();
            if cursor.multiplicity() == Some(0)
                || (!cursor.remaining_order().is_empty() && cursor.distinct_prefixes(1) == 0)
            {
                return Ok(());
            }
            cursors.push(cursor);
        }
        let mut values = Vec::new();
        for _ in &self.plan.order {
            control(GlaExecutionEvent::ScratchEntry)?;
            values.push(None);
        }
        let mut factors = Vec::new();
        for _ in self.relations {
            control(GlaExecutionEvent::ScratchEntry)?;
            factors.push(1_usize);
        }
        visit(
            self.plan,
            0,
            &mut cursors,
            &mut values,
            &mut factors,
            control,
            &mut emit,
        )
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

#[allow(clippy::too_many_arguments)]
fn visit<K: Ord + Clone, T: TrieCursor<K>, E, C, F>(
    plan: &FreeJoinPlan,
    at: usize,
    cursors: &mut [T],
    values: &mut [Option<K>],
    factors: &mut [usize],
    control: &mut C,
    emit: &mut F,
) -> Result<(), E>
where
    C: FnMut(GlaExecutionEvent) -> Result<(), E>,
    F: FnMut(JoinBinding<'_, K>, &mut C) -> Result<(), E>,
{
    control(GlaExecutionEvent::Work)?;
    let Some(stage) = plan.stages.get(at) else {
        for (relation, cursor) in cursors.iter().enumerate() {
            control(GlaExecutionEvent::Work)?;
            factors[relation] = cursor
                .multiplicity()
                .expect("a checked plan binds every attribute");
        }
        return emit(
            JoinBinding {
                variables: &plan.order,
                values,
                factors,
            },
            control,
        );
    };
    let proposer = driver(stage, cursors, control)?;
    let mut candidates = GroupScan::new(cursors[proposer].clone(), stage.variables.len(), control)?;
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
            for (offset, key) in candidate.iter().enumerate() {
                control(GlaExecutionEvent::ScratchEntry)?;
                values[stage.start + offset] = Some(key.clone());
            }
            visit(plan, at + 1, cursors, values, factors, control, emit)?;
        }
        for (relation, original) in saved.drain(..).rev() {
            cursors[relation] = original;
        }
    }
    Ok(())
}
