//! Endpoint-support specialization of an uncaptured VarLengthExpand.
//!
//! Inside EXISTS, multiplicity and chosen route are unobservable. ALL,
//! ALL SHORTEST and ANY SHORTEST have the same endpoint support: shortest is
//! selected WITHIN the interval per endpoint pair. A predicate on that endpoint
//! or a subsequent atom does not inspect transit vertices or route length.
//! We may therefore visit each (depth, vertex) once for these three modes, but
//! must not settle a vertex once: positive lower bounds and cycles forbid it.
//!
//! TRAIL, ACYCLIC and SIMPLE require path-local history, not depth-only
//! dominance. They use iterative backtracking and can take exponential work.
//! Support coalescing never changes these laws. All sets, frames, index reads,
//! and endpoint deliveries share the outer query's control and record meter.
//! This is not a captured-path selector, graph store, spill or byte-memory bound.

use super::*;
use std::collections::BTreeSet;

struct Frame {
    vertex: VId,
    incoming: Option<EId>,
    after: Option<EId>,
    offered: bool,
}

pub(super) struct Endpoints {
    start: VId,
    bounds: GraphWalkBounds,
    search: GraphWalkSearch,
    stack: Vec<Frame>,
    visited: BTreeSet<(usize, VId)>,
    emitted: BTreeSet<VId>,
    edges: BTreeSet<EId>,
    vertices: BTreeSet<VId>,
}
impl Endpoints {
    pub(super) fn new<S: EdgeScanSource, C>(
        start: VId,
        bounds: GraphWalkBounds,
        search: GraphWalkSearch,
        source: &S,
        control: &mut impl FnMut(GlaExecutionEvent) -> ScanResult<(), S::Error, C>,
    ) -> ScanResult<Self, S::Error, C> {
        // Identity paths still require a live vertex; no lookup capability is
        // needed when maximum=0, and missing/NULL roots are not invented.
        vertex(source, start, control)?;
        Self::from_admitted_start(start, bounds, search, control)
    }

