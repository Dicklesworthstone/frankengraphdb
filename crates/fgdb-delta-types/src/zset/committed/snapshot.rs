//! Fresh topology arrangements at an authoritative snapshot cut.
//!
//! The source must supply ALL live EIds and its current relation epochs from
//! the same authenticated graph/branch snapshot as `anchor`. This module does
//! not authenticate a caller's graph projection. It never constructs a delta
//! batch, reinterprets a snapshot as an insertion commit, or sets an existing
//! consumer's cursor. The anchor is copied from an actual committed batch;
//! subsequent indexed reads still verify its marker AND template digest.
//!
//! Accumulation is streaming and governed. A failed insertion poisons the
//! private builder: swallowing a callback error cannot publish a partial
//! baseline. Restart from the authoritative source after any refusal.

use super::{Anchor, CommittedEdgeInput, EdgeInputError, EdgeTuple, validate_batch};
use crate::zset::event;
use crate::{
    LimbLimit, LogicalDeltaBatch, RelationId, SchemaEpoch, ZSet, ZSetError, ZSetEvent, ZWeight,
};
use fgdb_types::{BranchId, CommitSeq, EId, GraphId};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SnapshotInputError<E> {
    Input(EdgeInputError<E>),
    /// Origin is the empty stream, not a sequence at which data may be seeded.
    NonEmptyOrigin,
    /// A batch at sequence zero cannot be a committed snapshot anchor.
    OriginAnchor,
    /// An earlier operation refused; restarting is required.
    Refused,
}
impl<E> From<EdgeInputError<E>> for SnapshotInputError<E> {
    fn from(error: EdgeInputError<E>) -> Self {
        Self::Input(error)
    }
}
impl<E> From<ZSetError<E>> for SnapshotInputError<E> {
    fn from(error: ZSetError<E>) -> Self {
        Self::Input(EdgeInputError::Delta(error))
    }
}
impl<E: core::fmt::Display> core::fmt::Display for SnapshotInputError<E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Input(error) => error.fmt(f),
            Self::NonEmptyOrigin => f.write_str("nonempty topology at the stream origin"),
            Self::OriginAnchor => f.write_str("sequence zero is not a committed snapshot anchor"),
            Self::Refused => f.write_str("snapshot input builder previously refused"),
        }
    }
}
impl<E: core::error::Error + 'static> core::error::Error for SnapshotInputError<E> {}

/// Owned preparation only. No input, frontier or partial snapshot can escape
/// until `finish`; a refusal makes this builder permanently unpublishable.
#[must_use = "a snapshot input has no effect until successfully finished"]
pub struct EdgeSnapshotBuilder {
    input: CommittedEdgeInput,
    delta: ZSet<EdgeTuple>,
    refused: bool,
}
impl EdgeSnapshotBuilder {
    /// `None` admits only the empty stream. `Some(batch)` names the exact
    /// snapshot cut, not a commit to replay. The source owns completeness,
    /// current schema epochs, and the snapshot's binding to this marker.
    /// Merely possessing an arbitrary decoded batch is not authentication.
    pub fn new<E>(
        graph: GraphId,
        branch: BranchId,
        anchor: Option<&LogicalDeltaBatch>,
        control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
    ) -> Result<Self, SnapshotInputError<E>> {
        event(control, ZSetEvent::Work)?;
        let mut input = CommittedEdgeInput::new(graph, branch);
        if let Some(batch) = anchor {
            if batch.commit_seq() == CommitSeq::ORIGIN {
                return Err(SnapshotInputError::OriginAnchor);
            }
            validate_batch(batch, batch.commit_seq())?;
            input.anchor = Some(Anchor::of(batch));
        }
        Ok(Self {
            input,
            delta: ZSet::new(),
            refused: false,
        })
    }

    /// Bind a fresh builder to the CURRENT cut of one caller-authenticated
    /// delta window. The source must still supply complete topology and schema
    /// epochs from that same graph/branch snapshot. There is no arbitrary cut
    /// parameter, synthetic committed batch, or import into an existing input.
    ///
    /// A fully retired window can supply its exact retained boundary identity.
    /// A bare decoded floor, unsupported envelope or missing current anchor
    /// refuses before a builder escapes. This metadata is continuity evidence,
    /// not snapshot authentication, a prefix commitment or authority to GC.
    pub fn from_index<E>(
        graph: GraphId,
        branch: BranchId,
        index: &crate::LocalDeltaBatchIndex,
        control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
    ) -> Result<Self, SnapshotInputError<E>> {
        event(control, ZSetEvent::Work)?;
        if index.format() != crate::INDEX_FORMAT_V1 {
            return Err(EdgeInputError::Index(crate::IndexError::UnsupportedFormat {
                format: index.format(),
            })
            .into());
        }
        let at = index.frontier();
        let _suffix = index.since(at).map_err(EdgeInputError::from)?;
        let mut input = CommittedEdgeInput::new(graph, branch);
        if at == CommitSeq::ORIGIN {
            if !index.is_empty() {
                return Err(SnapshotInputError::NonEmptyOrigin);
            }
        } else {
            input.anchor = Some(super::checked_anchor(index, at)?);
        }
        Ok(Self {
            input,
            delta: ZSet::new(),
            refused: false,
        })
    }

