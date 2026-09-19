//! Exact triangle analytics driven by the canonical committed topology input.
//!
//! All coordinates advance through CommittedEdgeInput before relation filtering.
//! That owner validates EId lifetimes, cascades, schema and history continuity;
//! IncrementalTriangles owns undirected normalization and ALL/DISTINCT weights.
//! One prepared guard couples both publications and exposes a tentative delta
//! and total for downstream admission. No new log, decoder or commit authority.

use super::{IncrementalTriangles, TriangleError, TriangleQuantifier, TriangleUpdate};
use crate::zset::committed::snapshot::EdgeSnapshot;
use crate::zset::committed::{CommittedEdgeInput, EdgeInputError, EdgeInputUpdate};
use crate::zset::event;
use crate::{
    LimbLimit, LocalDeltaBatchIndex, LogicalDeltaBatch, RelationId, ZSet, ZSetError, ZSetEvent,
    ZWeight,
};
use fgdb_types::{BranchId, CommitCx, CommitSeq, GraphId, VId};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommittedTrianglesError<E> {
    Input(EdgeInputError<E>),
    Triangles(TriangleError<E>),
}
impl<E> From<EdgeInputError<E>> for CommittedTrianglesError<E> {
    fn from(error: EdgeInputError<E>) -> Self {
        Self::Input(error)
    }
}
impl<E> From<TriangleError<E>> for CommittedTrianglesError<E> {
    fn from(error: TriangleError<E>) -> Self {
        Self::Triangles(error)
    }
}
impl<E> From<ZSetError<E>> for CommittedTrianglesError<E> {
    fn from(error: ZSetError<E>) -> Self {
        Self::Triangles(TriangleError::Delta(error))
    }
}
impl<E: core::fmt::Display> core::fmt::Display for CommittedTrianglesError<E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Input(error) => error.fmt(f),
            Self::Triangles(error) => error.fmt(f),
        }
    }
}
impl<E: core::error::Error + 'static> core::error::Error for CommittedTrianglesError<E> {}

/// Triangles in the undirected projection of one graph/branch/relation.
/// Reverse orientations and parallel identities contribute independent edge
/// choices for ALL; DISTINCT counts each supported unordered vertex triple once.
/// Self-loops retain their identity lifetime but never form a triangle.
///
/// This is an in-process operator, not a durable subscription. Initialization
/// requires origin or an authoritative EdgeSnapshot; there is no frontier setter.
/// Ordinary ticks visit changed neighborhoods, never a full result snapshot.
#[derive(PartialEq, Eq)]
pub struct CommittedTriangles {
    input: CommittedEdgeInput,
    relation: RelationId,
    triangles: IncrementalTriangles<VId>,
}
impl CommittedTriangles {
    pub fn new(
        graph: GraphId,
        branch: BranchId,
        relation: RelationId,
        quantifier: TriangleQuantifier,
    ) -> Self {
        Self {
            input: CommittedEdgeInput::new(graph, branch),
            relation,
            triangles: IncrementalTriangles::new(quantifier),
        }
    }

    /// Consume a completed, source-authenticated current topology snapshot.
    /// Historical triangles are not replayed. A failure discards this private
    /// build; a live owner must also admit its sink before swapping generations.
    pub fn from_snapshot<E>(
        snapshot: EdgeSnapshot,
        relation: RelationId,
        quantifier: TriangleQuantifier,
        limbs: LimbLimit,
        control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
    ) -> Result<Self, CommittedTrianglesError<E>> {
        let EdgeSnapshot { input, delta } = snapshot;
        let edges = project(&delta, relation, limbs, control)?;
        let mut triangles = IncrementalTriangles::new(quantifier);
        let pending = triangles.prepare(&edges, limbs, control)?;
        event(control, ZSetEvent::Work)?;
        let _ = pending.commit();
        Ok(Self {
            input,
            relation,
            triangles,
        })
    }

    pub fn frontier(&self) -> CommitSeq {
        self.input.frontier()
    }
    pub fn relation(&self) -> RelationId {
        self.relation
    }
    pub fn quantifier(&self) -> TriangleQuantifier {
        self.triangles.quantifier()
    }
    /// Exact count; never narrowed to u64/i128 or recomputed by scanning triples.
    pub fn total(&self) -> &ZWeight {
        self.triangles.total()
    }

