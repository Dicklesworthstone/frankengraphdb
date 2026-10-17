//! Identified adjacency: for one relation and orientation, each endpoint's
//! (edge, neighbor) list, read by the identity-preserving cursors.
//!
//! An evaluation builds one per relation and orientation it expands. It was
//! an ordered map of vectors: a vector allocated and grown per endpoint, and
//! a map node per endpoint, over every admitted edge of every query (a
//! quarter of a web-google SHORTEST, fgdb-61p1n). The flat form keeps the
//! endpoints ascending beside offsets into one array of lists, each list in
//! the order the map's vector held it.

use crate::GlaExecutionEvent;
use fgdb_types::{EId, VId};
use std::collections::BTreeMap;

/// The (edge, neighbor) list of each endpoint an identified cursor reaches.
pub trait IdentifiedNeighbors: Sync {
    /// `vertex`'s list, or None when it has no edge here.
    fn neighbors(&self, vertex: &VId) -> Option<&[(EId, VId)]>;
}

impl IdentifiedNeighbors for BTreeMap<VId, Vec<(EId, VId)>> {
    fn neighbors(&self, vertex: &VId) -> Option<&[(EId, VId)]> {
        self.get(vertex).map(Vec::as_slice)
    }
}

/// Endpoints ascending, each beside its list: `lists[starts[i]..starts[i +
/// 1]]` is `endpoints[i]`'s. A cursor finds an endpoint through the build's
/// own probed map (never iterated into an order), one probe where the
/// ordered map took a descent: every expanded vertex of a SHORTEST looks its
/// list up.
#[derive(Debug, Default)]
pub(crate) struct IdentifiedAdjacency {
    endpoints: Vec<VId>,
    positions: crate::id_hash::IdMap<VId, usize>,
    starts: Vec<usize>,
    lists: Vec<(EId, VId)>,
}

impl IdentifiedNeighbors for IdentifiedAdjacency {
    fn neighbors(&self, vertex: &VId) -> Option<&[(EId, VId)]> {
        let at = *self.positions.get(vertex)?;
        Some(&self.lists[self.starts[at]..self.starts[at + 1]])
    }
}

impl IdentifiedAdjacency {
    /// Every endpoint and its list, endpoints ascending.
    pub(crate) fn iter(&self) -> impl Iterator<Item = (VId, &[(EId, VId)])> + '_ {
        self.endpoints
            .iter()
            .zip(self.starts.windows(2))
            .map(|(&endpoint, range)| (endpoint, &self.lists[range[0]..range[1]]))
    }

    /// Each endpoint's list, endpoints ascending, to sort in place.
    pub(crate) fn lists_mut(&mut self) -> impl Iterator<Item = &mut [(EId, VId)]> + '_ {
        let mut rest = self.lists.as_mut_slice();
        self.starts.windows(2).map(move |range| {
            let (list, tail) = std::mem::take(&mut rest).split_at_mut(range[1] - range[0]);
            rest = tail;
            list
        })
    }
}

/// An identified adjacency while it is built: each push's endpoint slot,
/// numbered in arrival order through a probed (never iterated) map, beside
/// its (edge, neighbor). Edges arrive in no endpoint order (identities are a
/// keyed permutation), so no push can skip the probe.
#[derive(Default)]
pub(crate) struct IdentifiedStage {
    slots: crate::id_hash::IdMap<VId, usize>,
    endpoints: Vec<VId>,
    pushed: Vec<usize>,
    entries: Vec<(EId, VId)>,
}

