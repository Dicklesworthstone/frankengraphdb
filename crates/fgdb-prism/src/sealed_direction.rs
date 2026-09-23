//! Merge authenticated incidence faces BEFORE applying multigraph reduction.
//! An incoming edge keeps its immutable endpoints/properties; only the neighbor
//! selected by this projection changes. At most two raw lookaheads are retained.

use super::{Directedness, SealedProjectionError, SealedProjectionSpec};
use crate::sealed_control::Control;
use fgdb_strata::tiered::sealed::{
    SealedCursor, SealedEdge, SealedIncomingCursor, SealedIncomingIndex, SealedPartition,
};
use fgdb_types::{CommitSeq, EId, VId};

pub(super) struct Incidences<'a> {
    outgoing: Option<SealedCursor<'a>>,
    incoming: Option<SealedIncomingCursor<'a>>,
    out_head: Option<SealedEdge<'a>>,
    in_head: Option<SealedEdge<'a>>,
    source: VId,
    undirected: bool,
    finished: bool,
}

impl<'a> Incidences<'a> {
    pub(super) fn open(
        cx: &Control<'_>,
        partition: &'a SealedPartition,
        incoming: Option<&'a SealedIncomingIndex>,
        source: VId,
        config: &SealedProjectionSpec,
        relation_visible: bool,
        lower: Option<VId>,
    ) -> Result<Self, SealedProjectionError> {
        cx.checkpoint()?;
        let relation = config.selection.relation.ok_or(SealedProjectionError::RelationRequired)?;
        let direction = config.projection.directedness;
        if !relation_visible {
            // A forbidden relation never opens either incidence face. Keep the
            // original authenticated image untouched, rather than inventing
            // an empty source or filtering a computed neighbor/degree result.
            return Ok(Self {
                outgoing: None, incoming: None, out_head: None, in_head: None,
                source, undirected: direction == Directedness::Undirected,
                finished: false,
            });
        }
        let outgoing = if direction == Directedness::Reversed {
            None
        } else {
            cx.checkpoint()?;
            Some(partition.row_from(cx.query, source, relation, config.as_of, lower)
                .map_err(SealedProjectionError::Read)?)
        };
        let incoming = if direction == Directedness::Directed {
            None
        } else {
            let index = incoming.ok_or(SealedProjectionError::UnsupportedDirectedness(direction))?;
            // This is the private construction invariant, not permission to
            // substitute an equal-looking image from another source generation.
            if index.source_anchor() != partition.anchor()
                || !index.source().shares_image_with(partition)
            {
                return Err(SealedProjectionError::Read(
                    fgdb_strata::tiered::sealed::SealedError::ImageMismatch,
                ));
            }
            cx.checkpoint()?;
            Some(index.row_from(cx.query, source, relation, config.as_of, lower)
                .map_err(SealedProjectionError::Read)?)
        };
        Ok(Self {
            outgoing, incoming, out_head: None, in_head: None, source,
            undirected: direction == Directedness::Undirected, finished: false,
        })
    }

    pub(super) fn next(
        &mut self,
        cx: &Control<'_>,
    ) -> Result<Option<(VId, SealedEdge<'a>)>, SealedProjectionError> {
        if self.finished { return Ok(None); }
        let result = self.next_inner(cx);
        if result.is_err() || matches!(&result, Ok(None)) {
            self.finished = true;
            self.outgoing = None;
            self.incoming = None;
            self.out_head = None;
            self.in_head = None;
        }
        result
    }

    fn next_inner(
        &mut self,
        cx: &Control<'_>,
    ) -> Result<Option<(VId, SealedEdge<'a>)>, SealedProjectionError> {
        super::checkpoint(cx)?;
        if self.out_head.is_none() {
            if let Some(cursor) = &mut self.outgoing {
                self.out_head = cursor.next_with_checkpoint(cx.query, || cx.guard())?;
                if self.out_head.is_none() { self.outgoing = None; }
            }
        }
        if self.in_head.is_none() {
            if let Some(cursor) = &mut self.incoming {
                loop {
                    let edge = cursor.next_with_checkpoint(cx.query, || cx.guard())?;
                    match edge {
                        // The outgoing face owns a loop. Skip its incoming copy
                        // before property observation or any input-edge counting.
                        Some(edge) if self.undirected && edge.entry.src == self.source => continue,
                        other => { self.in_head = other; break; }
                    }
                }
                if self.in_head.is_none() { self.incoming = None; }
            }
        }
        let out_key = self.out_head.as_ref().map(|edge| key(edge.entry.dst, edge));
        let in_key = self.in_head.as_ref().map(|edge| key(edge.entry.src, edge));
        if prefer_outgoing(out_key, in_key) {
            Ok(self.out_head.take().map(|edge| (edge.entry.dst, edge)))
        } else {
            Ok(self.in_head.take().map(|edge| (edge.entry.src, edge)))
        }
    }
}

fn key(neighbor: VId, edge: &SealedEdge<'_>) -> (VId, EId, CommitSeq) {
    (neighbor, edge.entry.eid, edge.entry.created_at)
}

fn prefer_outgoing<K: Ord>(outgoing: Option<K>, incoming: Option<K>) -> bool {
    match (outgoing, incoming) {
        (Some(outgoing), Some(incoming)) => outgoing <= incoming,
        (Some(_), None) => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_order_uses_full_width_neighbor_then_eid_not_incidence_face() {
        let keys = [
            (VId(0), EId(u128::MAX), CommitSeq(7)),
            (VId(1), EId(0), CommitSeq(1)),
            (VId(1), EId(1), CommitSeq(1)),
            (VId(1), EId(u128::MAX), CommitSeq(3)),
            (VId(u128::MAX), EId(0), CommitSeq(2)),
        ];
        for mask in 0..(1 << keys.len()) {
            let mut outgoing = keys.iter().enumerate().filter(|(i, _)| mask & (1 << i) != 0)
                .map(|(_, &key)| key).peekable();
            let mut incoming = keys.iter().enumerate().filter(|(i, _)| mask & (1 << i) == 0)
                .map(|(_, &key)| key).peekable();
            let mut merged = Vec::new();
            while outgoing.peek().is_some() || incoming.peek().is_some() {
                let next = if prefer_outgoing(outgoing.peek().copied(), incoming.peek().copied()) {
                    outgoing.next()
                } else { incoming.next() };
                merged.push(next.unwrap());
            }
            assert_eq!(merged, keys, "face assignment {mask}");
        }
        assert!(!prefer_outgoing::<usize>(None, None));
        assert!(prefer_outgoing(Some(7), Some(7)));
    }
}
