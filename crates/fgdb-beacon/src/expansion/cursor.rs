//! A source-independent continuation for the existing unit-hop expansion.
//! Hosts may await membership or adjacency between protocol steps without
//! blocking an executor or retaining a second graph implementation.

use super::{ExpansionLimits, GraphHit, admit_visit, tree_work};
use crate::{BeaconError, WorkControl};
use fgdb_types::VId;
use std::collections::{BTreeMap, BinaryHeap, VecDeque};

/// The next source operation. A request must be completed before requesting
/// another. Complete is a traversal boundary, not an already-ranked answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExpansionRequest {
    Seed(VId),
    Neighbors(VId),
    Complete,
}

#[derive(Clone, Copy)]
struct Membership {
    id: VId,
    hops: u32,
    seed: bool,
}

/// Resumable multi-source BFS shared by synchronous and asynchronous hosts.
///
/// Request a seed, resolve its membership, then service each Neighbors request
/// completely. For every admitted neighbor call prepare_neighbor followed by
/// resolve_membership; finish_neighbors acknowledges successful source EOF.
/// Only after Complete may finish filter endpoints and rank the entire bounded
/// traversal. A small candidate depth never truncates source exploration.
///
/// Membership and adjacency MUST come from the same pinned, already-authorized
/// selection. Hosts filter relations and BOTH endpoints before supplying a
/// neighbor. This protocol enforces ordering, not source truth or authority.
/// Report external errors with refuse (or drop the cursor); never acknowledge
/// a failed source as EOF. A failed/unwound step poisons later operations.
///
/// State is O(visited vertices + candidates); no edge directory is retained.
/// Work is admitted before each membership request, queue step and ranking
/// operation. The optional admission callback runs before retaining a NEW
/// visit, permitting a host MemoryPool to cover the distance map and queue.
/// Hosts separately meter source I/O and reserve ranking/output workspace.
pub struct ExpansionCursor<'a> {
    seeds: &'a [VId],
    next_seed: usize,
    max_hops: u32,
    include_seeds: bool,
    candidates: usize,
    limits: ExpansionLimits,
    distance: BTreeMap<VId, u32>,
    frontier: VecDeque<(VId, u32)>,
    pending: Option<Membership>,
    active: Option<(VId, u32)>,
    complete: bool,
    failure: Option<BeaconError>,
}

impl core::fmt::Debug for ExpansionCursor<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ExpansionCursor")
            .field("visited", &self.distance.len())
            .field("complete", &self.complete)
            .field("failed", &self.failure.is_some())
            .finish_non_exhaustive()
    }
}

impl<'a> ExpansionCursor<'a> {
    pub fn new(
        seeds: &'a [VId],
        max_hops: u32,
        include_seeds: bool,
        candidates: u32,
        limits: ExpansionLimits,
        work: &mut dyn WorkControl,
    ) -> Result<Self, BeaconError> {
        work.charge(1)?;
        if seeds.len() > limits.max_seed_ids {
            return Err(BeaconError::ResourceLimit {
                resource: "expansion seed IDs",
                limit: limits.max_seed_ids,
            });
        }
        let candidates = usize::try_from(candidates)
            .map_err(|_| BeaconError::InvalidQuery("graph candidate depth exceeds usize"))?;
        Ok(Self {
            seeds,
            next_seed: 0,
            max_hops,
            include_seeds,
            candidates,
            limits,
            distance: BTreeMap::new(),
            frontier: VecDeque::new(),
            pending: None,
            active: None,
            complete: false,
            failure: None,
        })
    }

    /// Includes seeds and rejected result endpoints, but not absent seeds.
    pub fn visited_vertices(&self) -> usize {
        self.distance.len()
    }

    /// Preserve a source's first refusal even if a host catches it and retries.
    pub fn refuse(&mut self, error: BeaconError) -> BeaconError {
        self.failure.get_or_insert(error).clone()
    }

    fn step<T>(
        &mut self,
        operation: impl FnOnce(&mut Self) -> Result<T, BeaconError>,
    ) -> Result<T, BeaconError> {
        if let Some(error) = &self.failure {
            return Err(error.clone());
        }
        self.failure = Some(BeaconError::Invariant("unfinished expansion step"));
        let result = operation(self);
        self.failure = result.as_ref().err().cloned();
        result
    }

