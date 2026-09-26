//! Exact weakly connected components with explicit isolated vertices.
//!
//! Vertices form a set; edges form a nonnegative bag whose orientations are
//! normalized before integration. The representative of a component is its
//! minimum vertex under the canonical key order, never an allocation ordinal.
//! A result contains one (vertex, representative) row of weight one per vertex.
//!
//! Support changes rederive the union of affected OLD components plus new
//! vertices on the complete prospective topology. Other components are neither
//! scanned nor copied. Multiplicity-only edge changes do not traverse topology.
//! This is bounded component-local recomputation, not union-find complexity:
//! deleting one edge in a giant component can still visit that whole component.
//! Storage is linear in vertex/edge support, not a quadratic reachability table.
//! Logical work/scratch admission is not a byte bound; arbitrary key code and
//! standard allocation retain the parent Z-set panic/allocation boundary.

pub mod kcore;
pub mod strong;

use super::{ZSet, ZSetError, ZSetEvent, event};
use crate::{LimbLimit, ZWeight};
use std::collections::{BTreeMap, BTreeSet};

type Relation<V> = BTreeMap<V, BTreeSet<V>>;
type Weights<V> = BTreeMap<V, ZWeight>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ComponentError<E> {
    Delta(ZSetError<E>),
    /// Integrated vertex membership must be exactly zero or one.
    VertexMultiplicity,
    NegativeEdgeMultiplicity,
    /// Every supported edge must have two live vertices after the WHOLE tick.
    MissingEndpoint,
    CardinalityOverflow,
}
impl<E> From<ZSetError<E>> for ComponentError<E> {
    fn from(error: ZSetError<E>) -> Self {
        Self::Delta(error)
    }
}
impl<E: core::fmt::Display> core::fmt::Display for ComponentError<E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Delta(error) => error.fmt(f),
            Self::VertexMultiplicity => {
                f.write_str("component vertex membership is not zero or one")
            }
            Self::NegativeEdgeMultiplicity => f.write_str("negative component edge multiplicity"),
            Self::MissingEndpoint => f.write_str("component edge has a missing final endpoint"),
            Self::CardinalityOverflow => f.write_str("component cardinality overflow"),
        }
    }
}
impl<E: core::error::Error + 'static> core::error::Error for ComponentError<E> {}

#[derive(PartialEq, Eq)]
pub struct IncrementalComponents<V: Ord> {
    vertices: ZSet<V>,
    edges: ZSet<(V, V)>,
    neighbors: Relation<V>,
    labels: BTreeMap<V, V>,
    members: Relation<V>,
}
impl<V: Ord> Default for IncrementalComponents<V> {
    fn default() -> Self {
        Self::new()
    }
}
impl<V: Ord> IncrementalComponents<V> {
    pub fn new() -> Self {
        Self {
            vertices: ZSet::new(),
            edges: ZSet::new(),
            neighbors: Relation::new(),
            labels: BTreeMap::new(),
            members: Relation::new(),
        }
    }
    pub fn vertex_count(&self) -> usize {
        self.labels.len()
    }
    pub fn component_count(&self) -> usize {
        self.members.len()
    }
    pub fn representative(&self, vertex: &V) -> Option<&V> {
        self.labels.get(vertex)
    }
    pub fn pairs(&self) -> impl Iterator<Item = (&V, &V)> {
        self.labels.iter()
    }
}
impl<V: Ord> core::fmt::Debug for IncrementalComponents<V> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("IncrementalComponents")
            .field("vertices", &self.vertex_count())
            .field("components", &self.component_count())
            .field("data", &"[REDACTED]")
            .finish()
    }
}

fn canonical<V: Ord + Clone>(a: &V, b: &V) -> (V, V) {
    if a <= b {
        (a.clone(), b.clone())
    } else {
        (b.clone(), a.clone())
    }
}
fn insert<V: Ord + Clone, E>(
    set: &mut BTreeSet<V>,
    value: &V,
    control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
) -> Result<(), ZSetError<E>> {
    event(control, ZSetEvent::Work)?;
    if !set.contains(value) {
        event(control, ZSetEvent::ScratchEntry)?;
        set.insert(value.clone());
    }
    Ok(())
}
fn link<V: Ord + Clone, E>(
    rows: &mut Relation<V>,
    a: &V,
    b: &V,
    control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
) -> Result<(), ZSetError<E>> {
    event(control, ZSetEvent::Work)?;
    // Stage group/member and conservatively reserve eventual retained entries.
    for _ in 0..4 {
        event(control, ZSetEvent::ScratchEntry)?;
    }
    rows.entry(a.clone()).or_default().insert(b.clone());
    Ok(())
}
fn unlink<V: Ord>(rows: &mut Relation<V>, a: &V, b: &V) {
    if let Some(row) = rows.get_mut(a) {
        row.remove(b);
        if row.is_empty() {
            rows.remove(a);
        }
    }
}
fn present<V: Ord>(base: &ZSet<V>, next: &Weights<V>, vertex: &V) -> bool {
    next.get(vertex)
        .or_else(|| base.weight(vertex))
        .is_some_and(|weight| !weight.is_zero())
}

