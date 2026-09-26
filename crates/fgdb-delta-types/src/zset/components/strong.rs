//! Exact directed strong components over changing vertex sets and edge bags.
//!
//! The existing weak-component maintainer supplies conservative affected
//! regions. A directed support change rederives both endpoints' OLD weak
//! components plus new vertices, even when opposite updates cancel in the
//! undirected projection. No directed cycle can cross that region boundary:
//! every old edge stays within an old weak component and both endpoints of
//! every new edge are included. Unrelated regions are never scanned or copied.
//!
//! Two iterative DFS passes classify the prospective region. This is bounded
//! region-local recomputation, not a fully dynamic SCC complexity claim. Space
//! is linear in support, not a retained all-pairs reachability relation. Keys,
//! allocation and infallible publication have the parent Z-set panic boundary.

use super::super::event;
use super::{
    ComponentError, ComponentUpdate, IncrementalComponents, Relation, Weights, insert, link,
    present, unlink,
};
use crate::{LimbLimit, ZSet, ZSetEvent, ZWeight};
use std::collections::{BTreeMap, BTreeSet};

#[derive(PartialEq, Eq)]
struct Directed<V: Ord> {
    edges: ZSet<(V, V)>,
    outgoing: Relation<V>,
    incoming: Relation<V>,
}
#[derive(PartialEq, Eq)]
struct Membership<V: Ord> {
    labels: BTreeMap<V, V>,
    count: usize,
}

/// One weight-one `(vertex, minimum member)` row for every live vertex.
/// Isolated vertices are singleton SCCs; parallel edges retain exact counts.
#[derive(PartialEq, Eq)]
pub struct IncrementalStrongComponents<V: Ord> {
    weak: IncrementalComponents<V>,
    directed: Directed<V>,
    membership: Membership<V>,
}
impl<V: Ord> Default for IncrementalStrongComponents<V> {
    fn default() -> Self {
        Self::new()
    }
}
impl<V: Ord> IncrementalStrongComponents<V> {
    pub fn new() -> Self {
        Self {
            weak: IncrementalComponents::new(),
            directed: Directed {
                edges: ZSet::new(),
                outgoing: Relation::new(),
                incoming: Relation::new(),
            },
            membership: Membership {
                labels: BTreeMap::new(),
                count: 0,
            },
        }
    }
    pub fn vertex_count(&self) -> usize {
        self.membership.labels.len()
    }
    pub fn component_count(&self) -> usize {
        self.membership.count
    }
    pub fn representative(&self, vertex: &V) -> Option<&V> {
        self.membership.labels.get(vertex)
    }
    pub fn pairs(&self) -> impl Iterator<Item = (&V, &V)> {
        self.membership.labels.iter()
    }
}
impl<V: Ord> core::fmt::Debug for IncrementalStrongComponents<V> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("IncrementalStrongComponents")
            .field("vertices", &self.vertex_count())
            .field("components", &self.component_count())
            .field("data", &"[REDACTED]")
            .finish()
    }
}