    fn begin<E>(&mut self) -> Result<(), SnapshotInputError<E>> {
        if self.refused {
            return Err(SnapshotInputError::Refused);
        }
        // Set BEFORE any callback, arithmetic or allocation. An early return
        // cannot leave a recoverably incomplete builder looking usable.
        self.refused = true;
        Ok(())
    }

    fn epoch<E>(
        &mut self,
        relation: RelationId,
        epoch: SchemaEpoch,
        control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
    ) -> Result<(), SnapshotInputError<E>> {
        event(control, ZSetEvent::Work)?;
        match self.input.epochs.get(&relation) {
            Some(known) if *known != epoch => Err(EdgeInputError::SchemaChanged.into()),
            Some(_) => Ok(()),
            None => {
                event(control, ZSetEvent::ScratchEntry)?;
                self.input.epochs.insert(relation, epoch);
                Ok(())
            }
        }
    }

    /// Include known empty relations too. Equal repeated epoch observations
    /// are harmless; inconsistent epochs refuse the entire preparation.
    pub fn record_epoch<E>(
        &mut self,
        relation: RelationId,
        epoch: SchemaEpoch,
        control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
    ) -> Result<(), SnapshotInputError<E>> {
        self.begin()?;
        self.epoch(relation, epoch, control)?;
        self.refused = false;
        Ok(())
    }

    /// Admit exactly one live identity. Parallel EIds increase bag support;
    /// duplicate EIds, even with identical endpoints, are never deduplicated.
    /// Payloads, valid time and labels do not enter this topology arrangement.
    pub fn insert<E>(
        &mut self,
        eid: EId,
        tuple: EdgeTuple,
        epoch: SchemaEpoch,
        limbs: LimbLimit,
        control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
    ) -> Result<(), SnapshotInputError<E>> {
        self.begin()?;
        event(control, ZSetEvent::Work)?;
        if self.input.anchor.is_none() {
            return Err(SnapshotInputError::NonEmptyOrigin);
        }
        if self.input.edges.contains_key(&eid) {
            return Err(EdgeInputError::DuplicateEdge.into());
        }
        self.epoch(tuple.0, epoch, control)?;
        event(control, ZSetEvent::ScratchEntry)?;
        self.delta.accumulate(tuple, ZWeight::ONE, limbs, control)?;
        self.input.edges.insert(eid, tuple);
        self.refused = false;
        Ok(())
    }

    /// Finish an exact input/snapshot pair. There is no separate cursor setter
    /// and no method that imports rows into an already advanced input.
    pub fn finish<E>(
        self,
        control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
    ) -> Result<EdgeSnapshot, SnapshotInputError<E>> {
        if self.refused {
            return Err(SnapshotInputError::Refused);
        }
        event(control, ZSetEvent::Work)?;
        Ok(EdgeSnapshot {
            input: self.input,
            delta: self.delta,
        })
    }
}
impl core::fmt::Debug for EdgeSnapshotBuilder {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("EdgeSnapshotBuilder")
            .field("refused", &self.refused)
            .field("data", &"[REDACTED]")
            .finish()
    }
}

/// A completed fresh input together with its exact topology projection. The
/// fields cannot be mixed with another snapshot or another history's anchor.
#[must_use = "a completed snapshot must be installed or used to build downstream state"]
pub struct EdgeSnapshot {
    pub(crate) input: CommittedEdgeInput,
    pub(crate) delta: ZSet<EdgeTuple>,
}
impl EdgeSnapshot {
    pub fn frontier(&self) -> CommitSeq {
        self.input.frontier()
    }
    pub fn rows(&self) -> &ZSet<EdgeTuple> {
        &self.delta
    }
    /// Consume after downstream snapshot preparation succeeds. This is a NEW
    /// input; existing state must be replaced atomically by its owner.
    pub fn into_input(self) -> CommittedEdgeInput {
        self.input
    }
}
impl core::fmt::Debug for EdgeSnapshot {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("EdgeSnapshot")
            .field("frontier", &self.frontier())
            .field("data", &"[REDACTED]")
            .finish()
    }
}

#[cfg(test)]
mod tests;