impl IdentifiedStage {
    /// One (edge, neighbor) under `from`, charging the endpoint's own scratch
    /// entry first when it is new: one probe.
    pub(crate) fn push<E>(
        &mut self,
        from: VId,
        neighbor: (EId, VId),
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<(), E> {
        let slot = match self.slots.entry(from) {
            std::collections::hash_map::Entry::Occupied(entry) => *entry.get(),
            std::collections::hash_map::Entry::Vacant(entry) => {
                control(GlaExecutionEvent::ScratchEntry)?;
                self.endpoints.push(from);
                *entry.insert(self.endpoints.len() - 1)
            }
        };
        self.pushed.push(slot);
        self.entries.push(neighbor);
        Ok(())
    }

    /// The adjacency: endpoints ascending, each list in push order.
    pub(crate) fn finish(self) -> IdentifiedAdjacency {
        let Self {
            mut slots,
            endpoints,
            pushed,
            entries,
        } = self;
        let mut ordered: Vec<(VId, usize)> = endpoints.into_iter().zip(0..).collect();
        ordered.sort_unstable();
        let mut rank = vec![0; ordered.len()];
        for (at, &(_, slot)) in ordered.iter().enumerate() {
            rank[slot] = at;
        }
        for slot in slots.values_mut() {
            *slot = rank[*slot];
        }
        let endpoints: Vec<VId> = ordered.into_iter().map(|(endpoint, _)| endpoint).collect();
        let mut starts = vec![0; endpoints.len() + 1];
        for &slot in &pushed {
            starts[rank[slot] + 1] += 1;
        }
        for at in 1..starts.len() {
            starts[at] += starts[at - 1];
        }
        // Scatter each entry to its endpoint's next place, in push order. The
        // writes are independent, so their misses overlap; following the
        // permutation's cycles in place made each wait on the last (more
        // than a quarter of a web-google SHORTEST).
        let mut next = starts.clone();
        let mut lists = vec![(EId(0), VId(0)); entries.len()];
        for (slot, entry) in pushed.into_iter().zip(entries) {
            let at = rank[slot];
            lists[next[at]] = entry;
            next[at] += 1;
        }
        IdentifiedAdjacency {
            endpoints,
            positions: slots,
            starts,
            lists,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The flat adjacency holds exactly the ordered map of vectors the same
    /// pushes build, lists in push order, over random pushes with repeated
    /// endpoints, parallel entries and endpoints arriving in any order.
    #[test]
    fn flat_adjacency_equals_the_ordered_map_of_pushes() {
        let mut state = 0x1de7_f1ed_0000_0001_u64;
        let mut random = |bound: u64| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state % bound
        };
        let mut checked = 0;
        for _ in 0..400 {
            let endpoints = 1 + random(40);
            let pushes = random(200);
            let mut stage = IdentifiedStage::default();
            let mut map: BTreeMap<VId, Vec<(EId, VId)>> = BTreeMap::new();
            let (mut log, mut expected) = (Vec::new(), Vec::new());
            for _ in 0..pushes {
                let from = VId(u128::from(random(endpoints)) * 0x1_0000_0001);
                let neighbor = (
                    EId(u128::from(random(5))),
                    VId(u128::from(random(endpoints))),
                );
                stage
                    .push(from, neighbor, &mut |event| {
                        log.push(event);
                        Ok::<_, ()>(())
                    })
                    .unwrap();
                if !map.contains_key(&from) {
                    expected.push(GlaExecutionEvent::ScratchEntry);
                }
                map.entry(from).or_default().push(neighbor);
            }
            let flat = stage.finish();
            assert_eq!(log, expected);
            let listed: Vec<(VId, Vec<(EId, VId)>)> = flat
                .iter()
                .map(|(endpoint, list)| (endpoint, list.to_vec()))
                .collect();
            assert_eq!(listed, map.clone().into_iter().collect::<Vec<_>>());
            for probe in 0..endpoints + 2 {
                let vertex = VId(u128::from(probe) * 0x1_0000_0001);
                assert_eq!(flat.neighbors(&vertex), map.neighbors(&vertex));
            }
            let mut sorted = flat;
            for list in sorted.lists_mut() {
                list.sort_unstable();
            }
            for (endpoint, list) in sorted.iter() {
                let mut want = map[&endpoint].clone();
                want.sort_unstable();
                assert_eq!(list, want.as_slice());
            }
            checked += map.len();
        }
        assert!(checked > 5_000, "{checked}");
    }
}