    /// The async owner proves this vertex from an already admitted binding
    /// record. Keep initialization and mode history identical to the ordinary
    /// cursor; this method never establishes visibility by itself.
    pub(super) fn from_admitted_start<E>(
        start: VId,
        bounds: GraphWalkBounds,
        search: GraphWalkSearch,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Self, E> {
        let mut cursor = Self {
            start,
            bounds,
            search,
            stack: Vec::new(),
            visited: BTreeSet::new(),
            emitted: BTreeSet::new(),
            edges: BTreeSet::new(),
            vertices: BTreeSet::new(),
        };
        cursor.push(start, None, control)?;
        Ok(cursor)
    }

    /// The depth a visited state is keyed by. Without an upper bound every
    /// depth at or past the minimum has the same future (it may emit, and it
    /// may always continue), so the key saturates there and an expansion
    /// over a cycle still ends.
    fn depth_class(&self, depth: usize) -> usize {
        if self.bounds.is_bounded() {
            depth
        } else {
            depth.min(self.bounds.minimum() as usize)
        }
    }

    fn merge_depths(&self) -> bool {
        matches!(
            self.search,
            GraphWalkSearch::All | GraphWalkSearch::AllShortest | GraphWalkSearch::AnyShortest
        )
    }

    fn allows(&self, depth: usize, vertex: VId, edge: EId) -> bool {
        match self.search {
            GraphWalkSearch::All | GraphWalkSearch::AllShortest | GraphWalkSearch::AnyShortest => {
                !self.visited.contains(&(self.depth_class(depth), vertex))
            }
            GraphWalkSearch::Trail => !self.edges.contains(&edge),
            GraphWalkSearch::Acyclic => !self.vertices.contains(&vertex),
            GraphWalkSearch::Simple => vertex == self.start || !self.vertices.contains(&vertex),
        }
    }

    fn push<E>(
        &mut self,
        vertex: VId,
        incoming: Option<EId>,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<(), E> {
        // Reserve before every growing container; failed preparation/pull
        // discards the complete owning probe, never a partially advanced row.
        if self.merge_depths() {
            control(GlaExecutionEvent::ScratchEntry)?;
            self.visited
                .insert((self.depth_class(self.stack.len()), vertex));
        } else if self.search == GraphWalkSearch::Trail {
            if let Some(edge) = incoming {
                control(GlaExecutionEvent::ScratchEntry)?;
                self.edges.insert(edge);
            }
        } else if incoming.is_none() || vertex != self.start {
            control(GlaExecutionEvent::ScratchEntry)?;
            self.vertices.insert(vertex);
        }
        for _ in 0..4 {
            control(GlaExecutionEvent::ScratchEntry)?;
        }
        self.stack.push(Frame {
            vertex,
            incoming,
            after: None,
            offered: false,
        });
        Ok(())
    }

    fn pop<E>(
        &mut self,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<(), E> {
        control(GlaExecutionEvent::Work)?;
        let frame = self.stack.pop().expect("nonempty traversal");
        if self.search == GraphWalkSearch::Trail {
            if let Some(edge) = frame.incoming {
                self.edges.remove(&edge);
            }
        } else if !self.merge_depths() && frame.vertex != self.start {
            self.vertices.remove(&frame.vertex);
        }
        Ok(())
    }

    pub(super) fn next<S: EdgeScanSource, C>(
        &mut self,
        expansion: Expansion,
        source: &S,
        control: &mut impl FnMut(GlaExecutionEvent) -> ScanResult<(), S::Error, C>,
        record: &mut impl FnMut() -> ScanResult<(), S::Error, C>,
    ) -> ScanResult<Option<VId>, S::Error, C> {
        while !self.stack.is_empty() {
            control(GlaExecutionEvent::Work)?;
            let depth = self.stack.len() - 1;
            let frame = self.stack.last_mut().expect("nonempty traversal");
            let from = frame.vertex;
            if !frame.offered {
                frame.offered = true;
                if depth >= self.bounds.minimum() as usize && !self.emitted.contains(&from) {
                    control(GlaExecutionEvent::ScratchEntry)?;
                    self.emitted.insert(from);
                    return Ok(Some(from));
                }
            }
            if depth == self.bounds.maximum() as usize
                || (self.search == GraphWalkSearch::Simple && depth > 0 && from == self.start)
            {
                self.pop(control)?;
                continue;
            }
            let after = frame.after;
            let next = match source.next_incident_edge_for_relation(
                from,
                expansion.relation,
                expansion.direction,
                after,
                control,
            ) {
                Ok(next) => next,
                Err(EdgeExpansionSourceError::Unavailable) => {
                    return Err(GqlQueryError::Source(EdgeScanError::ExpansionUnavailable));
                }
                Err(EdgeExpansionSourceError::Read(error)) => flatten(Err(error))?,
            };
            let Some(eid) = next else {
                self.pop(control)?;
                continue;
            };
            control(GlaExecutionEvent::Work)?;
            if after.is_some_and(|prior| eid <= prior) {
                return Err(GqlQueryError::Source(EdgeScanError::NonIncreasingIdentity));
            }
            self.stack.last_mut().expect("nonempty traversal").after = Some(eid);
            record()?;
            let Some(to) = resolve_target(source, eid, from, expansion, control)? else {
                continue;
            };
            control(GlaExecutionEvent::Work)?;
            if self.allows(depth + 1, to, eid) {
                self.push(to, Some(eid), control)?;
            }
        }
        Ok(None)
    }

    /// Awaitable intake for this exact support state, not a second path mode
    /// implementation. Only the selected endpoint record survives a yield;
    /// transit records are dropped before another source read. Identity paths
    /// borrow their already admitted seed through the enclosing probe's loan.
    ///
    /// All growing state is charged to a live outer record before allocation.
    /// That record outlives the complete probe, including backtracking across
    /// local atoms and awaits. Charges are conservative until the outer binding
    /// completes; dropping a transit record cannot refund retained history.
    pub(super) async fn next_async<S, F, C>(
        &mut self,
        expansion: Expansion,
        source: &mut S,
        meter: &mut Meter<F>,
        owner: &mut S::Record,
    ) -> ScanResult<Option<(VId, Option<S::Record>)>, S::Error, C>
    where
        S: AsyncEdgeJoinSource,
        F: FnMut() -> Result<(), C> + Send,
        C: Send,
    {
        use super::super::asynchronous::{admit, check_admission};

        let mut selected = None;
        while !self.stack.is_empty() {
            async_event(source, owner, meter, GlaExecutionEvent::Work)?;
            let depth = self.stack.len() - 1;
            let frame = self.stack.last_mut().expect("nonempty traversal");
            let from = frame.vertex;
            if !frame.offered {
                frame.offered = true;
                if depth >= self.bounds.minimum() as usize && !self.emitted.contains(&from) {
                    async_event(source, owner, meter, GlaExecutionEvent::ScratchEntry)?;
                    self.emitted.insert(from);
                    return Ok(Some((from, selected)));
                }
            }
            // A transit record is not the owner of path state and is not
            // needed to open its endpoint's next strict-successor incidence.
            selected = None;
            if depth == self.bounds.maximum() as usize
                || (self.search == GraphWalkSearch::Simple && depth > 0 && from == self.start)
            {
                self.pop(&mut |event| async_event(source, owner, meter, event))?;
                continue;
            }
            let mut after = self.stack.last().expect("nonempty traversal").after;
            let mut admitted = None;
            let result = source
                .next_incident_candidate(
                    from,
                    expansion.relation,
                    expansion.direction,
                    after,
                    &mut |event| admit(meter, &mut after, &mut admitted, event),
                )
                .await;
            let candidate = match result {
                Ok(candidate) => candidate,
                Err(EdgeExpansionSourceError::Read(error)) => flatten(Err(error))?,
                Err(EdgeExpansionSourceError::Unavailable) => {
                    return Err(GqlQueryError::Source(EdgeScanError::ExpansionUnavailable));
                }
            };
            check_admission(&candidate, admitted)?;
            self.stack.last_mut().expect("nonempty traversal").after = after;
            let Some(candidate) = candidate else {
                self.pop(&mut |event| async_event(source, owner, meter, event))?;
                continue;
            };
            let Some(record) = candidate.record else {
                continue; // Invisible candidates advance, but never create a witness.
            };
            async_event(source, owner, meter, GlaExecutionEvent::Work)?;
            let edge = record.edge();
            if !expansion.relation.matches(edge.relation) {
                continue;
            }
            let to = match expansion.direction {
                GlaDirection::Forward if edge.source == from => edge.target,
                GlaDirection::Reverse if edge.target == from => edge.source,
                GlaDirection::Undirected if edge.source == from => edge.target,
                GlaDirection::Undirected if edge.target == from => edge.source,
                _ => continue,
            };
            for vid in [from, to] {
                async_event(source, owner, meter, GlaExecutionEvent::Work)?;
                if record.vertex(vid).is_none() {
                    return Err(GqlQueryError::Source(EdgeScanError::DanglingEndpoint));
                }
            }
            async_event(source, owner, meter, GlaExecutionEvent::Work)?;
            if self.allows(depth + 1, to, candidate.eid) {
                self.push(to, Some(candidate.eid), &mut |event| {
                    async_event(source, owner, meter, event)
                })?;
                selected = Some(record);
            }
        }
        Ok(None)
    }
}

pub(super) fn async_event<S, F, C>(
    source: &S,
    owner: &S::Record,
    meter: &mut Meter<F>,
    event: GlaExecutionEvent,
) -> ScanResult<(), S::Error, C>
where
    S: AsyncEdgeJoinSource,
    F: FnMut() -> Result<(), C>,
{
    meter.event(event)?;
    source
        .evaluation_event(owner, event)
        .map_err(|error| GqlQueryError::Source(EdgeScanError::Source(error)))
}

#[cfg(test)]
mod tests;
