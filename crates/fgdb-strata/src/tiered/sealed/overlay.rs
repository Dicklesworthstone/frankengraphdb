//! Post-seal `(Tier-R + Tier-D)` delta overlay and bidirectional neighbor cursor.
//!
//! When a relation carries an authenticated [`AdjRunSegment`] verified through
//! `s_seal` (`verified_through < as_of <= envelope_publication`), historical
//! adjacency in `[floor..=s_seal]` is already materialized in dual-face
//! Elias-Fano CSR (`E_out` + `E_in`). Post-seal mutations in `(s_seal..=as_of]`
//! remain in Tier-D delta blocks.
//!
//! [`SealedDeltaOverlay`] merges the immutable base run with only the post-seal
//! Tier-D delta statements (`last_seq > verified_through`):
//! - [`StrataNeighborCursor`] streams visible outgoing or incoming incidences at
//!   `as_of` in canonical `(neighbor, eid, created_at)` order with zero per-row
//!   allocation (`O(deg_CSR(v) + |Delta(v)|)`), reusing `FGRN-v2`'s durable
//!   `E_in` index on the incoming face.
//! - [`SealedDeltaOverlay::materialize_partition`] folds the base CSR and
//!   post-seal Tier-D delta into an `as_of`-anchored [`SealedPartition`] under
//!   `envelope_root` without retaining or compacting pre-seal Tier-D history.

use super::{
    AdjRunSegment, IncomingIndexLimits, SealedCursor, SealedEdge, SealedError,
    SealedIncomingCursor, SealedIncomingIndex, SealedLimits, SealedPartition, SealedScope, image,
};
use crate::edge_props::{BlockProps, EdgePropertyRow, admitted_row_bytes};
use crate::root::RootError;
use crate::{AdjacencyEntry, PartitionRootVersion};
use fgdb_delta_types::RelationId;
use fgdb_types::{CommitSeq, EId, QueryCx, VId};
use std::borrow::Borrow;
use std::collections::{BTreeMap, BTreeSet};

type OverlayEntryRow = (AdjacencyEntry, EdgePropertyRow);

/// Authenticated composition of a durable [`AdjRunSegment`] (`<= verified_through`)
/// with post-seal Tier-D delta blocks (`> verified_through`) up to `envelope_publication`.
#[derive(Clone, Debug)]
pub struct SealedDeltaOverlay {
    base: AdjRunSegment,
    base_incoming: SealedIncomingIndex,
    relation: RelationId,
    envelope_root: PartitionRootVersion,
    envelope_publication: CommitSeq,
    verified_through: CommitSeq,
    as_of: CommitSeq,
    retired_base_at_as_of: BTreeSet<(EId, CommitSeq)>,
    base_retirements_after_as_of: BTreeMap<(EId, CommitSeq), CommitSeq>,
    delta_out_at_as_of: BTreeMap<VId, Vec<OverlayEntryRow>>,
    delta_in_at_as_of: BTreeMap<VId, Vec<OverlayEntryRow>>,
    delta_retained_from_as_of: BTreeMap<VId, Vec<OverlayEntryRow>>,
}

impl SealedDeltaOverlay {
    /// Construct an authenticated post-seal delta overlay from an admitted base
    /// run and a store-minted [`crate::store::PostSealBlocks`] receipt.
    pub fn build(
        cx: &QueryCx,
        base: AdjRunSegment,
        envelope_root: PartitionRootVersion,
        verified_through: CommitSeq,
        as_of: CommitSeq,
        post_seal: &crate::store::PostSealBlocks,
        limits: SealedLimits,
    ) -> Result<Self, SealedError> {
        if post_seal.source_root != envelope_root
            || post_seal.relation != base.header().relation
            || post_seal.after_seq != verified_through
        {
            return Err(SealedError::ImageMismatch);
        }
        Self::from_post_seal_blocks(
            cx,
            base,
            envelope_root,
            post_seal.publication,
            verified_through,
            as_of,
            &post_seal.blocks,
            &post_seal.props,
            limits,
        )
    }