struct Changes<V: Ord> {
    added: Relation<V>,
    removed: Relation<V>,
    added_reverse: Relation<V>,
    removed_reverse: Relation<V>,
}
struct Prospective<'a, V: Ord> {
    base: &'a Directed<V>,
    changes: &'a Changes<V>,
}
impl<V: Ord> Prospective<'_, V> {
    // Yield removed entries too: the caller must charge their inspection.
    fn neighbors<'a>(
        &'a self,
        vertex: &'a V,
        reverse: bool,
    ) -> impl Iterator<Item = (&'a V, bool)> {
        let (base, added, removed) = if reverse {
            (
                &self.base.incoming,
                &self.changes.added_reverse,
                &self.changes.removed_reverse,
            )
        } else {
            (
                &self.base.outgoing,
                &self.changes.added,
                &self.changes.removed,
            )
        };
        base.get(vertex)
            .into_iter()
            .flatten()
            .chain(added.get(vertex).into_iter().flatten())
            .map(move |neighbor| {
                (
                    neighbor,
                    removed
                        .get(vertex)
                        .is_some_and(|row| row.contains(neighbor)),
                )
            })
    }
}
impl<V: Ord + Clone> Prospective<'_, V> {
    fn classify<E>(
        &self,
        live: &BTreeSet<V>,
        control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
    ) -> Result<Membership<V>, ComponentError<E>> {
        let mut seen = BTreeSet::new();
        let mut finished = Vec::new();
        let mut stack = Vec::new();
        for root in live {
            event(control, ZSetEvent::Work)?;
            if seen.contains(root) {
                continue;
            }
            event(control, ZSetEvent::ScratchEntry)?;
            stack.push((root.clone(), false));
            while let Some((vertex, exit)) = stack.pop() {
                event(control, ZSetEvent::Work)?;
                if exit {
                    event(control, ZSetEvent::ScratchEntry)?;
                    finished.push(vertex);
                    continue;
                }
                if seen.contains(&vertex) {
                    continue;
                }
                insert(&mut seen, &vertex, control)?;
                event(control, ZSetEvent::ScratchEntry)?;
                stack.push((vertex.clone(), true));
                for (neighbor, removed) in self.neighbors(&vertex, false) {
                    event(control, ZSetEvent::Work)?;
                    if removed {
                        continue;
                    }
                    if !live.contains(neighbor) {
                        return Err(ComponentError::MissingEndpoint);
                    }
                    if !seen.contains(neighbor) {
                        event(control, ZSetEvent::ScratchEntry)?;
                        stack.push((neighbor.clone(), false));
                    }
                }
            }
        }
        let mut assigned = BTreeSet::new();
        let mut labels = BTreeMap::new();
        let mut count = 0usize;
        while let Some(root) = finished.pop() {
            event(control, ZSetEvent::Work)?;
            if assigned.contains(&root) {
                continue;
            }
            let mut group = BTreeSet::new();
            event(control, ZSetEvent::ScratchEntry)?;
            let mut pending = vec![root];
            while let Some(vertex) = pending.pop() {
                event(control, ZSetEvent::Work)?;
                if assigned.contains(&vertex) {
                    continue;
                }
                insert(&mut assigned, &vertex, control)?;
                insert(&mut group, &vertex, control)?;
                for (neighbor, removed) in self.neighbors(&vertex, true) {
                    event(control, ZSetEvent::Work)?;
                    if removed {
                        continue;
                    }
                    if !live.contains(neighbor) {
                        return Err(ComponentError::MissingEndpoint);
                    }
                    if !assigned.contains(neighbor) {
                        event(control, ZSetEvent::ScratchEntry)?;
                        pending.push(neighbor.clone());
                    }
                }
            }
            let representative = group
                .first()
                .expect("unassigned root enters its SCC")
                .clone();
            for vertex in group {
                event(control, ZSetEvent::Work)?;
                // Private label plus eventual retained label.
                event(control, ZSetEvent::ScratchEntry)?;
                event(control, ZSetEvent::ScratchEntry)?;
                labels.insert(vertex, representative.clone());
            }
            count = count
                .checked_add(1)
                .ok_or(ComponentError::CardinalityOverflow)?;
        }
        Ok(Membership { labels, count })
    }
}

