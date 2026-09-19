//! One committed topology input and its recursively maintained reachability.
//!
//! Feed the unfiltered, authenticated `Database::delta_index()` window. Input
//! identity tracking and history admission are owned by `CommittedEdgeInput`;
//! this composition neither decodes a second log nor invents commit authority.
//! Every whole batch advances through the input, even when the chosen relation
//! has no delta. Only then is its relation projected into directed reachability.
//!
//! A prepared guard owns both transitions. Downstream work may inspect the
//! tentative closure delta and prepare another sink; dropping the guard aborts
//! everything here. Committing runs no recoverable callbacks between topology,
//! closure and frontier publication. Standard allocation/panic boundaries are
//! the same as the underlying operators. This is caller-driven, in-process
//! maintenance, not durable view publication, a retention lease or a scheduler.

use super::{IncrementalReachability, ReachabilityError, ReachabilityUpdate};
use crate::zset::committed::{CommittedEdgeInput, EdgeInputError, EdgeInputUpdate};
use crate::zset::event;
use crate::{LimbLimit, LocalDeltaBatchIndex, RelationId, ZSet, ZSetError, ZSetEvent};
use fgdb_types::{BranchId, CommitSeq, GraphId, VId};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommittedReachabilityError<E> {
    Input(EdgeInputError<E>),
    Reachability(ReachabilityError<E>),
}

impl<E> From<EdgeInputError<E>> for CommittedReachabilityError<E> {
    fn from(error: EdgeInputError<E>) -> Self {
        Self::Input(error)
    }
}
impl<E> From<ReachabilityError<E>> for CommittedReachabilityError<E> {
    fn from(error: ReachabilityError<E>) -> Self {
        Self::Reachability(error)
    }
}
impl<E> From<ZSetError<E>> for CommittedReachabilityError<E> {
    fn from(error: ZSetError<E>) -> Self {
        Self::Reachability(ReachabilityError::Delta(error))
    }
}
impl<E: core::fmt::Display> core::fmt::Display for CommittedReachabilityError<E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Input(error) => error.fmt(f),
            Self::Reachability(error) => error.fmt(f),
        }
    }
}
impl<E: core::error::Error + 'static> core::error::Error for CommittedReachabilityError<E> {}

/// Non-reflexive, directed reachability for a fixed graph/branch/relation.
/// Paths have one or more edges of the selected relation; cycles yield self
/// pairs. Properties, valid time and labels do not filter this topology view.
/// Parallel EIds retain independent lifetimes through the existing input.
///
/// Starts at the stream origin; there is deliberately no arbitrary frontier
/// setter. Rebuild by replaying complete retained history. A fork or missing
/// anchor refuses instead of serving a falsely advanced view. The exposed
/// frontier identifies the last published view, which may lag its source.
#[derive(PartialEq, Eq)]
pub struct CommittedReachability {
    input: CommittedEdgeInput,
    relation: RelationId,
    reachability: IncrementalReachability<VId>,
}

impl CommittedReachability {
    pub fn new(graph: GraphId, branch: BranchId, relation: RelationId) -> Self {
        Self {
            input: CommittedEdgeInput::new(graph, branch),
            relation,
            reachability: IncrementalReachability::new(),
        }
    }

    pub fn frontier(&self) -> CommitSeq {
        self.input.frontier()
    }

    pub fn relation(&self) -> RelationId {
        self.relation
    }

    pub fn contains(&self, source: VId, destination: VId) -> bool {
        self.reachability.contains(&source, &destination)
    }

    /// Borrowed, canonical pair export; does not materialize another closure.
    pub fn pairs(&self) -> impl Iterator<Item = (VId, VId)> + '_ {
        self.reachability.pairs().map(|(source, destination)| (*source, *destination))
    }

    pub fn snapshot<E>(
        &self,
        limbs: LimbLimit,
        control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
    ) -> Result<ZSet<(VId, VId)>, CommittedReachabilityError<E>> {
        self.reachability.snapshot(limbs, control).map_err(Into::into)
    }

    /// Prepare exactly the next global batch, or `None` when caught up after
    /// validating the history anchor. Empty closure deltas still need commit:
    /// edge identity replacements and unrelated commits must advance the input.
    /// There is no partial acknowledgement independent of the recursive view.
    pub fn prepare_next<E>(
        &mut self,
        index: &LocalDeltaBatchIndex,
        limbs: LimbLimit,
        control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
    ) -> Result<Option<CommittedReachabilityUpdate<'_>>, CommittedReachabilityError<E>> {
        let Some(input) = self.input.prepare_next(index, limbs, control)? else {
            return Ok(None);
        };
        let mut edges = ZSet::new();
        for ((relation, source, destination), weight) in input.delta().iter() {
            event(control, ZSetEvent::Work)?;
            if *relation == self.relation {
                let weight = weight.checked_clone(limbs).map_err(ZSetError::Arithmetic)?;
                edges.accumulate((*source, *destination), weight, limbs, control)?;
            }
        }
        let reachability = self.reachability.prepare(&edges, limbs, control)?;
        // One last cancellable boundary while BOTH publications are tentative.
        event(control, ZSetEvent::Work)?;
        Ok(Some(CommittedReachabilityUpdate { input, reachability }))
    }
}

impl core::fmt::Debug for CommittedReachability {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("CommittedReachability")
            .field("frontier", &self.frontier())
            .field("data", &"[REDACTED]")
            .finish()
    }
}

#[must_use = "dropping a committed reachability update aborts both transitions"]
pub struct CommittedReachabilityUpdate<'a> {
    input: EdgeInputUpdate<'a>,
    reachability: ReachabilityUpdate<'a, VId>,
}

impl CommittedReachabilityUpdate<'_> {
    pub fn commit_seq(&self) -> CommitSeq {
        self.input.commit_seq()
    }

    pub fn delta(&self) -> &ZSet<(VId, VId)> {
        self.reachability.delta()
    }

    /// Call after all downstream preparation succeeds. No recoverable work
    /// occurs between publishing the closure and acknowledging its input.
    pub fn commit(self) -> ZSet<(VId, VId)> {
        let Self { input, reachability } = self;
        let delta = reachability.commit();
        let _input_delta = input.commit();
        delta
    }
}

impl core::fmt::Debug for CommittedReachabilityUpdate<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("CommittedReachabilityUpdate")
            .field("commit_seq", &self.commit_seq())
            .field("delta_support", &self.delta().len())
            .field("data", &"[REDACTED]")
            .finish()
    }
}

#[cfg(test)]
mod tests;
