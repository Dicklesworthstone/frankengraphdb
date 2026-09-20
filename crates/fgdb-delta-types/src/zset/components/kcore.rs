//! Exact core numbers over the component maintainer's prospective topology.
//!
//! A vertex's core number is the greatest k for which it belongs to the k-core
//! of the undirected SIMPLE support graph. Parallel/opposite edges count as one
//! neighbor and self-loops count as none. Every live isolate has core number 0.
//! This projection is explicit; these are not multigraph or directed cores.
//!
//! The existing component guard validates vertices, endpoints, multiplicities
//! and cascades, and identifies complete affected components. This operator
//! borrows that guard's adjacency rather than retaining a second topology or
//! cloning unrelated components. Degree peeling rederives only that affected
//! region; a change in a giant component can still traverse the whole component.
//! Ordered degree queues give deterministic results with logarithmic queue
//! operations, not the linear-time bucket or specialized dynamic-core bound.
//! Work/scratch count logical events, not bytes or arbitrary key comparison/
//! cloning costs. Standard allocation/panic has the parent Z-set boundary.

use super::{ComponentError, ComponentUpdate, IncrementalComponents};
use crate::zset::{ZSet, ZSetError, ZSetEvent, event};
use crate::{LimbLimit, ZWeight};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CoreError<E> {
    Topology(ComponentError<E>),
    Delta(ZSetError<E>),
    DegreeOverflow,
    InconsistentTopology,
}
impl<E> From<ComponentError<E>> for CoreError<E> {
    fn from(error: ComponentError<E>) -> Self {
        Self::Topology(error)
    }
}
impl<E> From<ZSetError<E>> for CoreError<E> {
    fn from(error: ZSetError<E>) -> Self {
        Self::Delta(error)
    }
}
impl<E: core::fmt::Display> core::fmt::Display for CoreError<E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Topology(error) => error.fmt(f),
            Self::Delta(error) => error.fmt(f),
            Self::DegreeOverflow => f.write_str("core degree exceeds u64"),
            Self::InconsistentTopology => f.write_str("inconsistent prospective core topology"),
        }
    }
}
impl<E: core::error::Error + 'static> core::error::Error for CoreError<E> {}

/// The topology has one owner. Only per-vertex core numbers are retained here;
/// a single accepted tick publishes both the component and core arrangements.
#[derive(PartialEq, Eq)]
pub struct IncrementalCoreNumbers<V: Ord> {
    topology: IncrementalComponents<V>,
    numbers: BTreeMap<V, u64>,
}
impl<V: Ord> Default for IncrementalCoreNumbers<V> {
    fn default() -> Self {
        Self::new()
    }
}
impl<V: Ord> IncrementalCoreNumbers<V> {
    pub fn new() -> Self {
        Self {
            topology: IncrementalComponents::new(),
            numbers: BTreeMap::new(),
        }
    }
    pub fn vertex_count(&self) -> usize {
        self.numbers.len()
    }
    /// None means non-live; Some(0) is a live vertex without a nontrivial core.
    pub fn core_number(&self, vertex: &V) -> Option<u64> {
        self.numbers.get(vertex).copied()
    }
    pub fn numbers(&self) -> impl DoubleEndedIterator<Item = (&V, &u64)> + ExactSizeIterator {
        self.numbers.iter()
    }
}
impl<V: Ord> core::fmt::Debug for IncrementalCoreNumbers<V> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("IncrementalCoreNumbers")
            .field("vertices", &self.vertex_count())
            .field("data", &"[REDACTED]")
            .finish()
    }
}

// Old and newly inserted support are disjoint by the component compiler's
// transition law. Retractions are tested AFTER charging each candidate visit;
// a filter iterator must not hide arbitrarily many removed edges from control.
fn adjacent<'a, V: Ord>(
    topology: &'a ComponentUpdate<'_, V>,
    vertex: &V,
) -> impl Iterator<Item = &'a V> {
    topology
        .owner
        .neighbors
        .get(vertex)
        .into_iter()
        .flatten()
        .chain(topology.inserted.get(vertex).into_iter().flatten())
}
fn retained<V: Ord>(topology: &ComponentUpdate<'_, V>, vertex: &V, neighbor: &V) -> bool {
    vertex != neighbor
        && !topology
            .removed
            .get(vertex)
            .is_some_and(|row| row.contains(neighbor))
}

