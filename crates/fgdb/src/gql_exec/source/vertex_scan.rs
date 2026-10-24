//! Demand-driven vertex histories in one admitted immutable generation.

use super::{History, IndexMap, IndexNode, SourceEvent};
use fgdb_types::VId;
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

#[cfg(test)]
mod tests;