impl<V: Ord + Clone> IncrementalComponents<V> {
    /// Prepare both input deltas and the exact membership derivative together.
    /// Vertex retirement must include all incident edge retractions in this
    /// same tick. A partial parallel-edge retraction cannot retire its endpoint.
    /// Neither invalid input nor a control refusal changes accepted state.
    pub fn prepare<E>(
        &mut self,
        vertex_delta: &ZSet<V>,
        edge_delta: &ZSet<(V, V)>,
        limbs: LimbLimit,
        control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
    ) -> Result<ComponentUpdate<'_, V>, ComponentError<E>> {
        event(control, ZSetEvent::Work)?;
        let vertices = self
            .vertices
            .prepare_integration(vertex_delta, limbs, control)?;
        let normalized = edge_delta.map(|(a, b)| Ok(canonical(a, b)), limbs, control)?;
        let edges = self
            .edges
            .prepare_integration(&normalized, limbs, control)?;
        let mut touched = BTreeSet::new();
        for (vertex, weight) in &vertices {
            event(control, ZSetEvent::Work)?;
            if !weight.is_zero() && weight != &ZWeight::ONE {
                return Err(ComponentError::VertexMultiplicity);
            }
            if self.vertices.weight(vertex).is_some() != !weight.is_zero() {
                insert(&mut touched, vertex, control)?;
                if !weight.is_zero() {
                    event(control, ZSetEvent::ScratchEntry)?;
                }
            }
            if weight.is_zero() {
                // Unchanged incident support would otherwise disappear from a
                // component silently while remaining in the input arrangement.
                for neighbor in self.neighbors.get(vertex).into_iter().flatten() {
                    event(control, ZSetEvent::Work)?;
                    if !edges
                        .get(&canonical(vertex, neighbor))
                        .is_some_and(ZWeight::is_zero)
                    {
                        return Err(ComponentError::MissingEndpoint);
                    }
                }
            }
        }
        let mut inserted = Relation::new();
        let mut removed = Relation::new();
        for ((a, b), weight) in &edges {
            event(control, ZSetEvent::Work)?;
            if weight < &ZWeight::ZERO {
                return Err(ComponentError::NegativeEdgeMultiplicity);
            }
            let live = !weight.is_zero();
            if live
                && (!present(&self.vertices, &vertices, a)
                    || !present(&self.vertices, &vertices, b))
            {
                return Err(ComponentError::MissingEndpoint);
            }
            if live == self.edges.weight(&(a.clone(), b.clone())).is_some() {
                continue;
            }
            insert(&mut touched, a, control)?;
            insert(&mut touched, b, control)?;
            let changed = if live { &mut inserted } else { &mut removed };
            link(changed, a, b, control)?;
            if a != b {
                link(changed, b, a, control)?;
            }
            if live {
                event(control, ZSetEvent::ScratchEntry)?;
            }
        }
        let mut roots = BTreeSet::new();
        for vertex in &touched {
            event(control, ZSetEvent::Work)?;
            if let Some(root) = self.labels.get(vertex) {
                insert(&mut roots, root, control)?;
            }
        }
        let mut candidates = touched;
        for root in &roots {
            event(control, ZSetEvent::Work)?;
            for vertex in &self.members[root] {
                insert(&mut candidates, vertex, control)?;
            }
        }
        let mut remaining = BTreeSet::new();
        let mut old_vertices = 0usize;
        for vertex in &candidates {
            event(control, ZSetEvent::Work)?;
            if self.labels.contains_key(vertex) {
                old_vertices = old_vertices
                    .checked_add(1)
                    .ok_or(ComponentError::CardinalityOverflow)?;
            }
            if present(&self.vertices, &vertices, vertex) {
                insert(&mut remaining, vertex, control)?;
            }
        }
        let mut labels = BTreeMap::new();
        let mut members = Relation::new();
        while let Some(root) = remaining.pop_first() {
            event(control, ZSetEvent::Work)?;
            event(control, ZSetEvent::ScratchEntry)?;
            let mut pending = vec![root.clone()];
            let mut group = BTreeSet::new();
            while let Some(vertex) = pending.pop() {
                event(control, ZSetEvent::Work)?;
                // Private and eventual published label entries are separate.
                for _ in 0..2 {
                    event(control, ZSetEvent::ScratchEntry)?;
                }
                labels.insert(vertex.clone(), root.clone());
                insert(&mut group, &vertex, control)?;
                for neighbor in self
                    .neighbors
                    .get(&vertex)
                    .into_iter()
                    .flatten()
                    .chain(inserted.get(&vertex).into_iter().flatten())
                {
                    event(control, ZSetEvent::Work)?;
                    if removed
                        .get(&vertex)
                        .is_some_and(|row| row.contains(neighbor))
                    {
                        continue;
                    }
                    if remaining.remove(neighbor) {
                        event(control, ZSetEvent::ScratchEntry)?;
                        pending.push(neighbor.clone());
                    }
                }
            }
            event(control, ZSetEvent::ScratchEntry)?;
            members.insert(root, group);
        }
        let count = self
            .members
            .len()
            .checked_sub(roots.len())
            .and_then(|old| old.checked_add(members.len()))
            .ok_or(ComponentError::CardinalityOverflow)?;
        let vertex_count = self
            .labels
            .len()
            .checked_sub(old_vertices)
            .and_then(|old| old.checked_add(labels.len()))
            .ok_or(ComponentError::CardinalityOverflow)?;
        let mut delta = ZSet::new();
        for vertex in &candidates {
            event(control, ZSetEvent::Work)?;
            let (old, next) = (self.labels.get(vertex), labels.get(vertex));
            if old == next {
                continue;
            }
            if let Some(root) = old {
                delta.accumulate(
                    (vertex.clone(), root.clone()),
                    ZWeight::from_i128(-1),
                    limbs,
                    control,
                )?;
            }
            if let Some(root) = next {
                delta.accumulate((vertex.clone(), root.clone()), ZWeight::ONE, limbs, control)?;
            }
        }
        event(control, ZSetEvent::Work)?;
        Ok(ComponentUpdate {
            owner: self,
            vertices,
            edges,
            inserted,
            removed,
            roots,
            candidates,
            labels,
            members,
            count,
            vertex_count,
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

    /// Explicit complete result export. Ordinary maintenance does not use it.
    pub fn snapshot<E>(
        &self,
        limbs: LimbLimit,
        control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
    ) -> Result<ZSet<(V, V)>, ComponentError<E>> {
        Ok(ZSet::from_updates(
            self.labels
                .iter()
                .map(|(v, root)| ((v.clone(), root.clone()), ZWeight::ONE)),
            limbs,
            control,
        )?)
    }
}

#[must_use = "dropping a component update aborts both input and output changes"]
pub struct ComponentUpdate<'a, V: Ord> {
    owner: &'a mut IncrementalComponents<V>,
    vertices: Weights<V>,
    edges: Weights<(V, V)>,
    inserted: Relation<V>,
    removed: Relation<V>,
    roots: BTreeSet<V>,
    candidates: BTreeSet<V>,
    labels: BTreeMap<V, V>,
    members: Relation<V>,
    count: usize,
    vertex_count: usize,
    delta: ZSet<(V, V)>,
}
impl<V: Ord> ComponentUpdate<'_, V> {
    pub fn delta(&self) -> &ZSet<(V, V)> {
        &self.delta
    }
    pub fn component_count(&self) -> usize {
        self.count
    }
    pub fn vertex_count(&self) -> usize {
        self.vertex_count
    }
    pub fn affected_vertices(&self) -> usize {
        self.candidates.len()
    }
}
impl<V: Ord + Clone> ComponentUpdate<'_, V> {
    /// No arithmetic, caller callback or other recoverable refusal occurs
    /// between accepted input, arrangement and membership publication.
    pub fn commit(self) -> ZSet<(V, V)> {
        let Self {
            owner,
            vertices,
            edges,
            inserted,
            removed,
            roots,
            candidates,
            labels,
            members,
            delta,
            ..
        } = self;
        owner.vertices.publish(vertices);
        owner.edges.publish(edges);
        for (a, row) in removed {
            for b in row {
                unlink(&mut owner.neighbors, &a, &b);
            }
        }
        for (a, row) in inserted {
            owner.neighbors.entry(a).or_default().extend(row);
        }
        for root in roots {
            owner.members.remove(&root);
        }
        for vertex in candidates {
            owner.labels.remove(&vertex);
        }
        owner.labels.extend(labels);
        owner.members.extend(members);
        delta
    }
}
impl<V: Ord> core::fmt::Debug for ComponentUpdate<'_, V> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ComponentUpdate")
            .field("affected_vertices", &self.affected_vertices())
            .field("delta_support", &self.delta.len())
            .field("data", &"[REDACTED]")
            .finish()
    }
}

#[cfg(test)]
mod tests;