fn peel<V: Ord + Clone, E>(
    topology: &ComponentUpdate<'_, V>,
    control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
) -> Result<BTreeMap<V, u64>, CoreError<E>> {
    let mut degrees = BTreeMap::<V, u64>::new();
    let mut queue = BTreeSet::<(u64, V)>::new();
    for vertex in topology.labels.keys() {
        event(control, ZSetEvent::Work)?;
        let mut degree = 0_u64;
        for neighbor in adjacent(topology, vertex) {
            event(control, ZSetEvent::Work)?;
            if !retained(topology, vertex, neighbor) {
                continue;
            }
            // Affected regions are complete components of the final support.
            if !topology.labels.contains_key(neighbor) {
                return Err(CoreError::InconsistentTopology);
            }
            degree = degree.checked_add(1).ok_or(CoreError::DegreeOverflow)?;
        }
        event(control, ZSetEvent::ScratchEntry)?;
        degrees.insert(vertex.clone(), degree);
        event(control, ZSetEvent::ScratchEntry)?;
        queue.insert((degree, vertex.clone()));
    }
    let mut numbers = BTreeMap::new();
    while !queue.is_empty() {
        event(control, ZSetEvent::Work)?;
        let (degree, vertex) = queue.pop_first().expect("nonempty core queue");
        if degrees.remove(&vertex) != Some(degree) {
            return Err(CoreError::InconsistentTopology);
        }
        // Reserve both the private result and eventual retained map entry.
        event(control, ZSetEvent::ScratchEntry)?;
        event(control, ZSetEvent::ScratchEntry)?;
        for neighbor in adjacent(topology, &vertex) {
            event(control, ZSetEvent::Work)?;
            if !retained(topology, &vertex, neighbor) {
                continue;
            }
            if let Some(old) = degrees.get_mut(neighbor) {
                // Do NOT decrement a neighbor already at this shell's degree.
                // Otherwise a triangle would be mislabeled 2,1,0 instead of 2,2,2.
                if *old > degree {
                    event(control, ZSetEvent::Work)?;
                    event(control, ZSetEvent::ScratchEntry)?;
                    if !queue.remove(&(*old, neighbor.clone())) {
                        return Err(CoreError::InconsistentTopology);
                    }
                    *old -= 1;
                    queue.insert((*old, neighbor.clone()));
                }
            }
        }
        numbers.insert(vertex, degree);
    }
    Ok(numbers)
}

impl<V: Ord + Clone> IncrementalCoreNumbers<V> {
    /// Prepare one complete vertex/edge tick. The component guard and every
    /// peeling/output change remain tentative until the returned guard commits.
    /// Multiplicity-only ticks have no affected region and do not peel a core.
    pub fn prepare<E>(
        &mut self,
        vertices: &ZSet<V>,
        edges: &ZSet<(V, V)>,
        limbs: LimbLimit,
        control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
    ) -> Result<CoreUpdate<'_, V>, CoreError<E>> {
        let topology = self.topology.prepare(vertices, edges, limbs, control)?;
        let replacements = peel(&topology, control)?;
        let mut delta = ZSet::new();
        for vertex in &topology.candidates {
            event(control, ZSetEvent::Work)?;
            let old = self.numbers.get(vertex);
            let next = replacements.get(vertex);
            if old == next {
                continue;
            }
            if let Some(number) = old {
                delta.accumulate(
                    (vertex.clone(), *number),
                    ZWeight::from_i128(-1),
                    limbs,
                    control,
                )?;
            }
            if let Some(number) = next {
                delta.accumulate((vertex.clone(), *number), ZWeight::ONE, limbs, control)?;
            }
        }
        event(control, ZSetEvent::Work)?;
        Ok(CoreUpdate {
            topology,
            numbers: &mut self.numbers,
            replacements,
            delta,
        })
    }
    pub fn apply<E>(
        &mut self,
        vertices: &ZSet<V>,
        edges: &ZSet<(V, V)>,
        limbs: LimbLimit,
        control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
    ) -> Result<ZSet<(V, u64)>, CoreError<E>> {
        Ok(self.prepare(vertices, edges, limbs, control)?.commit())
    }
    /// Explicit full export; ordinary maintenance never scans this result map.
    pub fn snapshot<E>(
        &self,
        limbs: LimbLimit,
        control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
    ) -> Result<ZSet<(V, u64)>, CoreError<E>> {
        Ok(ZSet::from_updates(
            self.numbers
                .iter()
                .map(|(v, n)| ((v.clone(), *n), ZWeight::ONE)),
            limbs,
            control,
        )?)
    }
}

#[must_use = "dropping a core update aborts topology, membership and core-number changes"]
pub struct CoreUpdate<'a, V: Ord> {
    topology: ComponentUpdate<'a, V>,
    numbers: &'a mut BTreeMap<V, u64>,
    replacements: BTreeMap<V, u64>,
    delta: ZSet<(V, u64)>,
}
impl<V: Ord> CoreUpdate<'_, V> {
    pub fn delta(&self) -> &ZSet<(V, u64)> {
        &self.delta
    }
    pub fn vertex_count(&self) -> usize {
        self.topology.vertex_count()
    }
    pub fn affected_vertices(&self) -> usize {
        self.topology.affected_vertices()
    }
    /// Inspect the prospective answer without accidentally reviving a retired
    /// vertex from the accepted map when its replacement is absent.
    pub fn core_number(&self, vertex: &V) -> Option<u64> {
        if self.topology.candidates.contains(vertex) {
            self.replacements.get(vertex).copied()
        } else {
            self.numbers.get(vertex).copied()
        }
    }
}
impl<V: Ord + Clone> CoreUpdate<'_, V> {
    /// Publish with no recoverable callback or arithmetic between participants.
    pub fn commit(self) -> ZSet<(V, u64)> {
        let Self {
            topology,
            numbers,
            replacements,
            delta,
        } = self;
        for vertex in &topology.candidates {
            numbers.remove(vertex);
        }
        numbers.extend(replacements);
        let _ = topology.commit();
        delta
    }
}
impl<V: Ord> core::fmt::Debug for CoreUpdate<'_, V> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("CoreUpdate")
            .field("affected_vertices", &self.affected_vertices())
            .field("delta_support", &self.delta.len())
            .field("data", &"[REDACTED]")
            .finish()
    }
}

#[cfg(test)]
mod tests;
