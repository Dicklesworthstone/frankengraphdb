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
    SealedIncomingCursor, SealedIncomingIndex, SealedLimits, SealedPartition, SealedScope,
    check_limit, image,
};
use crate::edge_props::{BlockProps, EdgePropertyRow, admitted_row_bytes};
use crate::root::RootError;
use crate::{AdjacencyEntry, PartitionRootVersion};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_types::{CanonicalPropertyValue, CommitSeq, EId, QueryCx, VId};
use std::borrow::Borrow;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

type OverlayEntryRow = (AdjacencyEntry, Arc<EdgePropertyRow>);
type BorrowedOverlayRow<'a> = (AdjacencyEntry, &'a [(PropertyKeyId, CanonicalPropertyValue)]);

fn admit_delta_descriptor(
    rows: &BTreeMap<VId, Vec<OverlayEntryRow>>,
    vertex: VId,
    limit: usize,
) -> Result<(), SealedError> {
    if !rows.contains_key(&vertex) {
        let requested = rows.len().checked_add(1).ok_or(SealedError::SizeOverflow)?;
        check_limit("overlay descriptors", requested, limit)?;
    }
    Ok(())
}

fn push_delta(
    rows: &mut BTreeMap<VId, Vec<OverlayEntryRow>>,
    vertex: VId,
    entry: AdjacencyEntry,
    properties: Arc<EdgePropertyRow>,
) -> Result<(), SealedError> {
    let row = rows.entry(vertex).or_default();
    row.try_reserve(1)
        .map_err(|_| SealedError::AllocationFailed)?;
    row.push((entry, properties));
    Ok(())
}

/// Cumulative output admission, before each decoded row/property copy. The
/// image builder keeps its own identical final-image checks; this earlier
/// boundary also bounds the temporary descriptor family being fed to it.
struct MaterializedAdmission {
    limits: SealedLimits,
    incidences: usize,
    descriptors: usize,
    property_bytes: usize,
    last_src: Option<VId>,
}

impl MaterializedAdmission {
    fn new(limits: SealedLimits) -> Self {
        Self {
            limits,
            incidences: 0,
            descriptors: 0,
            property_bytes: 0,
            last_src: None,
        }
    }

