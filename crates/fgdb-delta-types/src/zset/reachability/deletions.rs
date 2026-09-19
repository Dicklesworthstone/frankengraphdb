//! Decremental closure with an explicit boundary, not path reference counts.
//!
//! For every removed support edge u->v, potentially lost pairs lie in
//! ({u} union old_predecessors(u)) x ({v} union old_reachable(v)). Every lost
//! path contains a removed edge, so this union is complete. For a fixed source
//! s, old destinations outside that union remain reachable without any removed
//! edge. They are a sound boundary, not a stale-closure shortcut.
//!
//! Seed candidate destinations from surviving incoming edges whose origin is
//! s or a still-reachable boundary vertex, then traverse surviving edges ONLY
//! inside the candidate region. Any surviving path into the region has a first
//! entry (or re-entry) from that boundary. Cycles inside the region cannot seed
//! themselves. A candidate s is rederived only through a nonempty cycle; the
//! zero-hop root is used as an edge origin, never emitted as a reachable pair.
//!
//! Retained forward/reverse closure rows are borrowed. Only lost pairs are
//! staged for publication. Candidate cones can overlap or cover the complete
//! closure, and incident edges still cost work: this is not an output-linear
//! worst-case claim, nor disk-backed arrangements or an allocator-byte bound.

use super::*;

impl<V: Ord + Clone> IncrementalReachability<V> {
    pub(super) fn derive_deletions<E>(
        &self,
        removed: &Relation<V>,
        limbs: LimbLimit,
        control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
    ) -> Result<ZSet<(V, V)>, ZSetError<E>> {
        let mut candidates = Relation::new();
        for (source, destinations) in removed {
            event(control, ZSetEvent::Work)?;
            for destination in destinations {
                event(control, ZSetEvent::Work)?;
                for root in std::iter::once(source)
                    .chain(self.predecessors.get(source).into_iter().flatten())
                {
                    event(control, ZSetEvent::Work)?;
                    for target in std::iter::once(destination)
                        .chain(self.reachable.get(destination).into_iter().flatten())
                    {
                        event(control, ZSetEvent::Work)?;
                        insert_pair(&mut candidates, root, target, control)?;
                    }
                }
            }
        }

        let mut output = ZSet::new();
        for (source, targets) in candidates {
            event(control, ZSetEvent::Work)?;
            let mut survived = BTreeSet::new();
            let mut pending = Vec::new();
            for target in &targets {
                event(control, ZSetEvent::Work)?;
                // Enumerate incoming support, not all of source's unaffected
                // reachable vertices. The latter would rescan a long prefix
                // even for the removal of its single terminal edge.
                for predecessor in self.incoming.get(target).into_iter().flatten() {
                    event(control, ZSetEvent::Work)?;
                    if removed.get(predecessor).is_some_and(|row| row.contains(target)) {
                        continue;
                    }
                    if predecessor == &source
                        || (!targets.contains(predecessor) && self.contains(&source, predecessor))
                    {
                        enqueue(&mut survived, &mut pending, target, control)?;
                        break;
                    }
                }
            }
            while let Some(vertex) = pending.pop() {
                event(control, ZSetEvent::Work)?;
                let deleted = removed.get(&vertex);
                for target in self.outgoing.get(&vertex).into_iter().flatten() {
                    event(control, ZSetEvent::Work)?;
                    if targets.contains(target)
                        && deleted.is_none_or(|row| !row.contains(target))
                    {
                        enqueue(&mut survived, &mut pending, target, control)?;
                    }
                }
            }
            for target in targets {
                event(control, ZSetEvent::Work)?;
                if !survived.contains(&target) {
                    output.accumulate(
                        (source.clone(), target),
                        ZWeight::from_i128(-1),
                        limbs,
                        control,
                    )?;
                }
            }
        }
        Ok(output)
    }
}

fn enqueue<V: Ord + Clone, E>(
    survived: &mut BTreeSet<V>,
    pending: &mut Vec<V>,
    vertex: &V,
    control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
) -> Result<(), ZSetError<E>> {
    if insert_vertex(survived, vertex, control)? {
        event(control, ZSetEvent::ScratchEntry)?;
        pending.push(vertex.clone());
    }
    Ok(())
}

#[cfg(test)]
mod tests;
