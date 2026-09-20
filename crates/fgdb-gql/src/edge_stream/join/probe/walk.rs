//! Endpoint-support specialization of a finite, uncaptured VarLengthExpand.
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
        start: VId, bounds: GraphWalkBounds, search: GraphWalkSearch, source: &S,
        control: &mut impl FnMut(GlaExecutionEvent) -> ScanResult<(), S::Error, C>,
    ) -> ScanResult<Self, S::Error, C> {
        // Identity paths still require a live vertex; no lookup capability is
        // needed when maximum=0, and missing/NULL roots are not invented.
        vertex(source, start, control)?;
        let mut cursor = Self {
            start, bounds, search, stack: Vec::new(), visited: BTreeSet::new(),
            emitted: BTreeSet::new(), edges: BTreeSet::new(), vertices: BTreeSet::new(),
        };
        cursor.push(start, None, control)?;
        Ok(cursor)
    }

    fn merge_depths(&self) -> bool {
        matches!(self.search, GraphWalkSearch::All | GraphWalkSearch::AllShortest | GraphWalkSearch::AnyShortest)
    }

    fn push<E>(
        &mut self, vertex: VId, incoming: Option<EId>,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<(), E> {
        // Reserve before every growing container; failed preparation/pull
        // discards the complete owning probe, never a partially advanced row.
        if self.merge_depths() {
            control(GlaExecutionEvent::ScratchEntry)?;
            self.visited.insert((self.stack.len(), vertex));
        } else if self.search == GraphWalkSearch::Trail {
            if let Some(edge) = incoming {
                control(GlaExecutionEvent::ScratchEntry)?;
                self.edges.insert(edge);
            }
        } else if incoming.is_none() || vertex != self.start {
            control(GlaExecutionEvent::ScratchEntry)?;
            self.vertices.insert(vertex);
        }
        for _ in 0..4 { control(GlaExecutionEvent::ScratchEntry)?; }
        self.stack.push(Frame { vertex, incoming, after: None, offered: false });
        Ok(())
    }

    fn pop<E>(&mut self, control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>) -> Result<(), E> {
        control(GlaExecutionEvent::Work)?;
        let frame = self.stack.pop().expect("nonempty traversal");
        if self.search == GraphWalkSearch::Trail {
            if let Some(edge) = frame.incoming { self.edges.remove(&edge); }
        } else if !self.merge_depths() && frame.vertex != self.start {
            self.vertices.remove(&frame.vertex);
        }
        Ok(())
    }

    pub(super) fn next<S: EdgeScanSource, C>(
        &mut self, expansion: Expansion, source: &S,
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
                || (self.search == GraphWalkSearch::Simple && depth > 0 && from == self.start) {
                self.pop(control)?;
                continue;
            }
            let after = frame.after;
            let next = match source.next_incident_edge(from, expansion.direction, after, control) {
                Ok(next) => next,
                Err(EdgeExpansionSourceError::Unavailable) => {
                    return Err(GqlQueryError::Source(EdgeScanError::ExpansionUnavailable));
                }
                Err(EdgeExpansionSourceError::Read(error)) => flatten(Err(error))?,
            };
            let Some(eid) = next else { self.pop(control)?; continue; };
            control(GlaExecutionEvent::Work)?;
            if after.is_some_and(|prior| eid <= prior) {
                return Err(GqlQueryError::Source(EdgeScanError::NonIncreasingIdentity));
            }
            self.stack.last_mut().expect("nonempty traversal").after = Some(eid);
            record()?;
            let Some(to) = resolve_target(source, eid, from, expansion, control)? else { continue; };
            control(GlaExecutionEvent::Work)?;
            let allowed = match self.search {
                GraphWalkSearch::All | GraphWalkSearch::AllShortest | GraphWalkSearch::AnyShortest =>
                    !self.visited.contains(&(depth + 1, to)),
                GraphWalkSearch::Trail => !self.edges.contains(&eid),
                GraphWalkSearch::Acyclic => !self.vertices.contains(&to),
                GraphWalkSearch::Simple => to == self.start || !self.vertices.contains(&to),
            };
            if allowed { self.push(to, Some(eid), control)?; }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests;