    fn push(
        &mut self,
        rows: &mut Vec<(AdjacencyEntry, EdgePropertyRow)>,
        entry: AdjacencyEntry,
        properties: &[(PropertyKeyId, CanonicalPropertyValue)],
    ) -> Result<(), SealedError> {
        let incidences = self
            .incidences
            .checked_add(1)
            .ok_or(SealedError::SizeOverflow)?;
        check_limit("incidences", incidences, self.limits.max_incidences)?;
        u32::try_from(incidences).map_err(|_| SealedError::SizeOverflow)?;
        let descriptors = self
            .descriptors
            .checked_add(usize::from(self.last_src != Some(entry.src)))
            .ok_or(SealedError::SizeOverflow)?;
        check_limit("descriptors", descriptors, self.limits.max_rows)?;
        let row_bytes =
            usize::try_from(admitted_row_bytes(properties).map_err(SealedError::Property)?)
                .map_err(|_| SealedError::SizeOverflow)?;
        let property_bytes = self
            .property_bytes
            .checked_add(row_bytes)
            .ok_or(SealedError::SizeOverflow)?;
        check_limit(
            "property bytes",
            property_bytes,
            self.limits.max_property_bytes,
        )?;
        rows.try_reserve(1)
            .map_err(|_| SealedError::AllocationFailed)?;
        let mut owned = Vec::new();
        owned
            .try_reserve_exact(properties.len())
            .map_err(|_| SealedError::AllocationFailed)?;
        owned.extend(properties.iter().cloned());
        rows.push((entry, owned));
        self.incidences = incidences;
        self.descriptors = descriptors;
        self.property_bytes = property_bytes;
        self.last_src = Some(entry.src);
        Ok(())
    }
}

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
    ///
    /// Existing base-index admission is preserved. The limits also bound the
    /// selected suffix separately. Distinct suffix statements (including
    /// retirement evidence) consume the incidence allowance; repeated
    /// restatements replace the same slot.
    /// Each suffix descriptor map obeys the row allowance, and retained suffix
    /// properties share one canonical-byte allowance and one owned copy across
    /// cursor faces. These are population/payload bounds, not an allocator/RSS
    /// guarantee. Materialization admits the combined output cumulatively.
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
        let anchor = base.partition().anchor();
        // The base and suffix must have been admitted under the SAME root.
        // A relation name and caller-supplied horizon do not prove ancestry:
        // another root can carry a different run for that relation at the
        // same sequence. Check the opaque store-minted envelope before any
        // incoming-index construction or suffix materialization. The original
        // run's source root may differ legitimately after intervening writes.
        if anchor.relation() != Some(relation)
            || anchor.admitted_envelope_root() != Some(envelope_root)
            || anchor.admitted_publication() != envelope_publication
            || anchor.verified_through() != verified_through
            || anchor.scope().publication > verified_through
            || verified_through >= envelope_publication
        {
            return Err(SealedError::ImageMismatch);
        }
        let floor = anchor.scope().floor;
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

        // Collapse statements under the canonical last-block-wins rule.
        // Retain borrowed property rows until winners and their allowances
        // are known; a superseded statement never makes a recursive copy.
        let mut winning: BTreeMap<(EId, CommitSeq), BorrowedOverlayRow<'_>> = BTreeMap::new();
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
                let key = (entry.eid, entry.created_at);
                if !winning.contains_key(&key) {
                    let requested = winning
                        .len()
                        .checked_add(1)
                        .ok_or(SealedError::SizeOverflow)?;
                    check_limit("overlay statements", requested, limits.max_incidences)?;
                }
                let row = props_ref.map_or(&[][..], |properties| properties.props_ref(index));
                winning.insert(key, (*entry, row));
            }
        }

        let base_incoming = base.partition().incoming_index(
            cx,
            IncomingIndexLimits {
                max_rows: limits.max_rows,
                max_incidences: limits.max_incidences,
                max_workspace_bytes: usize::MAX / 2,
            },
        )?;
        let mut retired_base_at_as_of = BTreeSet::new();
        let mut base_retirements_after_as_of = BTreeMap::new();
        let mut delta_out_at_as_of: BTreeMap<VId, Vec<OverlayEntryRow>> = BTreeMap::new();
        let mut delta_in_at_as_of: BTreeMap<VId, Vec<OverlayEntryRow>> = BTreeMap::new();
        let mut delta_retained_from_as_of: BTreeMap<VId, Vec<OverlayEntryRow>> = BTreeMap::new();
        let mut property_bytes = 0usize;

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
            } else if entry.retired_at.is_none_or(|retired| retired > as_of) {
                // Admission precedes both the recursive copy and descriptor
                // allocation. Future retained versions count too, even though
                // neither visible cursor will yield them at this cut.
                admit_delta_descriptor(&delta_retained_from_as_of, entry.src, limits.max_rows)?;
                let visible = entry.visible_at(as_of);
                if visible {
                    admit_delta_descriptor(&delta_out_at_as_of, entry.src, limits.max_rows)?;
                    admit_delta_descriptor(&delta_in_at_as_of, entry.dst, limits.max_rows)?;
                }
                let bytes = usize::try_from(
                    admitted_row_bytes(row)
                        .map_err(|error| SealedError::History(RootError::PropertyPatch(error)))?,
                )
                .map_err(|_| SealedError::SizeOverflow)?;
                property_bytes = property_bytes
                    .checked_add(bytes)
                    .ok_or(SealedError::SizeOverflow)?;
                check_limit(
                    "overlay property bytes",
                    property_bytes,
                    limits.max_property_bytes,
                )?;
                let mut owned = Vec::new();
                owned
                    .try_reserve_exact(row.len())
                    .map_err(|_| SealedError::AllocationFailed)?;
                owned.extend(row.iter().cloned());
                let row = Arc::new(owned);
                if visible {
                    push_delta(&mut delta_out_at_as_of, entry.src, entry, Arc::clone(&row))?;
                    push_delta(&mut delta_in_at_as_of, entry.dst, entry, Arc::clone(&row))?;
                }
                push_delta(&mut delta_retained_from_as_of, entry.src, entry, row)?;
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
    /// Complete source descriptors feed the incremental image builder in order;
    /// decoded rows from a completed descriptor are released before the next.
    /// Combined incidence, descriptor and canonical-property allowances are
    /// checked before every copied row. Final encoded-image limits remain the
    /// ordinary sealer's, without a whole-operation memory/RSS claim.
    pub fn materialize_partition(
        &self,
        cx: &QueryCx,
        limits: SealedLimits,
    ) -> Result<SealedPartition, SealedError> {
        cx.checkpoint().map_err(SealedError::Interrupted)?;
        let base_rows = &self.base.partition().image.rows;
        let mut base_idx = 0usize;
        let mut delta_iter = self.delta_retained_from_as_of.iter().peekable();
        let mut checkpoint = || cx.checkpoint().map_err(SealedError::Interrupted);
        let mut builder = image::ImageBuilder::new(limits)?;
        let mut admission = MaterializedAdmission::new(limits);

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
            let mut merged = Vec::new();

            loop {
                checkpoint()?;
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
                            admission.push(&mut merged, edge.entry, edge.properties)?;
                        } else {
                            delta_pos += 1;
                            admission.push(&mut merged, *d_entry, d_row.as_slice())?;
                        }
                    }
                    (Some(_), None) => {
                        let edge = peeked_base.take().expect("peeked above");
                        admission.push(&mut merged, edge.entry, edge.properties)?;
                    }
                    (None, Some((d_entry, d_row))) => {
                        delta_pos += 1;
                        admission.push(&mut merged, *d_entry, d_row.as_slice())?;
                    }
                    (None, None) => break,
                }
            }
            builder.push_retained(merged, &mut checkpoint)?;
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
        let built_image = builder.finish(&mut checkpoint)?;
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
