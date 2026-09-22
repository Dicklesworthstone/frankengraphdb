//! Closed, read-free GLA islands lowered to the real Generic Join family.
//!
//! The public governed/budgeted collectors enter here only after admission.
//! Internal aggregate visitors do not: their callbacks can have effects that
//! a terminal identity projection cannot have. Unknown operators, predicates,
//! captured paths, correlated scopes and property-valued columns stay on their
//! existing evaluator, with its original read/error precedence.

use super::super::{GlaDirection, GlaOperator, ProjectedRows, build_index};
use crate::GlaExecutionEvent;
use crate::algebra::{GlaOutput, GlaPlan, ValueProjection};
use crate::free_join::{
    FreeJoin, FreeJoinPlan, GraphTrie, GraphTrieError, JoinVariable, MAX_JOIN_VARIABLES,
};
use fgdb_delta_types::RelationId;
use fgdb_types::{CanonicalScalar, VId};

#[cfg(test)]
mod tests;

pub(super) struct Topology {
    physical: FreeJoinPlan,
    accesses: Vec<GlaOperator>,
    slots: Vec<JoinVariable>,
    inequalities: Vec<(usize, usize)>,
    projection: usize,
    distinct: bool,
    page_capacity: Option<u64>,
}

fn reverse(direction: GlaDirection) -> GlaDirection {
    match direction {
        GlaDirection::Forward => GlaDirection::Reverse,
        GlaDirection::Reverse => GlaDirection::Forward,
        GlaDirection::Undirected => GlaDirection::Undirected,
    }
}
fn root(parents: &[usize], mut slot: usize) -> usize {
    while parents[slot] != slot {
        slot = parents[slot];
    }
    slot
}

