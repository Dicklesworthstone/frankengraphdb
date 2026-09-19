//! Deletion-safe, incremental, non-reflexive transitive closure of a finite bag.
//!
//! Edge multiplicities are integrated exactly and must remain nonnegative.
//! Positive support, rather than path counts, defines reachability: cycles do
//! not create infinite weights and parallel edges survive partial retraction.
//! Only sources preceding a changed edge in the OLD closure need rederivation.
//! Recomputing those sources on the complete prospective topology handles both
//! cycles and simultaneous changes without self-supporting deleted paths.
//!
//! This is an in-memory algebra operator, not durable arrangement storage or a
//! subscription scheduler. Closure storage can be quadratic. Work/scratch
//! controls admit logical entries, not allocator bytes; arbitrary key cloning,
//! comparison, allocation failure and panic have the parent Z-set boundary.

pub mod committed;

use super::{ZSet, ZSetError, ZSetEvent, event};
use crate::{LimbLimit, ZWeight};
use std::collections::{BTreeMap, BTreeSet, btree_map::Entry};

type Relation<V> = BTreeMap<V, BTreeSet<V>>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReachabilityError<E> {
    Delta(ZSetError<E>),
    /// A consolidated edge count would become negative. No state is changed.
    NegativeMultiplicity,
}

impl<E> From<ZSetError<E>> for ReachabilityError<E> {
    fn from(error: ZSetError<E>) -> Self {
        Self::Delta(error)
    }
}

impl<E: core::fmt::Display> core::fmt::Display for ReachabilityError<E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Delta(error) => error.fmt(f),
            Self::NegativeMultiplicity => {
                f.write_str("negative integrated reachability edge count")
            }
        }
    }
}
impl<E: core::error::Error + 'static> core::error::Error for ReachabilityError<E> {}

/// Exact reachability through ONE OR MORE edges. A self pair exists only when
/// there is a nonempty cycle; isolated vertices and zero-hop paths are absent.
/// Each reachable pair has weight one, regardless of its number of routes.
#[derive(PartialEq, Eq)]
pub struct IncrementalReachability<V: Ord> {
    edges: ZSet<(V, V)>,
    outgoing: Relation<V>,
    reachable: Relation<V>,
    predecessors: Relation<V>,
}

impl<V: Ord> Default for IncrementalReachability<V> {
    fn default() -> Self {
        Self::new()
    }
}

impl<V: Ord> IncrementalReachability<V> {
    pub fn new() -> Self {
        Self {
            edges: ZSet::new(),
            outgoing: BTreeMap::new(),
            reachable: BTreeMap::new(),
            predecessors: BTreeMap::new(),
        }
    }

    pub fn contains(&self, source: &V, destination: &V) -> bool {
        self.reachable
            .get(source)
            .is_some_and(|row| row.contains(destination))
    }

    pub fn edge_weight(&self, edge: &(V, V)) -> Option<&ZWeight> {
        self.edges.weight(edge)
    }

    /// Explicit ordered export, without constructing a second result relation.
    pub fn pairs(&self) -> impl Iterator<Item = (&V, &V)> {
        self.reachable
            .iter()
            .flat_map(|(source, row)| row.iter().map(move |destination| (source, destination)))
    }
}

impl<V: Ord> core::fmt::Debug for IncrementalReachability<V> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("IncrementalReachability")
            .field("edge_support", &self.edges.len())
            .field("source_count", &self.reachable.len())
            .field("data", &"[REDACTED]")
            .finish()
    }
}

impl<V: Ord + Clone> IncrementalReachability<V> {
    pub fn snapshot<E>(
        &self,
        limbs: LimbLimit,
        control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
    ) -> Result<ZSet<(V, V)>, ReachabilityError<E>> {
        let mut result = ZSet::new();
        for (source, destination) in self.pairs() {
            event(control, ZSetEvent::Work)?;
            result.accumulate(
                (source.clone(), destination.clone()),
                ZWeight::ONE,
                limbs,
                control,
            )?;
        }
        Ok(result)
    }

