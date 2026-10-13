//! A full scan's admitted records, kept for the statements that follow it on
//! the same snapshot (fgdb-61p1n).
//!
//! Under a capability that hides nothing, every whole-graph statement admits
//! the same records in the same order with the same live calls: they depend
//! only on the snapshot and the cut. The first such statement records where
//! each admitted record sits and every live call it made. A later one replays
//! those calls exactly (`tape.rs`), so its charges, refusal points and
//! cancellation ordinals are a fresh scan's, then refills its tables from the
//! coordinates instead of merging both histories again. A scoped capability,
//! a bound root, the transaction overlay and every metadata projection run as
//! before.
//!
//! The sidecar follows `query_prism/cache.rs`: a weak owner on the live
//! snapshot allocation, no eviction of live entries, never a wait. It is
//! trusted-host state and is never handed to a token holder.

use super::tape::{Step, Tape};
use crate::Snapshot;
use crate::query::prism::LiveCache;
use fgdb_delta_types::RelationId;
use fgdb_types::{CommitSeq, EId, VId};
use std::mem::size_of;
use std::sync::Arc;

/// Retained coordinate bytes over all live snapshots. A graph too large for
/// this is admitted afresh by every statement, as before.
const ADMITTED_BYTES: usize = 128 * 1024 * 1024;

pub(super) static ADMITTED: LiveCache<Snapshot, AdmissionKey, Arc<AdmittedCore>> =
    LiveCache::new(ADMITTED_BYTES);

/// What a full scan's admitted set depends on under a scope that hides nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct AdmissionKey {
    pub(super) at: CommitSeq,
    /// Whether the plan reads edges: a vertex-only plan admits none.
    pub(super) edges: bool,
}

/// One admitted edge: its identity and where its winning statement sits.
#[derive(Clone, Copy, Debug)]
pub(super) struct CoreEdge {
    pub(super) eid: EId,
    pub(super) src: VId,
    pub(super) dst: VId,
    pub(super) relation: RelationId,
    pub(super) block: u32,
    pub(super) row: u32,
}

/// A full scan's admitted records, in admission order (ascending identity),
/// as coordinates into the snapshot allocation that produced them.
#[derive(Debug, Default)]
pub(super) struct AdmittedCore {
    /// Identity, patch and row of each admitted vertex.
    pub(super) vertices: Vec<(VId, u32, u32)>,
    pub(super) edges: Vec<CoreEdge>,
    pub(super) records: u64,
    pub(super) tape: Tape,
}

impl AdmittedCore {
    pub(super) fn bytes(&self) -> usize {
        size_of::<Self>()
            + self.vertices.capacity() * size_of::<(VId, u32, u32)>()
            + self.edges.capacity() * size_of::<CoreEdge>()
            + self.tape.bytes()
    }
}

/// The core a full scan is recording. A coordinate or label count that does
/// not fit the compact form abandons the recording; the scan itself goes on.
#[derive(Default)]
pub(super) struct Recording {
    core: AdmittedCore,
    abandoned: bool,
}

impl Recording {
    /// A charge-free source poll.
    pub(super) fn poll(&mut self) {
        self.core.tape.poll();
    }

    /// An admitted vertex and the number of its labels that were charged.
    pub(super) fn vertex(&mut self, vid: VId, patch: usize, row: usize, labels: usize) {
        match (
            u32::try_from(patch),
            u32::try_from(row),
            u32::try_from(labels),
        ) {
            (Ok(patch), Ok(row), Ok(labels)) => {
                self.core.tape.emit(Some(Step::Vertex(labels)));
                self.core.vertices.push((vid, patch, row));
            }
            _ => self.abandoned = true,
        }
    }

    /// An admitted edge.
    pub(super) fn edge(
        &mut self,
        (eid, src, relation, dst): (EId, VId, RelationId, VId),
        block: usize,
        row: usize,
    ) {
        match (u32::try_from(block), u32::try_from(row)) {
            (Ok(block), Ok(row)) => {
                self.core.tape.emit(Some(Step::Edge));
                self.core.edges.push(CoreEdge {
                    eid,
                    src,
                    dst,
                    relation,
                    block,
                    row,
                });
            }
            _ => self.abandoned = true,
        }
    }

    /// The finished core of a scan that admitted `records`, unless abandoned.
    pub(super) fn finish(self, records: u64) -> Option<AdmittedCore> {
        if self.abandoned {
            return None;
        }
        let mut core = self.core;
        core.records = records;
        core.tape = core.tape.finish();
        core.vertices.shrink_to_fit();
        core.edges.shrink_to_fit();
        Some(core)
    }
}

#[cfg(test)]
#[path = "admission_tests.rs"]
mod tests;