/// Recognize the entire shape before allocating or changing the old lane's
/// event trace. This is a bounded syntactic choice, not data-dependent planning.
/// Keep one/two-edge traversals on their cheaper existing indexed access path.
pub(super) fn compile<E>(
    operators: &[GlaOperator],
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<Option<Topology>, E> {
    if !matches!(operators.first(), Some(GlaOperator::ScanEdges { .. })) {
        return Ok(None);
    }
    let mut width = 2;
    let mut projection = None;
    for (at, operator) in operators.iter().enumerate().skip(1) {
        match operator {
            GlaOperator::Expand { source, .. }
                if (source.ordinal() as usize) < width && width < MAX_JOIN_VARIABLES =>
            {
                width += 1;
            }
            GlaOperator::VertexIdentity { left, right, .. }
                if (left.ordinal() as usize) < width && (right.ordinal() as usize) < width => {}
            GlaOperator::Project { slot } if (slot.ordinal() as usize) < width => {
                projection = Some(at);
                break;
            }
            GlaOperator::ProjectBindings { slots }
                if !slots.is_empty()
                    && slots.iter().all(|slot| (slot.ordinal() as usize) < width) =>
            {
                projection = Some(at);
                break;
            }
            GlaOperator::ProjectValues { columns }
                if !columns.is_empty()
                    && columns.iter().all(|column| {
                        matches!(column,
                    ValueProjection::Vertex { slot } if (slot.ordinal() as usize) < width)
                    }) =>
            {
                projection = Some(at);
                break;
            }
            _ => return Ok(None),
        }
    }
    let Some(projection) = projection else {
        return Ok(None);
    };
    if width < 4 {
        return Ok(None);
    }
    let mut tail = projection + 1;
    let distinct = matches!(operators.get(tail), Some(GlaOperator::Distinct));
    if distinct {
        tail += 1;
    }
    let ordered = matches!(
        (&operators[projection], operators.get(tail)),
        (
            GlaOperator::Project { .. },
            Some(GlaOperator::OrderByVertexId)
        ) | (
            GlaOperator::ProjectBindings { .. },
            Some(GlaOperator::OrderByBindings)
        ) | (
            GlaOperator::ProjectValues { .. },
            Some(GlaOperator::OrderByValues | GlaOperator::OrderByValueColumns { .. })
        )
    );
    if !ordered || operators.len() != tail + 2 {
        return Ok(None);
    }
    let Some(GlaOperator::Limit { offset, count }) = operators.last() else {
        return Ok(None);
    };
    let page_capacity = if *count == Some(0) {
        Some(0)
    } else {
        count.and_then(|count| offset.checked_add(count))
    };

    let mut parents: [usize; MAX_JOIN_VARIABLES] = core::array::from_fn(|at| at);
    for operator in &operators[1..projection] {
        control(GlaExecutionEvent::Work)?;
        if let GlaOperator::VertexIdentity {
            left,
            right,
            equal: true,
        } = operator
        {
            let a = root(&parents, left.ordinal() as usize);
            let b = root(&parents, right.ordinal() as usize);
            parents[a.max(b)] = a.min(b);
        }
    }
    let mut slots = Vec::new();
    let mut order = Vec::new();
    for at in 0..width {
        control(GlaExecutionEvent::ScratchEntry)?;
        let variable = JoinVariable(root(&parents, at) as u32);
        slots.push(variable);
        if variable.0 as usize == at {
            control(GlaExecutionEvent::ScratchEntry)?;
            order.push(variable);
        }
    }
    let mut accesses = Vec::new();
    let mut schemas = Vec::new();
    let mut inequalities = Vec::new();
    let mut appended = 2;
    for operator in &operators[..projection] {
        control(GlaExecutionEvent::Work)?;
        let (a, b, relation, direction) = match operator {
            GlaOperator::ScanEdges {
                relation,
                direction,
            } => (0, 1, *relation, *direction),
            GlaOperator::Expand {
                source,
                relation,
                direction,
            } => {
                let target = appended;
                appended += 1;
                (source.ordinal() as usize, target, *relation, *direction)
            }
            GlaOperator::VertexIdentity {
                left,
                right,
                equal: false,
            } => {
                control(GlaExecutionEvent::ScratchEntry)?;
                inequalities.push((left.ordinal() as usize, right.ordinal() as usize));
                continue;
            }
            GlaOperator::VertexIdentity { equal: true, .. } => continue,
            _ => unreachable!("complete topology shape checked above"),
        };
        let (a, b) = (slots[a], slots[b]);
        let direction = if a > b { reverse(direction) } else { direction };
        for _ in 0..4 {
            control(GlaExecutionEvent::ScratchEntry)?;
        }
        schemas.push(if a == b {
            vec![a]
        } else {
            vec![a.min(b), a.max(b)]
        });
        accesses.push(GlaOperator::ScanEdges {
            relation,
            direction,
        });
    }
    // FreeJoin's bounded compiler owns additional cover/probe metadata, never
    // row payload. Reserve per-variable/per-relation descriptors before build.
    for _ in &order {
        for _ in &schemas {
            for _ in 0..4 {
                control(GlaExecutionEvent::ScratchEntry)?;
            }
        }
    }
    let physical = FreeJoinPlan::generic(schemas, order)
        .expect("validated endpoint equivalence classes form a covered join");
    Ok(Some(Topology {
        physical,
        accesses,
        slots,
        inequalities,
        projection,
        distinct,
        page_capacity,
    }))
}

fn admitted<T, E>(result: Result<T, GraphTrieError<E>>) -> Result<T, E> {
    match result {
        Ok(value) => Ok(value),
        Err(GraphTrieError::Control(error)) => Err(error),
        Err(GraphTrieError::UnsortedInput | GraphTrieError::RepeatedVariable) => {
            unreachable!("build_index and the checked physical plan own graph-trie ordering")
        }
    }
}

impl Topology {
    pub(super) fn execute<Row: GlaOutput, E, C>(
        &self,
        logical: &GlaPlan<Row>,
        edges: impl IntoIterator<Item = (VId, RelationId, VId)>,
        control: &mut C,
    ) -> Result<Vec<Row>, E>
    where
        C: FnMut(GlaExecutionEvent) -> Result<(), E>,
    {
        // Build only advertised physical orientations, once, from the same
        // admitted source. No second snapshot scan or flat tuple copy occurs.
        let index = build_index(&self.accesses, edges, control)?;
        let mut tries = Vec::new();
        for (at, access) in self.accesses.iter().enumerate() {
            control(GlaExecutionEvent::ScratchEntry)?;
            let GlaOperator::ScanEdges {
                relation,
                direction,
            } = access
            else {
                unreachable!()
            };
            let adjacency = index
                .get(&(*relation, *direction))
                .expect("every requested orientation was registered");
            let order = self
                .physical
                .required_order(at)
                .expect("one schema per access");
            // The borrow, not this local tag, pins the source. This private
            // invocation does not issue or substitute snapshot certificates.
            let trie = if order.len() == 1 {
                admitted(GraphTrie::diagonal(order[0], adjacency, 0, control))?
            } else {
                admitted(GraphTrie::adjacency(
                    [order[0], order[1]],
                    adjacency,
                    0,
                    control,
                ))?
            };
            tries.push(trie);
        }
        let join = FreeJoin::new(&self.physical, &tries)
            .expect("each graph access advertises its required order");
        let operators = logical.operators();
        let mut projected = ProjectedRows::<Row>::for_plan(self.distinct, operators);
        let mut slots = [None; MAX_JOIN_VARIABLES];
        join.for_each_binding(control, |binding, control| {
            for (at, variable) in self.slots.iter().enumerate() {
                control(GlaExecutionEvent::Work)?;
                slots[at] = Some(*binding.get(*variable).expect("complete physical binding"));
            }
            for &(left, right) in &self.inequalities {
                control(GlaExecutionEvent::Work)?;
                if slots[left] == slots[right] {
                    return Ok(());
                }
            }
            let mut emit = |control: &mut C| {
                control(GlaExecutionEvent::Work)?;
                Row::collect_properties(
                    &operators[self.projection],
                    &slots[..self.slots.len()],
                    &mut projected,
                    &mut |_, _| -> Result<Option<&CanonicalScalar>, E> {
                        unreachable!("the selected projection contains only vertex identities")
                    },
                    control,
                )
            };
            if self.distinct {
                emit(control)?;
            } else if let Some(capacity) = self.page_capacity {
                // Only a retention bound, NEVER an estimated cardinality:
                // extra identical occurrences cannot improve a full top-k.
                let copies = binding
                    .multiplicity_factors()
                    .iter()
                    .fold(1_u128, |n, &weight| {
                        n.checked_mul(weight as u128)
                            .unwrap_or(u128::from(capacity))
                            .min(u128::from(capacity))
                    });
                for _ in 0..copies {
                    emit(control)?;
                }
            } else {
                // Do not multiply unbounded factors into a finite counter.
                binding.for_each_occurrence(control, |_, control| emit(control))?;
            }
            Ok(())
        })?;
        let Some(GlaOperator::Limit { offset, count }) = operators.last() else {
            unreachable!()
        };
        let offset = usize::try_from(*offset).unwrap_or(usize::MAX);
        let count = count
            .and_then(|n| usize::try_from(n).ok())
            .unwrap_or(usize::MAX);
        let mut value = Vec::new();
        for row in projected.into_rows().skip(offset).take(count) {
            control(GlaExecutionEvent::ResultRow)?;
            value.push(row.retain_visible(logical.visible_columns));
        }
        Ok(value)
    }
}