    /// Stage one consolidated edge delta. Existing arrangements remain intact
    /// until the returned guard commits; a control/arithmetic refusal or a
    /// dropped guard permits retry of exactly the same input.
    ///
    /// For each support-changing edge u->v, the affected sources are u and the
    /// OLD predecessors of u. For any changed path, its first changed edge has
    /// an unchanged old prefix, so this union is complete even when several
    /// insertions/deletions cooperate in a single tick. Other sources are not
    /// scanned. Multiplicity changes that preserve positive support need no
    /// graph traversal at all.
    pub fn prepare<E>(
        &mut self,
        delta: &ZSet<(V, V)>,
        limbs: LimbLimit,
        control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
    ) -> Result<ReachabilityUpdate<'_, V>, ReachabilityError<E>> {
        event(control, ZSetEvent::Work)?;
        let weights = self.edges.prepare_integration(delta, limbs, control)?;
        let mut inserted = Relation::new();
        let mut removed = Relation::new();
        let mut affected = BTreeSet::new();
        for (edge, next) in &weights {
            let (source, destination) = edge;
            event(control, ZSetEvent::Work)?;
            if next < &ZWeight::ZERO {
                return Err(ReachabilityError::NegativeMultiplicity);
            }
            let old_present = self.edges.weight(edge).is_some();
            let new_present = !next.is_zero();
            if old_present == new_present {
                continue;
            }
            if new_present {
                // Staging and eventual retained topology are separate entries.
                insert_pair(&mut inserted, source, destination, control)?;
                event(control, ZSetEvent::ScratchEntry)?;
                event(control, ZSetEvent::ScratchEntry)?;
            } else {
                insert_pair(&mut removed, source, destination, control)?;
            }
            insert_vertex(&mut affected, source, control)?;
            if let Some(predecessors) = self.predecessors.get(source) {
                for predecessor in predecessors {
                    insert_vertex(&mut affected, predecessor, control)?;
                }
            }
        }

        let mut replacements = Relation::new();
        let mut output = ZSet::new();
        for source in affected {
            event(control, ZSetEvent::Work)?;
            let next = self.rederive(&source, &inserted, &removed, control)?;
            let previous = self.reachable.get(&source);
            if let Some(previous) = previous {
                for destination in previous {
                    event(control, ZSetEvent::Work)?;
                    if !next.contains(destination) {
                        output.accumulate(
                            (source.clone(), destination.clone()),
                            ZWeight::from_i128(-1),
                            limbs,
                            control,
                        )?;
                    }
                }
            }
            for destination in &next {
                event(control, ZSetEvent::Work)?;
                if previous.is_none_or(|row| !row.contains(destination)) {
                    // Reserve the eventual reverse dependency entry/group.
                    event(control, ZSetEvent::ScratchEntry)?;
                    event(control, ZSetEvent::ScratchEntry)?;
                    output.accumulate(
                        (source.clone(), destination.clone()),
                        ZWeight::ONE,
                        limbs,
                        control,
                    )?;
                }
            }
            event(control, ZSetEvent::ScratchEntry)?;
            if previous.is_none() && !next.is_empty() {
                event(control, ZSetEvent::ScratchEntry)?;
            }
            replacements.insert(source, next);
        }
        event(control, ZSetEvent::Work)?;
        Ok(ReachabilityUpdate {
            owner: self,
            weights,
            inserted,
            removed,
            replacements,
            delta: output,
        })
    }

    pub fn apply<E>(
        &mut self,
        delta: &ZSet<(V, V)>,
        limbs: LimbLimit,
        control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
    ) -> Result<ZSet<(V, V)>, ReachabilityError<E>> {
        Ok(self.prepare(delta, limbs, control)?.commit())
    }

    fn rederive<E>(
        &self,
        source: &V,
        inserted: &Relation<V>,
        removed: &Relation<V>,
        control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
    ) -> Result<BTreeSet<V>, ZSetError<E>> {
        let mut reached = BTreeSet::new();
        event(control, ZSetEvent::ScratchEntry)?;
        let mut pending = vec![source.clone()];
        while let Some(vertex) = pending.pop() {
            event(control, ZSetEvent::Work)?;
            let deleted = removed.get(&vertex);
            for destination in self.outgoing.get(&vertex).into_iter().flatten() {
                event(control, ZSetEvent::Work)?;
                if deleted.is_none_or(|row| !row.contains(destination)) {
                    discover(source, destination, &mut reached, &mut pending, control)?;
                }
            }
            for destination in inserted.get(&vertex).into_iter().flatten() {
                event(control, ZSetEvent::Work)?;
                discover(source, destination, &mut reached, &mut pending, control)?;
            }
        }
        Ok(reached)
    }
}