impl<V: Ord + Clone> IncrementalStrongComponents<V> {
    /// Prepare an exact derivative without modifying any accepted arrangement.
    /// Directional counts are checked BEFORE their weak projection: a negative
    /// forward count cannot be concealed by positive reverse support. Vertex
    /// deletions must retract all incident support in the same complete tick.
    /// Dropping the prepared update, cancellation or invalid input changes none
    /// of the weak index, directed topology or SCC membership.
    pub fn prepare<E>(
        &mut self,
        vertex_delta: &ZSet<V>,
        edge_delta: &ZSet<(V, V)>,
        limbs: LimbLimit,
        control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
    ) -> Result<StrongComponentUpdate<'_, V>, ComponentError<E>> {
        event(control, ZSetEvent::Work)?;
        let edges = self
            .directed
            .edges
            .prepare_integration(edge_delta, limbs, control)?;
        let mut changes = Changes {
            added: Relation::new(),
            removed: Relation::new(),
            added_reverse: Relation::new(),
            removed_reverse: Relation::new(),
        };
        let mut touched = BTreeSet::new();
        for (vertex, _) in vertex_delta.iter() {
            insert(&mut touched, vertex, control)?;
        }
        for ((a, b), weight) in &edges {
            event(control, ZSetEvent::Work)?;
            if weight < &ZWeight::ZERO {
                return Err(ComponentError::NegativeEdgeMultiplicity);
            }
            let live = !weight.is_zero();
            if live
                == self
                    .directed
                    .edges
                    .weight(&(a.clone(), b.clone()))
                    .is_some()
            {
                continue;
            }
            insert(&mut touched, a, control)?;
            insert(&mut touched, b, control)?;
            let (forward, reverse) = if live {
                (&mut changes.added, &mut changes.added_reverse)
            } else {
                (&mut changes.removed, &mut changes.removed_reverse)
            };
            if live {
                event(control, ZSetEvent::ScratchEntry)?;
            }
            link(forward, a, b, control)?;
            link(reverse, b, a, control)?;
        }
        let mut roots = BTreeSet::new();
        for vertex in &touched {
            event(control, ZSetEvent::Work)?;
            if let Some(root) = self.weak.representative(vertex) {
                insert(&mut roots, root, control)?;
            }
        }
        let mut candidates = touched;
        for root in roots {
            event(control, ZSetEvent::Work)?;
            for vertex in &self.weak.members[&root] {
                insert(&mut candidates, vertex, control)?;
            }
        }
        let weak = self
            .weak
            .prepare(vertex_delta, edge_delta, limbs, control)?;
        let mut live = BTreeSet::new();
        let mut old_roots = BTreeSet::new();
        for vertex in &candidates {
            event(control, ZSetEvent::Work)?;
            if present(&weak.owner.vertices, &weak.vertices, vertex) {
                insert(&mut live, vertex, control)?;
            }
            if let Some(root) = self.membership.labels.get(vertex) {
                insert(&mut old_roots, root, control)?;
            }
        }
        let next = Prospective {
            base: &self.directed,
            changes: &changes,
        }
        .classify(&live, control)?;
        let count = self
            .membership
            .count
            .checked_sub(old_roots.len())
            .and_then(|previous| previous.checked_add(next.count))
            .ok_or(ComponentError::CardinalityOverflow)?;
        let mut delta = ZSet::new();
        for vertex in &candidates {
            event(control, ZSetEvent::Work)?;
            let (before, after) = (self.membership.labels.get(vertex), next.labels.get(vertex));
            if before == after {
                continue;
            }
            if let Some(root) = before {
                delta.accumulate(
                    (vertex.clone(), root.clone()),
                    ZWeight::from_i128(-1),
                    limbs,
                    control,
                )?;
            }
            if let Some(root) = after {
                delta.accumulate((vertex.clone(), root.clone()), ZWeight::ONE, limbs, control)?;
            }
        }
        event(control, ZSetEvent::Work)?;
        Ok(StrongComponentUpdate {
            weak,
            directed: &mut self.directed,
            membership: &mut self.membership,
            edges,
            changes,
            candidates,
            labels: next.labels,
            count,
            delta,
        })
    }
    pub fn apply<E>(
        &mut self,
        vertices: &ZSet<V>,
        edges: &ZSet<(V, V)>,
        limbs: LimbLimit,
        control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
    ) -> Result<ZSet<(V, V)>, ComponentError<E>> {
        Ok(self.prepare(vertices, edges, limbs, control)?.commit())
    }
    /// Explicit full export; ordinary maintenance only publishes its derivative.
    pub fn snapshot<E>(
        &self,
        limbs: LimbLimit,
        control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
    ) -> Result<ZSet<(V, V)>, ComponentError<E>> {
        Ok(ZSet::from_updates(
            self.pairs()
                .map(|(v, root)| ((v.clone(), root.clone()), ZWeight::ONE)),
            limbs,
            control,
        )?)
    }
}

#[must_use = "dropping an SCC update aborts every input, index and output change"]
pub struct StrongComponentUpdate<'a, V: Ord> {
    weak: ComponentUpdate<'a, V>,
    directed: &'a mut Directed<V>,
    membership: &'a mut Membership<V>,
    edges: Weights<(V, V)>,
    changes: Changes<V>,
    candidates: BTreeSet<V>,
    labels: BTreeMap<V, V>,
    count: usize,
    delta: ZSet<(V, V)>,
}
impl<V: Ord> StrongComponentUpdate<'_, V> {
    pub fn delta(&self) -> &ZSet<(V, V)> {
        &self.delta
    }
    pub fn component_count(&self) -> usize {
        self.count
    }
    pub fn vertex_count(&self) -> usize {
        self.weak.vertex_count()
    }
    pub fn affected_vertices(&self) -> usize {
        self.candidates.len()
    }
}
impl<V: Ord + Clone> StrongComponentUpdate<'_, V> {
    /// Publish the admitted weak index, directed bag and membership together.
    /// No callback or recoverable arithmetic/refusal occurs during publication.
    pub fn commit(self) -> ZSet<(V, V)> {
        let Self {
            weak,
            directed,
            membership,
            edges,
            changes,
            candidates,
            labels,
            count,
            delta,
        } = self;
        let _ = weak.commit();
        directed.edges.publish(edges);
        for (a, row) in changes.removed {
            for b in row {
                unlink(&mut directed.outgoing, &a, &b);
                unlink(&mut directed.incoming, &b, &a);
            }
        }
        for (a, row) in changes.added {
            for b in row {
                directed
                    .incoming
                    .entry(b.clone())
                    .or_default()
                    .insert(a.clone());
                directed.outgoing.entry(a.clone()).or_default().insert(b);
            }
        }
        for vertex in candidates {
            membership.labels.remove(&vertex);
        }
        membership.labels.extend(labels);
        membership.count = count;
        delta
    }
}
impl<V: Ord> core::fmt::Debug for StrongComponentUpdate<'_, V> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("StrongComponentUpdate")
            .field("affected_vertices", &self.affected_vertices())
            .field("delta_support", &self.delta.len())
            .field("data", &"[REDACTED]")
            .finish()
    }
}

#[cfg(test)]
mod tests;