    /// Construct an authenticated post-seal delta overlay from an admitted base
    /// run and the root-validated post-seal Tier-D blocks (`last_seq > verified_through`)
    /// for `relation`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn from_post_seal_blocks(
        cx: &QueryCx,
        base: AdjRunSegment,
        envelope_root: PartitionRootVersion,
        envelope_publication: CommitSeq,
        verified_through: CommitSeq,
        as_of: CommitSeq,
        post_seal_blocks: &[Vec<AdjacencyEntry>],
        post_seal_props: &[Option<impl Borrow<BlockProps>>],
        limits: SealedLimits,
    ) -> Result<Self, SealedError> {
        cx.checkpoint().map_err(SealedError::Interrupted)?;
        let relation = base.header().relation;
        let floor = base.header().scope.floor;
        if as_of < floor || as_of <= verified_through || as_of > envelope_publication {
            return Err(SealedError::SnapshotOutsideAnchor {
                requested: as_of,
                floor,
                publication: envelope_publication,
            });
        }
        if post_seal_blocks.len() != post_seal_props.len() {
            return Err(SealedError::History(RootError::BlockPropsArity {
                blocks: post_seal_blocks.len(),
                props: post_seal_props.len(),
            }));
        }

        let base_incoming = base.partition().incoming_index(
            cx,
            IncomingIndexLimits {
                max_rows: limits.max_rows,
                max_incidences: limits.max_incidences,
                max_workspace_bytes: usize::MAX / 2,
            },
        )?;

        // Collapse post-seal statements on `relation` under Strata's canonical
        // last-block-wins rule keyed by `(eid, created_at)`.
        let mut winning: BTreeMap<(EId, CommitSeq), OverlayEntryRow> = BTreeMap::new();
        for (block, props) in post_seal_blocks.iter().zip(post_seal_props) {
            cx.checkpoint().map_err(SealedError::Interrupted)?;
            let props_ref = props.as_ref().map(Borrow::borrow);
            for (index, entry) in block.iter().enumerate() {
                if index.is_multiple_of(128) {
                    cx.checkpoint().map_err(SealedError::Interrupted)?;
                }
                if entry.relation != relation {
                    continue;
                }
                let row = props_ref.map(|p| p.props_of(index)).unwrap_or_default();
                winning.insert((entry.eid, entry.created_at), (*entry, row));
            }
        }

        let mut retired_base_at_as_of = BTreeSet::new();
        let mut base_retirements_after_as_of = BTreeMap::new();
        let mut delta_out_at_as_of: BTreeMap<VId, Vec<OverlayEntryRow>> = BTreeMap::new();
        let mut delta_in_at_as_of: BTreeMap<VId, Vec<OverlayEntryRow>> = BTreeMap::new();
        let mut delta_retained_from_as_of: BTreeMap<VId, Vec<OverlayEntryRow>> = BTreeMap::new();

        for (step, ((eid, created_at), (entry, row))) in winning.into_iter().enumerate() {
            if step.is_multiple_of(128) {
                cx.checkpoint().map_err(SealedError::Interrupted)?;
            }
            if created_at <= verified_through {
                if let Some(retired) = entry.retired_at
                    && retired > verified_through
                {
                    if retired <= as_of {
                        retired_base_at_as_of.insert((eid, created_at));
                    } else {
                        base_retirements_after_as_of.insert((eid, created_at), retired);
                    }
                }
            } else {
                if entry.retired_at.is_none_or(|retired| retired > as_of) {
                    admitted_row_bytes(&row)
                        .map_err(|err| SealedError::History(RootError::PropertyPatch(err)))?;
                    if entry.visible_at(as_of) {
                        delta_out_at_as_of
                            .entry(entry.src)
                            .or_default()
                            .push((entry, row.clone()));
                        delta_in_at_as_of
                            .entry(entry.dst)
                            .or_default()
                            .push((entry, row.clone()));
                    }
                    delta_retained_from_as_of
                        .entry(entry.src)
                        .or_default()
                        .push((entry, row));
                }
            }
        }

        for rows in delta_out_at_as_of.values_mut() {
            cx.checkpoint().map_err(SealedError::Interrupted)?;
            rows.sort_unstable_by_key(|(entry, _)| (entry.dst, entry.eid, entry.created_at));
        }
        for rows in delta_in_at_as_of.values_mut() {
            cx.checkpoint().map_err(SealedError::Interrupted)?;
            rows.sort_unstable_by_key(|(entry, _)| (entry.src, entry.eid, entry.created_at));
        }
        for rows in delta_retained_from_as_of.values_mut() {
            cx.checkpoint().map_err(SealedError::Interrupted)?;
            rows.sort_unstable_by_key(|(entry, _)| (entry.dst, entry.eid, entry.created_at));
        }

        Ok(Self {
            base,
            base_incoming,
            relation,
            envelope_root,
            envelope_publication,
            verified_through,
            as_of,
            retired_base_at_as_of,
            base_retirements_after_as_of,
            delta_out_at_as_of,
            delta_in_at_as_of,
            delta_retained_from_as_of,
        })
    }

    pub const fn relation(&self) -> RelationId {
        self.relation
    }

    pub const fn verified_through(&self) -> CommitSeq {
        self.verified_through
    }

    pub const fn as_of(&self) -> CommitSeq {
        self.as_of
    }

    pub const fn envelope_root(&self) -> PartitionRootVersion {
        self.envelope_root
    }

    pub const fn envelope_publication(&self) -> CommitSeq {
        self.envelope_publication
    }

    pub fn base_run(&self) -> &AdjRunSegment {
        &self.base
    }

    /// Number of base incidences retired in `(verified_through..=as_of]`.
    pub fn retired_base_count(&self) -> usize {
        self.retired_base_at_as_of.len()
    }

    /// Number of post-seal delta incidences visible at `as_of`.
    pub fn visible_delta_count(&self) -> usize {
        self.delta_out_at_as_of.values().map(Vec::len).sum()
    }

    /// Open a zero-allocation merged `(Tier-R + Tier-D)` outgoing cursor for `src` at `as_of`.
    pub fn outgoing_cursor<'a>(
        &'a self,
        cx: &QueryCx,
        src: VId,
    ) -> Result<StrataNeighborCursor<'a>, SealedError> {
        self.outgoing_cursor_from(cx, src, None)
    }

    /// Open a lower-bounded merged `(Tier-R + Tier-D)` outgoing cursor for `src` at `as_of`.
    pub fn outgoing_cursor_from<'a>(
        &'a self,
        cx: &QueryCx,
        src: VId,
        lower_dst: Option<VId>,
    ) -> Result<StrataNeighborCursor<'a>, SealedError> {
        let base_cursor = self.base.partition().row_from(
            cx,
            src,
            self.relation,
            self.verified_through,
            lower_dst,
        )?;
        let delta_slice = self.delta_out_at_as_of.get(&src).map_or(&[][..], |rows| {
            let start = match lower_dst {
                Some(bound) => rows.partition_point(|(entry, _)| entry.dst < bound),
                None => 0,
            };
            &rows[start..]
        });
        Ok(StrataNeighborCursor {
            face: CursorFace::Outgoing {
                base: base_cursor,
                peeked: None,
            },
            delta: delta_slice,
            retired_base: &self.retired_base_at_as_of,
            retirements_after_as_of: &self.base_retirements_after_as_of,
            finished: false,
        })
    }

    /// Open a zero-allocation merged `(Tier-R + Tier-D)` incoming cursor for `dst` at `as_of`,
    /// backed by `FGRN-v2`'s durable `E_in` Elias-Fano CSR index.
    pub fn incoming_cursor<'a>(
        &'a self,
        cx: &QueryCx,
        dst: VId,
    ) -> Result<StrataNeighborCursor<'a>, SealedError> {
        self.incoming_cursor_from(cx, dst, None)
    }

    /// Open a lower-bounded merged `(Tier-R + Tier-D)` incoming cursor for `dst` at `as_of`.
    pub fn incoming_cursor_from<'a>(
        &'a self,
        cx: &QueryCx,
        dst: VId,
        lower_src: Option<VId>,
    ) -> Result<StrataNeighborCursor<'a>, SealedError> {
        let base_cursor = self.base_incoming.row_from(
            cx,
            dst,
            self.relation,
            self.verified_through,
            lower_src,
        )?;
        let delta_slice = self.delta_in_at_as_of.get(&dst).map_or(&[][..], |rows| {
            let start = match lower_src {
                Some(bound) => rows.partition_point(|(entry, _)| entry.src < bound),
                None => 0,
            };
            &rows[start..]
        });
        Ok(StrataNeighborCursor {
            face: CursorFace::Incoming {
                base: base_cursor,
                peeked: None,
            },
            delta: delta_slice,
            retired_base: &self.retired_base_at_as_of,
            retirements_after_as_of: &self.base_retirements_after_as_of,
            finished: false,
        })
    }

    /// Fold `(base CSR at verified_through \ retired_base) U post_seal_delta`
    /// into an `as_of`-floored [`SealedPartition`] anchored to `envelope_root`,
    /// preserving exact byte/anchor parity with a full Tier-D seal while reading
    /// historical `[floor..=verified_through]` rows from the compressed run.
    pub fn materialize_partition(
        &self,
        cx: &QueryCx,
        limits: SealedLimits,
    ) -> Result<SealedPartition, SealedError> {
        cx.checkpoint().map_err(SealedError::Interrupted)?;
        let base_rows = &self.base.partition().image.rows;
        let mut base_idx = 0usize;
        let mut delta_iter = self.delta_retained_from_as_of.iter().peekable();
        let mut merged: Vec<OverlayEntryRow> = Vec::new();

        loop {
            cx.checkpoint().map_err(SealedError::Interrupted)?;
            let next_base_src = base_rows.get(base_idx).map(|row| row.key.src);
            let next_delta_src = delta_iter.peek().map(|(src, _)| **src);
            let src = match (next_base_src, next_delta_src) {
                (Some(b), Some(d)) => {
                    if b <= d {
                        base_idx += 1;
                    }
                    b.min(d)
                }
                (Some(b), None) => {
                    base_idx += 1;
                    b
                }
                (None, Some(d)) => d,
                (None, None) => break,
            };
            let delta_slice = if next_delta_src == Some(src) {
                delta_iter
                    .next()
                    .map_or(&[][..], |(_, rows)| rows.as_slice())
            } else {
                &[][..]
            };

            let mut base_cursor = if next_base_src == Some(src) {
                Some(
                    self.base
                        .partition()
                        .row(cx, src, self.relation, self.verified_through)?,
                )
            } else {
                None
            };
            let mut peeked_base: Option<SealedEdge<'_>> = None;
            let mut delta_pos = 0usize;

            loop {
                if peeked_base.is_none()
                    && let Some(cursor) = base_cursor.as_mut()
                {
                    while let Some(mut edge) = cursor.next(cx)? {
                        let key = (edge.entry.eid, edge.entry.created_at);
                        if self.retired_base_at_as_of.contains(&key) {
                            continue;
                        }
                        if let Some(&retired) = self.base_retirements_after_as_of.get(&key) {
                            edge.entry.retired_at = Some(retired);
                        }
                        peeked_base = Some(edge);
                        break;
                    }
                }
                let next_delta = delta_slice.get(delta_pos);
                match (&peeked_base, next_delta) {
                    (Some(b), Some((d_entry, d_row))) => {
                        let b_key = (b.entry.dst, b.entry.eid, b.entry.created_at);
                        let d_key = (d_entry.dst, d_entry.eid, d_entry.created_at);
                        if b_key <= d_key {
                            let edge = peeked_base.take().expect("peeked above");
                            merged
                                .try_reserve(1)
                                .map_err(|_| SealedError::AllocationFailed)?;
                            merged.push((edge.entry, edge.properties.to_vec()));
                        } else {
                            delta_pos += 1;
                            merged
                                .try_reserve(1)
                                .map_err(|_| SealedError::AllocationFailed)?;
                            merged.push((*d_entry, d_row.clone()));
                        }
                    }
                    (Some(_), None) => {
                        let edge = peeked_base.take().expect("peeked above");
                        merged
                            .try_reserve(1)
                            .map_err(|_| SealedError::AllocationFailed)?;
                        merged.push((edge.entry, edge.properties.to_vec()));
                    }
                    (None, Some((d_entry, d_row))) => {
                        delta_pos += 1;
                        merged
                            .try_reserve(1)
                            .map_err(|_| SealedError::AllocationFailed)?;
                        merged.push((*d_entry, d_row.clone()));
                    }
                    (None, None) => break,
                }
            }
        }

        let base_scope = self.base.header().scope;
        let scope = SealedScope {
            source_root: self.envelope_root,
            graph: base_scope.graph,
            branch: base_scope.branch,
            partition: base_scope.partition,
            floor: self.as_of,
            publication: self.envelope_publication,
        };
        let mut checkpoint = || cx.checkpoint().map_err(SealedError::Interrupted);
        let built_image = image::build_retained(merged, limits, &mut checkpoint)?;
        let mut partition = SealedPartition::finish_scoped(
            scope,
            Some(self.relation),
            built_image,
            limits,
            &mut checkpoint,
        )?;
        let incoming = partition.incoming_index(
            cx,
            IncomingIndexLimits {
                max_rows: limits.max_rows,
                max_incidences: limits.max_incidences,
                max_workspace_bytes: usize::MAX / 2,
            },
        )?;
        partition.set_durable_incoming(incoming.index_arc());
        Ok(partition)
    }
}

