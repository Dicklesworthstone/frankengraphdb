//! Demand-driven vertex histories in one admitted immutable generation.

use super::{History, IndexMap, IndexNode, SourceEvent, VertexCursor};
use fgdb_strata::vertex::{VertexPatchRows, VertexRow};
use fgdb_types::{CommitSeq, VId};
use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::sync::Arc;

/// An owned AVL spine lets a snapshot-owning stream walk each identity once
/// without a self-borrow, a candidate bag or repeated root descents. Opening
/// allocates nothing; the first demand reserves at most the tree's height.
/// The source pins the generation for this cursor's entire lifetime.
#[derive(Default)]
pub(super) struct VertexHistoryScanCursor {
    spine: Vec<Arc<IndexNode<VId, History>>>,
    pending: Option<Arc<IndexNode<VId, History>>>,
    current: Option<Arc<IndexNode<VId, History>>>,
    initialized: bool,
}

impl VertexHistoryScanCursor {
    pub(super) fn next<E>(
        &mut self,
        histories: &IndexMap<VId, History>,
        control: &mut impl FnMut(SourceEvent) -> Result<(), E>,
    ) -> Result<Option<VId>, E> {
        if !self.initialized {
            // All later spines fit this immutable tree's height. Charge the
            // entire reservation before allocating, including unused slots.
            let height = usize::from(histories.height());
            for _ in 0..height {
                control(SourceEvent::ScratchEntry)?;
            }
            let mut spine = Vec::with_capacity(height);
            Self::descend(&mut spine, histories.0.as_ref(), control)?;
            control(SourceEvent::Work)?;
            self.spine = spine;
            self.initialized = true;
        } else {
            let retained = self.spine.len();
            let prepared = Self::descend(&mut self.spine, self.pending.as_ref(), control)
                .and_then(|()| control(SourceEvent::Work));
            if let Err(error) = prepared {
                // A refused descent neither loses pending nodes nor replaces
                // the last delivered history. Retrying yields the same row.
                self.spine.truncate(retained);
                return Err(error);
            }
            self.pending = None;
        }

        let Some(current) = self.spine.pop() else {
            self.current = None;
            return Ok(None);
        };
        self.pending = current.right.0.clone();
        let vid = current.key;
        self.current = Some(current);
        Ok(Some(vid))
    }

    fn descend<E>(
        spine: &mut Vec<Arc<IndexNode<VId, History>>>,
        mut node: Option<&Arc<IndexNode<VId, History>>>,
        control: &mut impl FnMut(SourceEvent) -> Result<(), E>,
    ) -> Result<(), E> {
        while let Some(current) = node {
            control(SourceEvent::Work)?;
            spine.push(Arc::clone(current));
            node = current.left.0.as_ref();
        }
        Ok(())
    }

    /// Only the current root can reuse this history. A correlated probe may
    /// resolve a different identity before the next pull and must still seek.
    pub(super) fn history(&self, vid: VId) -> Option<&History> {
        self.current
            .as_ref()
            .filter(|current| current.key == vid)
            .map(|current| &current.value)
    }
}

/// A cold scan merges the admitted sorted patches directly. Opening and
/// stopping before the first pull allocate nothing; first demand retains at
/// most one head per patch, never a graph-sized history or property index.
/// The pending identity survives a refused checkpoint so that retry cannot
/// skip either an unfinished history or the last delivered row.
#[derive(Default)]
pub(super) struct VertexPatchScanCursor {
    heads: BinaryHeap<VertexCursor>,
    pending: Option<PatchVertex>,
    current: Option<PatchVertex>,
    initialized: bool,
}

struct PatchVertex {
    vid: VId,
    winner: Option<(usize, usize)>,
}

impl VertexPatchScanCursor {
    pub(super) fn next<E>(
        &mut self,
        patches: &[VertexPatchRows],
        as_of: CommitSeq,
        control: &mut impl FnMut(SourceEvent) -> Result<(), E>,
    ) -> Result<Option<VId>, E> {
        if !self.initialized {
            // Reserve the complete capacity before allocation, even for empty
            // patches. All subsequent head replacements reuse this storage.
            for _ in patches {
                control(SourceEvent::ScratchEntry)?;
            }
            let mut heads = BinaryHeap::with_capacity(patches.len());
            for (patch, rows) in patches.iter().enumerate() {
                control(SourceEvent::Work)?;
                if let Some(row) = rows.first() {
                    heads.push(Reverse((row.vid, row.created_at, patch, 0)));
                }
            }
            self.heads = heads;
            self.initialized = true;
        }
        let vid = match &self.pending {
            Some(pending) => pending.vid,
            None => {
                control(SourceEvent::Work)?;
                let Some(&Reverse((vid, ..))) = self.heads.peek() else {
                    self.current = None;
                    return Ok(None);
                };
                self.pending = Some(PatchVertex { vid, winner: None });
                vid
            }
        };
        while let Some(mut head) = self.heads.peek_mut() {
            let Reverse((next, created_at, patch, row)) = *head;
            if next != vid {
                break;
            }
            control(SourceEvent::Work)?;
            // Heap order is (identity, creation, publication patch, row).
            // Later retirement restatements therefore supersede their live
            // copies, exactly as the full snapshot merge does.
            if created_at <= as_of {
                self.pending.as_mut().expect("pending identity").winner = Some((patch, row));
            }
            if let Some(next) = patches[patch].get(row + 1) {
                *head = Reverse((next.vid, next.created_at, patch, row + 1));
            } else {
                std::collections::binary_heap::PeekMut::pop(head);
            }
        }
        control(SourceEvent::Work)?;
        self.current = self.pending.take();
        Ok(Some(vid))
    }

    /// `Some(None)` is the current identity with no visible winner. `None`
    /// means an independent probe and must use its ordinary point lookup.
    pub(super) fn current_row<'a>(
        &self,
        patches: &'a [VertexPatchRows],
        vid: VId,
        as_of: CommitSeq,
    ) -> Option<Option<&'a VertexRow>> {
        self.current
            .as_ref()
            .filter(|row| row.vid == vid)
            .map(|row| {
                row.winner
                    .map(|(patch, row)| &patches[patch][row])
                    .filter(|row| row.visible_at(as_of))
            })
    }
}

#[cfg(test)]
mod tests;