fn insert_vertex<V: Ord + Clone, E>(
    vertices: &mut BTreeSet<V>,
    vertex: &V,
    control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
) -> Result<bool, ZSetError<E>> {
    event(control, ZSetEvent::Work)?;
    if vertices.contains(vertex) {
        return Ok(false);
    }
    event(control, ZSetEvent::ScratchEntry)?;
    vertices.insert(vertex.clone());
    Ok(true)
}

fn insert_pair<V: Ord + Clone, E>(
    relation: &mut Relation<V>,
    source: &V,
    destination: &V,
    control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
) -> Result<(), ZSetError<E>> {
    event(control, ZSetEvent::Work)?;
    let row = match relation.entry(source.clone()) {
        Entry::Vacant(entry) => {
            event(control, ZSetEvent::ScratchEntry)?;
            entry.insert(BTreeSet::new())
        }
        Entry::Occupied(entry) => entry.into_mut(),
    };
    insert_vertex(row, destination, control)?;
    Ok(())
}

fn discover<V: Ord + Clone, E>(
    source: &V,
    destination: &V,
    reached: &mut BTreeSet<V>,
    pending: &mut Vec<V>,
    control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
) -> Result<(), ZSetError<E>> {
    if insert_vertex(reached, destination, control)? && destination != source {
        event(control, ZSetEvent::ScratchEntry)?;
        pending.push(destination.clone());
    }
    // The root was expanded initially. A closing cycle records its self pair
    // but must not schedule that root again or invent zero-hop reachability.
    Ok(())
}

fn remove_pair<V: Ord>(relation: &mut Relation<V>, source: &V, destination: &V) {
    if let Some(row) = relation.get_mut(source) {
        row.remove(destination);
        if row.is_empty() {
            relation.remove(source);
        }
    }
}

#[must_use = "dropping a reachability update aborts it"]
pub struct ReachabilityUpdate<'a, V: Ord> {
    owner: &'a mut IncrementalReachability<V>,
    weights: BTreeMap<(V, V), ZWeight>,
    inserted: Relation<V>,
    removed: Relation<V>,
    replacements: Relation<V>,
    delta: ZSet<(V, V)>,
}

impl<V: Ord> ReachabilityUpdate<'_, V> {
    pub fn delta(&self) -> &ZSet<(V, V)> {
        &self.delta
    }
}

impl<V: Ord + Clone> ReachabilityUpdate<'_, V> {
    /// Publish after downstream sinks/operators have also prepared. No
    /// recoverable arithmetic or callback runs between these assignments.
    pub fn commit(self) -> ZSet<(V, V)> {
        let Self {
            owner,
            weights,
            inserted,
            removed,
            replacements,
            delta,
        } = self;
        owner.edges.publish(weights);
        for (source, row) in removed {
            for destination in row {
                remove_pair(&mut owner.outgoing, &source, &destination);
            }
        }
        for (source, row) in inserted {
            owner.outgoing.entry(source).or_default().extend(row);
        }
        for (source, row) in replacements {
            if row.is_empty() {
                owner.reachable.remove(&source);
            } else {
                owner.reachable.insert(source, row);
            }
        }
        for ((source, destination), weight) in delta.iter() {
            if weight < &ZWeight::ZERO {
                remove_pair(&mut owner.predecessors, destination, source);
            } else {
                owner
                    .predecessors
                    .entry(destination.clone())
                    .or_default()
                    .insert(source.clone());
            }
        }
        delta
    }
}

impl<V: Ord> core::fmt::Debug for ReachabilityUpdate<'_, V> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ReachabilityUpdate")
            .field("delta_support", &self.delta.len())
            .field("data", &"[REDACTED]")
            .finish()
    }
}

#[cfg(test)]
mod tests;