    pub fn request(
        &mut self,
        work: &mut dyn WorkControl,
    ) -> Result<ExpansionRequest, BeaconError> {
        self.step(|cursor| {
            if cursor.pending.is_some() || cursor.active.is_some() {
                return Err(BeaconError::InvalidQuery("expansion source request is outstanding"));
            }
            if cursor.complete || cursor.candidates == 0 {
                cursor.complete = true;
                return Ok(ExpansionRequest::Complete);
            }
            if let Some(&id) = cursor.seeds.get(cursor.next_seed) {
                work.charge(tree_work(cursor.distance.len()))?;
                cursor.pending = Some(Membership { id, hops: 0, seed: true });
                return Ok(ExpansionRequest::Seed(id));
            }
            while let Some((id, hops)) = cursor.frontier.pop_front() {
                work.charge(1)?;
                if hops < cursor.max_hops {
                    cursor.active = Some((id, hops));
                    return Ok(ExpansionRequest::Neighbors(id));
                }
            }
            cursor.complete = true;
            Ok(ExpansionRequest::Complete)
        })
    }

    /// Preflight a neighbor's membership and distance-map work. A host may
    /// suspend for source membership AFTER this step, before resolving it.
    pub fn prepare_neighbor(
        &mut self,
        id: VId,
        work: &mut dyn WorkControl,
    ) -> Result<(), BeaconError> {
        self.step(|cursor| {
            if cursor.pending.is_some() {
                return Err(BeaconError::InvalidQuery("expansion membership is outstanding"));
            }
            let (_, hops) = cursor.active.ok_or(BeaconError::InvalidQuery(
                "expansion has no active neighbor source",
            ))?;
            work.charge(tree_work(cursor.distance.len()))?;
            // An active source is strictly below max_hops <= u32::MAX.
            cursor.pending = Some(Membership { id, hops: hops + 1, seed: false });
            Ok(())
        })
    }

    /// Resolve exactly the outstanding membership request. Absent seeds are
    /// skipped; an out-of-selection emitted neighbor is a source error.
    /// `admit` is called once per new visit, AFTER its visit limit is checked
    /// and BEFORE map/queue allocation. Duplicate paths and seeds call it zero
    /// times. Callback refusal/unwind leaves no publishable partial traversal.
    pub fn resolve_membership(
        &mut self,
        present: bool,
        admit: impl FnOnce() -> Result<(), BeaconError>,
    ) -> Result<(), BeaconError> {
        self.step(|cursor| {
            let pending = cursor.pending.take().ok_or(BeaconError::InvalidQuery(
                "expansion has no outstanding membership",
            ))?;
            if !present && !pending.seed {
                return Err(BeaconError::InvalidQuery(
                    "expansion endpoint outside selected corpus",
                ));
            }
            if present && !cursor.distance.contains_key(&pending.id) {
                admit_visit(cursor.distance.len(), cursor.limits)?;
                admit()?;
                cursor.distance.insert(pending.id, pending.hops);
                cursor.frontier.push_back((pending.id, pending.hops));
            }
            if pending.seed {
                cursor.next_seed += 1;
            }
            Ok(())
        })
    }

    /// Acknowledge complete, successful adjacency consumption, not a prefix.
    pub fn finish_neighbors(&mut self) -> Result<(), BeaconError> {
        self.step(|cursor| {
            if cursor.pending.is_some() || cursor.active.take().is_none() {
                return Err(BeaconError::InvalidQuery("expansion neighbor source is incomplete"));
            }
            Ok(())
        })
    }

    /// Consume a complete traversal. Endpoint eligibility is evaluated once
    /// per eligible-to-rank vertex in VId order, before canonical (hops, VId)
    /// top-k. Rejected vertices kept their transit role. Any late refusal
    /// discards all private candidates; no result escapes before the last gate.
    pub fn finish(
        self,
        mut eligible: impl FnMut(VId, &mut dyn WorkControl) -> Result<bool, BeaconError>,
        work: &mut dyn WorkControl,
    ) -> Result<Vec<GraphHit>, BeaconError> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if !self.complete || self.pending.is_some() || self.active.is_some() {
            return Err(BeaconError::InvalidQuery("expansion traversal is incomplete"));
        }
        if self.candidates == 0 {
            return Ok(Vec::new());
        }
        let mut best = BinaryHeap::new();
        for (id, hops) in self.distance {
            work.charge(2 * tree_work(best.len()))?;
            if hops == 0 && !self.include_seeds {
                continue;
            }
            if !eligible(id, work)? {
                continue;
            }
            let candidate = (hops, id);
            if best.len() < self.candidates {
                best.push(candidate);
            } else if best.peek().is_some_and(|worst| candidate < *worst) {
                best.pop();
                best.push(candidate);
            }
        }
        let mut rows = Vec::new();
        while !best.is_empty() {
            work.charge(tree_work(best.len()))?;
            let (hops, id) = best.pop().ok_or(BeaconError::Invariant("expansion heap disappeared"))?;
            rows.push(GraphHit { id, hops });
        }
        for i in 0..rows.len() / 2 {
            work.charge(1)?;
            let other = rows.len() - i - 1;
            rows.swap(i, other);
        }
        work.charge(1)?;
        Ok(rows)
    }
}

#[cfg(test)]
mod tests;