enum CursorFace<'a> {
    Outgoing {
        base: SealedCursor<'a>,
        peeked: Option<SealedEdge<'a>>,
    },
    Incoming {
        base: SealedIncomingCursor<'a>,
        peeked: Option<SealedEdge<'a>>,
    },
}

/// Allocation-free sorted merge cursor over a durable sealed CSR face (`E_out`
/// or `E_in`) and post-seal Tier-D delta statements at an authorized `as_of` cut.
pub struct StrataNeighborCursor<'a> {
    face: CursorFace<'a>,
    delta: &'a [OverlayEntryRow],
    retired_base: &'a BTreeSet<(EId, CommitSeq)>,
    retirements_after_as_of: &'a BTreeMap<(EId, CommitSeq), CommitSeq>,
    finished: bool,
}

impl<'a> StrataNeighborCursor<'a> {
    pub fn next(&mut self, cx: &QueryCx) -> Result<Option<SealedEdge<'a>>, SealedError> {
        if self.finished {
            return Ok(None);
        }
        if let Err(err) = cx.checkpoint().map_err(SealedError::Interrupted) {
            self.finished = true;
            return Err(err);
        }

        match &mut self.face {
            CursorFace::Outgoing { base, peeked } => {
                if peeked.is_none() {
                    loop {
                        match base.next(cx) {
                            Ok(Some(mut edge)) => {
                                let key = (edge.entry.eid, edge.entry.created_at);
                                if self.retired_base.contains(&key) {
                                    continue;
                                }
                                if let Some(&retired) = self.retirements_after_as_of.get(&key) {
                                    edge.entry.retired_at = Some(retired);
                                }
                                *peeked = Some(edge);
                                break;
                            }
                            Ok(None) => break,
                            Err(err) => {
                                self.finished = true;
                                return Err(err);
                            }
                        }
                    }
                }
                match (peeked.as_ref(), self.delta.first()) {
                    (Some(b), Some((d_entry, d_row))) => {
                        let b_key = (b.entry.dst, b.entry.eid, b.entry.created_at);
                        let d_key = (d_entry.dst, d_entry.eid, d_entry.created_at);
                        if b_key <= d_key {
                            Ok(peeked.take())
                        } else {
                            self.delta = &self.delta[1..];
                            Ok(Some(SealedEdge {
                                entry: *d_entry,
                                properties: d_row.as_slice(),
                            }))
                        }
                    }
                    (Some(_), None) => Ok(peeked.take()),
                    (None, Some((d_entry, d_row))) => {
                        self.delta = &self.delta[1..];
                        Ok(Some(SealedEdge {
                            entry: *d_entry,
                            properties: d_row.as_slice(),
                        }))
                    }
                    (None, None) => {
                        self.finished = true;
                        Ok(None)
                    }
                }
            }
            CursorFace::Incoming { base, peeked } => {
                if peeked.is_none() {
                    loop {
                        match base.next(cx) {
                            Ok(Some(mut edge)) => {
                                let key = (edge.entry.eid, edge.entry.created_at);
                                if self.retired_base.contains(&key) {
                                    continue;
                                }
                                if let Some(&retired) = self.retirements_after_as_of.get(&key) {
                                    edge.entry.retired_at = Some(retired);
                                }
                                *peeked = Some(edge);
                                break;
                            }
                            Ok(None) => break,
                            Err(err) => {
                                self.finished = true;
                                return Err(err);
                            }
                        }
                    }
                }
                match (peeked.as_ref(), self.delta.first()) {
                    (Some(b), Some((d_entry, d_row))) => {
                        let b_key = (b.entry.src, b.entry.eid, b.entry.created_at);
                        let d_key = (d_entry.src, d_entry.eid, d_entry.created_at);
                        if b_key <= d_key {
                            Ok(peeked.take())
                        } else {
                            self.delta = &self.delta[1..];
                            Ok(Some(SealedEdge {
                                entry: *d_entry,
                                properties: d_row.as_slice(),
                            }))
                        }
                    }
                    (Some(_), None) => Ok(peeked.take()),
                    (None, Some((d_entry, d_row))) => {
                        self.delta = &self.delta[1..];
                        Ok(Some(SealedEdge {
                            entry: *d_entry,
                            properties: d_row.as_slice(),
                        }))
                    }
                    (None, None) => {
                        self.finished = true;
                        Ok(None)
                    }
                }
            }
        }
    }
}