    /// Explicit audit/new-sink export. Ordinary maintenance does not call this.
    pub fn snapshot<E>(
        &self,
        limbs: LimbLimit,
        control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
    ) -> Result<ZSet<(VId, VId, VId)>, CommittedTrianglesError<E>> {
        self.triangles.snapshot(limbs, control).map_err(Into::into)
    }

    /// Read exactly one successor from an authenticated retained index. Even a
    /// caught-up call validates the anchor. Empty triangle deltas still commit
    /// input identities and advance the frontier: they must not be skipped.
    pub fn prepare_next<E>(
        &mut self,
        index: &LocalDeltaBatchIndex,
        limbs: LimbLimit,
        control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
    ) -> Result<Option<CommittedTrianglesUpdate<'_>>, CommittedTrianglesError<E>> {
        let Some(input) = self.input.prepare_next(index, limbs, control)? else {
            return Ok(None);
        };
        Self::prepare_input(input, &mut self.triangles, self.relation, limbs, control).map(Some)
    }

    /// Live-publisher lane. The caller supplies commit-purpose authority and
    /// proves this is the next whole committed batch of the same source. Detached
    /// consumers use prepare_next and its retained-anchor checks instead.
    pub fn prepare_committed_successor<E>(
        &mut self,
        cx: &CommitCx,
        batch: &LogicalDeltaBatch,
        limbs: LimbLimit,
        control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
    ) -> Result<CommittedTrianglesUpdate<'_>, CommittedTrianglesError<E>> {
        let input = self
            .input
            .prepare_committed_successor(cx, batch, limbs, control)?;
        Self::prepare_input(input, &mut self.triangles, self.relation, limbs, control)
    }

    fn prepare_input<'a, E>(
        input: EdgeInputUpdate<'a>,
        triangles: &'a mut IncrementalTriangles<VId>,
        relation: RelationId,
        limbs: LimbLimit,
        control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
    ) -> Result<CommittedTrianglesUpdate<'a>, CommittedTrianglesError<E>> {
        let edges = project(input.delta(), relation, limbs, control)?;
        let triangles = triangles.prepare(&edges, limbs, control)?;
        event(control, ZSetEvent::Work)?;
        Ok(CommittedTrianglesUpdate { input, triangles })
    }
}

fn project<E>(
    input: &ZSet<crate::zset::committed::EdgeTuple>,
    relation: RelationId,
    limbs: LimbLimit,
    control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
) -> Result<ZSet<(VId, VId)>, CommittedTrianglesError<E>> {
    let mut edges = ZSet::new();
    for ((r, a, b), weight) in input.iter() {
        event(control, ZSetEvent::Work)?;
        if *r == relation {
            let weight = weight.checked_clone(limbs).map_err(ZSetError::Arithmetic)?;
            edges.accumulate((*a, *b), weight, limbs, control)?;
        }
    }
    Ok(edges)
}

impl core::fmt::Debug for CommittedTriangles {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("CommittedTriangles")
            .field("frontier", &self.frontier())
            .field("data", &"[REDACTED]")
            .finish()
    }
}

#[must_use = "dropping a committed triangle update aborts input and analytics"]
pub struct CommittedTrianglesUpdate<'a> {
    input: EdgeInputUpdate<'a>,
    triangles: TriangleUpdate<'a, VId>,
}
impl CommittedTrianglesUpdate<'_> {
    pub fn commit_seq(&self) -> CommitSeq {
        self.input.commit_seq()
    }
    pub fn delta(&self) -> &ZSet<(VId, VId, VId)> {
        self.triangles.delta()
    }
    pub fn total(&self) -> &ZWeight {
        self.triangles.total()
    }

    /// Publish only after every downstream participant prepares. There are no
    /// recoverable callbacks between these existing infallible publications;
    /// ordinary allocation/panic behavior retains the parent operators' boundary.
    pub fn commit(self) -> ZSet<(VId, VId, VId)> {
        let Self { input, triangles } = self;
        let delta = triangles.commit();
        let _ = input.commit();
        delta
    }
}
impl core::fmt::Debug for CommittedTrianglesUpdate<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("CommittedTrianglesUpdate")
            .field("commit_seq", &self.commit_seq())
            .field("delta_support", &self.delta().len())
            .field("data", &"[REDACTED]")
            .finish()
    }
}

#[cfg(test)]
mod tests;
