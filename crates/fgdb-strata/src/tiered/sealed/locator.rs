//! Query-local EId access derived only from an admitted immutable image.
//!
//! The directory retains content-version coordinates, not another adjacency or
//! property image. Root and incidence candidates include invisible history;
//! resolving a candidate is a separate exact-cut operation. A consumer needing
//! the complete publication's candidate domain must additionally admit a
//! floor-zero image verified through that publication. A materialized visible
//! delta overlay does not prove that stronger domain.

use super::{Image, RowScan, SealedEdge, SealedError, SealedPartition, check_limit};
use crate::AdjacencyEntry;
use crate::tiered::edge_scan::BufferedEdgeDirection;
use crate::tiered::memory::MemoryCharge;
use fgdb_delta_types::RelationId;
use fgdb_types::{CommitSeq, EId, QueryCx, VId};
use std::cmp::Ordering;

#[derive(Clone, Copy, Eq, Ord, PartialEq, PartialOrd)]
struct VersionRef {
    eid: EId,
    created_at: CommitSeq,
    position: u64,
}

/// A private runtime directory pinned to its original authenticated image.
/// Directory capacities and fixed owner metadata have one affine reservation;
/// the source image requires its own independently retained reservation.
/// No coordinate, caller-provided identity map, or decoded property escapes.
pub(crate) struct SealedEdgeLocator {
    source: SealedPartition,
    relation: RelationId,
    versions: Vec<VersionRef>,
    outgoing: Vec<usize>,
    incoming: Vec<usize>,
    // Declared last: every directory allocation is freed before its refund.
    charge: MemoryCharge,
}

impl SealedEdgeLocator {
    /// Requested directory backing stores plus the explicit fixed owner.
    /// `incidences` counts retained content versions, including invisible ones.
    /// Source-image allocations and allocator metadata are not included.
    pub(crate) fn required_bytes(incidences: usize) -> Result<usize, SealedError> {
        incidences
            .checked_mul(size_of::<VersionRef>() + 2 * size_of::<usize>())
            .and_then(|bytes| bytes.checked_add(size_of::<Self>()))
            .ok_or(SealedError::SizeOverflow)
    }

    /// Consume an already admitted image and directory-only reservation.
    /// Every allocation is preceded by admission, and every comparison or
    /// source decode checks QueryCx around the caller's typed work control.
    /// Errors drop the entire private directory; no partial index is returned.
    pub(crate) fn build_with<E>(
        source: SealedPartition,
        relation: RelationId,
        charge: MemoryCharge,
        cx: &QueryCx,
        control: &mut impl FnMut() -> Result<(), E>,
        map_error: fn(SealedError) -> E,
    ) -> Result<Self, E> {
        cx.with_restriction(|| {
            let mut checkpoint = || checked(cx, control, map_error);
            checkpoint()?;
            source.anchor.check_relation(relation).map_err(map_error)?;
            let mut count = 0usize;
            for row in &source.image.rows {
                checkpoint()?;
                if row.key.relation == relation {
                    count = count
                        .checked_add(row.count)
                        .ok_or_else(|| map_error(SealedError::SizeOverflow))?;
                }
            }
            check_limit(
                "EId directory bytes",
                Self::required_bytes(count).map_err(map_error)?,
                charge.bytes(),
            )
            .map_err(map_error)?;
            let mut directory = Self {
                source,
                relation,
                versions: Vec::new(),
                outgoing: Vec::new(),
                incoming: Vec::new(),
                charge,
            };
            checkpoint()?;
            directory
                .versions
                .try_reserve_exact(count)
                .map_err(|_| map_error(SealedError::AllocationFailed))?;
            checkpoint()?;
            directory
                .outgoing
                .try_reserve_exact(count)
                .map_err(|_| map_error(SealedError::AllocationFailed))?;
            checkpoint()?;
            directory
                .incoming
                .try_reserve_exact(count)
                .map_err(|_| map_error(SealedError::AllocationFailed))?;
            // reserve_exact may return a larger logical capacity. Such an
            // allocation cannot escape under a smaller caller reservation.
            directory.check_capacities().map_err(map_error)?;

            let image = &directory.source.image;
            for (row_index, row) in image.rows.iter().enumerate() {
                checkpoint()?;
                if row.key.relation != relation {
                    continue;
                }
                let start = image
                    .offsets
                    .select(row_index)
                    .ok_or_else(|| map_error(SealedError::NonCanonical))?;
                let mut scan = RowScan::new(image, row, 0);
                for at in 0..row.count {
                    checkpoint()?;
                    let (entry, _) = scan
                        .next()
                        .ok_or_else(|| map_error(SealedError::NonCanonical))?;
                    let position = start
                        .checked_add(
                            u64::try_from(at).map_err(|_| map_error(SealedError::SizeOverflow))?,
                        )
                        .ok_or_else(|| map_error(SealedError::SizeOverflow))?;
                    directory.versions.push(VersionRef {
                        eid: entry.eid,
                        created_at: entry.created_at,
                        position,
                    });
                }
            }
            crate::tiered::edge_scan::sort(&mut directory.versions, &mut checkpoint)?;
            for pair in directory.versions.windows(2) {
                checkpoint()?;
                if (pair[0].eid, pair[0].created_at) == (pair[1].eid, pair[1].created_at) {
                    return Err(map_error(SealedError::NonCanonical));
                }
            }
            for at in 0..directory.versions.len() {
                checkpoint()?;
                directory.outgoing.push(at);
                directory.incoming.push(at);
            }
            for (positions, direction) in [
                (&mut directory.outgoing, BufferedEdgeDirection::Outgoing),
                (&mut directory.incoming, BufferedEdgeDirection::Incoming),
            ] {
                sort_by(positions, &mut checkpoint, &mut |left, right| {
                    let left = directory.versions[*left];
                    let right = directory.versions[*right];
                    let left_endpoint = endpoint(image, left, direction).map_err(map_error)?;
                    let right_endpoint = endpoint(image, right, direction).map_err(map_error)?;
                    Ok((left_endpoint, left).cmp(&(right_endpoint, right)))
                })?;
            }
            checkpoint()?;
            Ok(directory)
        })
    }

    fn check_capacities(&self) -> Result<(), SealedError> {
        let versions = self
            .versions
            .capacity()
            .checked_mul(size_of::<VersionRef>())
            .ok_or(SealedError::SizeOverflow)?;
        let positions = self
            .outgoing
            .capacity()
            .checked_add(self.incoming.capacity())
            .and_then(|count| count.checked_mul(size_of::<usize>()))
            .ok_or(SealedError::SizeOverflow)?;
        let bytes = versions
            .checked_add(positions)
            .and_then(|bytes| bytes.checked_add(size_of::<Self>()))
            .ok_or(SealedError::SizeOverflow)?;
        check_limit("EId directory bytes", bytes, self.charge.bytes())
    }

    /// Strictly increasing stable EIds, including all retained invisible IDs.
    /// Candidate admission belongs to the caller, before `edge_with` resolves
    /// history or materializes a returned record.
    pub(crate) fn next_root_with<E>(
        &self,
        cx: &QueryCx,
        as_of: CommitSeq,
        after: Option<EId>,
        control: &mut impl FnMut() -> Result<(), E>,
        map_error: fn(SealedError) -> E,
    ) -> Result<Option<EId>, E> {
        let mut checkpoint = || checked(cx, control, map_error);
        checkpoint()?;
        self.source.anchor.authorize(as_of).map_err(map_error)?;
        let at = upper_bound(self.versions.len(), &mut checkpoint, |at| {
            Ok(after.is_some_and(|after| self.versions[at].eid <= after))
        })?;
        Ok(self.versions.get(at).map(|version| version.eid))
    }

    /// Independently seek a source, target, or their union in stable EId order.
    /// The immutable permutations contain historical incidences as well as
    /// live ones. Strict successor semantics deduplicate versions and loops.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn next_incident_with<E>(
        &self,
        cx: &QueryCx,
        as_of: CommitSeq,
        vertex: VId,
        direction: BufferedEdgeDirection,
        after: Option<EId>,
        control: &mut impl FnMut() -> Result<(), E>,
        map_error: fn(SealedError) -> E,
    ) -> Result<Option<EId>, E> {
        let mut checkpoint = || checked(cx, control, map_error);
        checkpoint()?;
        self.source.anchor.authorize(as_of).map_err(map_error)?;
        let mut seek =
            |direction| self.seek_incident(vertex, direction, after, &mut checkpoint, map_error);
        match direction {
            BufferedEdgeDirection::Outgoing | BufferedEdgeDirection::Incoming => seek(direction),
            BufferedEdgeDirection::Undirected => {
                let outgoing = seek(BufferedEdgeDirection::Outgoing)?;
                let incoming = seek(BufferedEdgeDirection::Incoming)?;
                Ok(match (outgoing, incoming) {
                    (Some(left), Some(right)) => Some(left.min(right)),
                    (left, right) => left.or(right),
                })
            }
        }
    }

    fn seek_incident<E>(
        &self,
        vertex: VId,
        direction: BufferedEdgeDirection,
        after: Option<EId>,
        checkpoint: &mut impl FnMut() -> Result<(), E>,
        map_error: fn(SealedError) -> E,
    ) -> Result<Option<EId>, E> {
        let positions = match direction {
            BufferedEdgeDirection::Outgoing => &self.outgoing,
            BufferedEdgeDirection::Incoming => &self.incoming,
            BufferedEdgeDirection::Undirected => unreachable!("seek one directed permutation"),
        };
        let at = upper_bound(positions.len(), checkpoint, |at| {
            let version = self.versions[positions[at]];
            let found = endpoint(&self.source.image, version, direction).map_err(map_error)?;
            Ok(found < vertex
                || (found == vertex && after.is_some_and(|after| version.eid <= after)))
        })?;
        let Some(&position) = positions.get(at) else {
            return Ok(None);
        };
        checkpoint()?;
        let version = self.versions[position];
        let found = endpoint(&self.source.image, version, direction).map_err(map_error)?;
        Ok((found == vertex).then_some(version.eid))
    }

    /// Resolve one identity at the authorized cut, borrowing the original
    /// version's property row. This does not prove either endpoint is visible.
    pub(crate) fn edge_with<E>(
        &self,
        cx: &QueryCx,
        as_of: CommitSeq,
        eid: EId,
        control: &mut impl FnMut() -> Result<(), E>,
        map_error: fn(SealedError) -> E,
    ) -> Result<Option<SealedEdge<'_>>, E> {
        let mut checkpoint = || checked(cx, control, map_error);
        checkpoint()?;
        self.source.anchor.authorize(as_of).map_err(map_error)?;
        let end = upper_bound(self.versions.len(), &mut checkpoint, |at| {
            let version = self.versions[at];
            Ok((version.eid, version.created_at) <= (eid, as_of))
        })?;
        let Some(version) = end.checked_sub(1).and_then(|at| self.versions.get(at)) else {
            return Ok(None);
        };
        if version.eid != eid {
            return Ok(None);
        }
        checkpoint()?;
        let (entry, locator) = incidence(&self.source.image, *version).map_err(map_error)?;
        if entry.relation != self.relation {
            return Err(map_error(SealedError::NonCanonical));
        }
        if !entry.visible_at(as_of) {
            return Ok(None);
        }
        let properties = if locator == 0 {
            &[][..]
        } else {
            self.source
                .image
                .properties
                .get(locator as usize - 1)
                .ok_or_else(|| map_error(SealedError::NonCanonical))?
                .as_slice()
        };
        Ok(Some(SealedEdge { entry, properties }))
    }
}

fn checked<E>(
    cx: &QueryCx,
    control: &mut impl FnMut() -> Result<(), E>,
    map_error: fn(SealedError) -> E,
) -> Result<(), E> {
    cx.checkpoint()
        .map_err(SealedError::Interrupted)
        .map_err(map_error)?;
    control()?;
    // A successful observer may itself request cancellation. Observe it
    // before this boundary allocates, decodes, or exposes a completed value.
    cx.checkpoint()
        .map_err(SealedError::Interrupted)
        .map_err(map_error)
}

fn incidence(image: &Image, version: VersionRef) -> Result<(AdjacencyEntry, u32), SealedError> {
    let row_index = image
        .offsets
        .rank_le(version.position)
        .checked_sub(1)
        .ok_or(SealedError::NonCanonical)?;
    let start = image
        .offsets
        .select(row_index)
        .ok_or(SealedError::NonCanonical)?;
    let row = image.rows.get(row_index).ok_or(SealedError::NonCanonical)?;
    let local = usize::try_from(version.position - start).map_err(|_| SealedError::SizeOverflow)?;
    let value = row
        .incidence(image, local)
        .ok_or(SealedError::NonCanonical)?;
    if value.0.eid != version.eid || value.0.created_at != version.created_at {
        return Err(SealedError::NonCanonical);
    }
    Ok(value)
}

fn endpoint(
    image: &Image,
    version: VersionRef,
    direction: BufferedEdgeDirection,
) -> Result<VId, SealedError> {
    let (entry, _) = incidence(image, version)?;
    match direction {
        BufferedEdgeDirection::Outgoing => Ok(entry.src),
        BufferedEdgeDirection::Incoming => Ok(entry.dst),
        BufferedEdgeDirection::Undirected => unreachable!("decode one directed endpoint"),
    }
}

fn upper_bound<E>(
    len: usize,
    checkpoint: &mut impl FnMut() -> Result<(), E>,
    mut before: impl FnMut(usize) -> Result<bool, E>,
) -> Result<usize, E> {
    let (mut low, mut high) = (0, len);
    while low < high {
        checkpoint()?;
        let middle = low + (high - low) / 2;
        if before(middle)? {
            low = middle + 1;
        } else {
            high = middle;
        }
    }
    Ok(low)
}

// Constant-scratch comparator variant of the edge merge's controlled heap
// sort. Coordinates stay compact; endpoint keys are decoded only while used.
fn sort_by<T, E>(
    values: &mut [T],
    checkpoint: &mut impl FnMut() -> Result<(), E>,
    compare: &mut impl FnMut(&T, &T) -> Result<Ordering, E>,
) -> Result<(), E> {
    fn sift<T, E>(
        values: &mut [T],
        mut root: usize,
        end: usize,
        checkpoint: &mut impl FnMut() -> Result<(), E>,
        compare: &mut impl FnMut(&T, &T) -> Result<Ordering, E>,
    ) -> Result<(), E> {
        while root < end / 2 {
            let mut child = root * 2 + 1;
            if child + 1 < end {
                checkpoint()?;
                if compare(&values[child], &values[child + 1])? == Ordering::Less {
                    child += 1;
                }
            }
            checkpoint()?;
            if compare(&values[root], &values[child])? != Ordering::Less {
                break;
            }
            values.swap(root, child);
            root = child;
        }
        Ok(())
    }
    let len = values.len();
    for root in (0..len / 2).rev() {
        sift(values, root, len, checkpoint, compare)?;
    }
    for end in (1..len).rev() {
        checkpoint()?;
        values.swap(0, end);
        sift(values, 0, end, checkpoint, compare)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PartitionRootVersion;
    use crate::compact::Compaction;
    use crate::edge_props::BlockProps;
    use crate::tiered::memory::MemoryPool;
    use crate::tiered::sealed::{SealedLimits, SealedScope, image};
    use asupersync::lab::run_async_under_lab;
    use fgdb_delta_types::PropertyKeyId;
    use fgdb_types::{
        BranchId, CanonicalPropertyValue, CanonicalScalar, GraphId, ObjectId, PurposeContexts,
    };

    const R: RelationId = RelationId(5);
    const P: PropertyKeyId = PropertyKeyId(11);

    fn entry(id: u128, src: u128, dst: u128, born: u64, retired: Option<u64>) -> AdjacencyEntry {
        AdjacencyEntry {
            src: VId(src),
            relation: R,
            dst: VId(dst),
            eid: EId(id),
            created_at: CommitSeq(born),
            retired_at: retired.map(CommitSeq),
        }
    }

    fn seal(
        entries: &[(AdjacencyEntry, i64)],
        floor: u64,
        relation: Option<RelationId>,
    ) -> SealedPartition {
        let mut blocks = Vec::new();
        let mut block_props = Vec::new();
        for chunk in entries.chunks(120) {
            blocks.push(chunk.iter().map(|(entry, _)| *entry).collect());
            block_props.push(Some(BlockProps {
                locators: (1..=chunk.len()).map(|at| at as u8).collect(),
                rows: chunk
                    .iter()
                    .map(|(_, value)| {
                        vec![(
                            P,
                            CanonicalPropertyValue::Scalar(CanonicalScalar::Int(*value)),
                        )]
                    })
                    .collect(),
            }));
        }
        let image = image::build(
            Compaction {
                blocks,
                block_props,
                dropped: 0,
                superseded: 0,
            },
            SealedLimits::default(),
            &mut || Ok(()),
        )
        .unwrap();
        SealedPartition::finish_scoped(
            SealedScope {
                source_root: PartitionRootVersion(ObjectId([7; 32])),
                graph: GraphId(1),
                branch: BranchId(2),
                partition: 3,
                floor: CommitSeq(floor),
                publication: CommitSeq(10),
            },
            relation,
            image,
            SealedLimits::default(),
            &mut || Ok(()),
        )
        .unwrap()
    }

    fn fixture() -> SealedPartition {
        seal(
            &[
                (entry(90, 3, 1, 1, None), 901),
                (entry(4, 1, 3, 3, Some(7)), 403),
                (entry(7, 2, 2, 2, None), 702),
                (entry(2, 1, 1, 1, Some(4)), 201),
                (entry(u128::MAX, 2, 1, 8, None), 998),
                (entry(4, 1, 3, 1, Some(3)), 401),
                (entry(0, 1, 2, 1, None), 1),
            ],
            0,
            Some(R),
        )
    }

    fn run(test: impl FnOnce(&QueryCx) + Send + 'static) {
        let ((), report) = run_async_under_lab(0x7c6_311, |root| async move {
            test(&PurposeContexts::narrow_runtime_root(&root).query());
        });
        assert!(report.invariant_violations.is_empty(), "{report:?}");
    }

    fn build(cx: &QueryCx, pool: &MemoryPool, source: SealedPartition) -> SealedEdgeLocator {
        let bytes = SealedEdgeLocator::required_bytes(source.incidence_count()).unwrap();
        let charge = pool.reserve(cx, bytes).unwrap();
        SealedEdgeLocator::build_with(source, R, charge, cx, &mut || Ok(()), |error| error).unwrap()
    }

    fn root_ids(index: &SealedEdgeLocator, cx: &QueryCx, cut: u64) -> Vec<EId> {
        let mut ids = Vec::new();
        let mut after = None;
        while let Some(eid) = index
            .next_root_with(cx, CommitSeq(cut), after, &mut || Ok(()), |error| error)
            .unwrap()
        {
            ids.push(eid);
            after = Some(eid);
        }
        ids
    }

    fn incident_ids(
        index: &SealedEdgeLocator,
        cx: &QueryCx,
        vertex: u128,
        direction: BufferedEdgeDirection,
        cut: u64,
    ) -> Vec<EId> {
        let mut ids = Vec::new();
        let mut after = None;
        while let Some(eid) = index
            .next_incident_with(
                cx,
                CommitSeq(cut),
                VId(vertex),
                direction,
                after,
                &mut || Ok(()),
                |error| error,
            )
            .unwrap()
        {
            ids.push(eid);
            after = Some(eid);
        }
        ids
    }

    #[test]
    fn candidates_keep_retired_and_future_full_width_identities_in_native_order() {
        run(|cx| {
            let source = fixture();
            let original = source.clone();
            let bytes = SealedEdgeLocator::required_bytes(source.incidence_count()).unwrap();
            let pool = MemoryPool::new(bytes, 0).unwrap();
            let index = build(cx, &pool, source);
            assert!(index.source.shares_image_with(&original));
            assert_eq!(pool.used(), bytes);
            for cut in [0, 1, 3, 7, 8, 10] {
                assert_eq!(
                    root_ids(&index, cx, cut),
                    [0, 2, 4, 7, 90, u128::MAX].map(EId)
                );
            }
            assert_eq!(
                index
                    .next_root_with(cx, CommitSeq(3), Some(EId(5)), &mut || Ok(()), |error| {
                        error
                    })
                    .unwrap(),
                Some(EId(7))
            );
            assert_eq!(
                index
                    .next_root_with(
                        cx,
                        CommitSeq(3),
                        Some(EId(u128::MAX)),
                        &mut || Ok(()),
                        |error| error
                    )
                    .unwrap(),
                None
            );
            drop(index);
            assert_eq!(pool.used(), 0);
            assert_eq!(original.incidence_count(), 7);
        });
    }

    #[test]
    fn directional_successors_deduplicate_versions_and_self_loops() {
        run(|cx| {
            let pool = MemoryPool::new(64 * 1024, 0).unwrap();
            let index = build(cx, &pool, fixture());
            for cut in [0, 3, 9] {
                for (vertex, direction, expected) in [
                    (1, BufferedEdgeDirection::Outgoing, vec![0, 2, 4]),
                    (1, BufferedEdgeDirection::Incoming, vec![2, 90, u128::MAX]),
                    (
                        1,
                        BufferedEdgeDirection::Undirected,
                        vec![0, 2, 4, 90, u128::MAX],
                    ),
                    (2, BufferedEdgeDirection::Outgoing, vec![7, u128::MAX]),
                    (2, BufferedEdgeDirection::Incoming, vec![0, 7]),
                    (2, BufferedEdgeDirection::Undirected, vec![0, 7, u128::MAX]),
                    (99, BufferedEdgeDirection::Undirected, Vec::new()),
                ] {
                    assert_eq!(
                        incident_ids(&index, cx, vertex, direction, cut),
                        expected.into_iter().map(EId).collect::<Vec<_>>()
                    );
                }
            }
        });
    }

    #[test]
    fn exact_cut_lookup_borrows_the_winning_content_versions_property_row() {
        run(|cx| {
            let pool = MemoryPool::new(64 * 1024, 0).unwrap();
            let index = build(cx, &pool, fixture());
            for (id, cut, expected_birth, expected_value) in [
                (4, 1, 1, 401),
                (4, 2, 1, 401),
                (4, 3, 3, 403),
                (4, 6, 3, 403),
                (u128::MAX, 8, 8, 998),
                (2, 3, 1, 201),
                (7, 10, 2, 702),
            ] {
                let edge = index
                    .edge_with(cx, CommitSeq(cut), EId(id), &mut || Ok(()), |error| error)
                    .unwrap()
                    .unwrap();
                assert_eq!(edge.entry.eid, EId(id));
                assert_eq!(edge.entry.created_at, CommitSeq(expected_birth));
                assert_eq!(
                    edge.properties,
                    [(
                        P,
                        CanonicalPropertyValue::Scalar(CanonicalScalar::Int(expected_value))
                    )]
                );
                assert!(
                    index
                        .source
                        .image
                        .properties
                        .iter()
                        .any(|row| { std::ptr::eq(row.as_slice(), edge.properties) })
                );
            }
            for (id, cut) in [(0, 0), (4, 7), (2, 4), (u128::MAX, 7), (8, 10)] {
                assert!(
                    index
                        .edge_with(cx, CommitSeq(cut), EId(id), &mut || Ok(()), |error| error)
                        .unwrap()
                        .is_none()
                );
            }
        });
    }

    #[test]
    fn csr_positions_and_relation_scope_remain_source_bound() {
        run(|cx| {
            let mut entries = (0..24)
                .map(|at| (entry(300 - at, 4, 100 + at, 1, None), at as i64))
                .collect::<Vec<_>>();
            let mut other = entry(1, 4, 99, 1, None);
            other.relation = RelationId(9);
            entries.push((other, 99));
            let source = seal(&entries, 0, None);
            assert_eq!(source.stats().sealed_rows, 1);
            let pool = MemoryPool::new(64 * 1024, 0).unwrap();
            let index = build(cx, &pool, source);
            let expected = (277..=300).map(EId).collect::<Vec<_>>();
            assert_eq!(root_ids(&index, cx, 5), expected);
            assert_eq!(
                incident_ids(&index, cx, 4, BufferedEdgeDirection::Outgoing, 5),
                expected
            );
            assert_eq!(
                incident_ids(&index, cx, 105, BufferedEdgeDirection::Incoming, 5),
                [EId(295)]
            );
            let edge = index
                .edge_with(cx, CommitSeq(5), EId(295), &mut || Ok(()), |error| error)
                .unwrap()
                .unwrap();
            assert_eq!(edge.entry.dst, VId(105));
            assert_eq!(
                edge.properties,
                [(P, CanonicalPropertyValue::Scalar(CanonicalScalar::Int(5)))]
            );
            assert!(
                index
                    .edge_with(cx, CommitSeq(5), EId(1), &mut || Ok(()), |error| error)
                    .unwrap()
                    .is_none()
            );
        });
    }

    #[test]
    fn empty_images_and_anchor_refusals_do_not_claim_unadmitted_cuts_or_relations() {
        run(|cx| {
            let pool = MemoryPool::new(64 * 1024, 0).unwrap();
            let index = build(cx, &pool, seal(&[], 2, Some(R)));
            assert!(root_ids(&index, cx, 2).is_empty());
            assert!(incident_ids(&index, cx, 1, BufferedEdgeDirection::Undirected, 10).is_empty());
            for cut in [1, 11] {
                assert!(matches!(
                    index.next_root_with(cx, CommitSeq(cut), None, &mut || Ok(()), |error| error),
                    Err(SealedError::SnapshotOutsideAnchor { .. })
                ));
                assert!(matches!(
                    index.edge_with(cx, CommitSeq(cut), EId(1), &mut || Ok(()), |error| error),
                    Err(SealedError::SnapshotOutsideAnchor { .. })
                ));
                assert!(matches!(
                    index.next_incident_with(
                        cx,
                        CommitSeq(cut),
                        VId(1),
                        BufferedEdgeDirection::Outgoing,
                        None,
                        &mut || Ok(()),
                        |error| error
                    ),
                    Err(SealedError::SnapshotOutsideAnchor { .. })
                ));
            }
            drop(index);
            let source = fixture();
            let charge = pool
                .reserve(
                    cx,
                    SealedEdgeLocator::required_bytes(source.incidence_count()).unwrap(),
                )
                .unwrap();
            assert!(matches!(
                SealedEdgeLocator::build_with(
                    source,
                    RelationId(9),
                    charge,
                    cx,
                    &mut || Ok(()),
                    |error| error
                ),
                Err(SealedError::RelationOutsideAnchor { .. })
            ));
            assert_eq!(pool.used(), 0);
        });
    }

    #[derive(Debug)]
    enum ControlledError {
        Source(SealedError),
        Stopped,
    }

    #[test]
    fn refused_directory_or_typed_work_refunds_every_partial_allocation() {
        run(|cx| {
            let source = fixture();
            let bytes = SealedEdgeLocator::required_bytes(source.incidence_count()).unwrap();
            let pool = MemoryPool::new(bytes, 0).unwrap();
            let charge = pool.reserve(cx, bytes - 1).unwrap();
            assert!(matches!(
                SealedEdgeLocator::build_with(
                    source.clone(),
                    R,
                    charge,
                    cx,
                    &mut || Ok(()),
                    |error| error
                ),
                Err(SealedError::Limit {
                    resource: "EId directory bytes",
                    ..
                })
            ));
            assert_eq!(pool.used(), 0);
            let mut steps = 0;
            let charge = pool.reserve(cx, bytes).unwrap();
            let index = SealedEdgeLocator::build_with(
                source.clone(),
                R,
                charge,
                cx,
                &mut || {
                    steps += 1;
                    Ok(())
                },
                ControlledError::Source,
            )
            .unwrap();
            drop(index);
            assert_eq!(pool.used(), 0);
            for stop in [0, 1, 3, 11, 33, steps / 2, steps - 1] {
                let charge = pool.reserve(cx, bytes).unwrap();
                let mut checks = 0;
                let result = SealedEdgeLocator::build_with(
                    source.clone(),
                    R,
                    charge,
                    cx,
                    &mut || {
                        if checks == stop {
                            return Err(ControlledError::Stopped);
                        }
                        checks += 1;
                        Ok(())
                    },
                    ControlledError::Source,
                );
                assert!(matches!(result, Err(ControlledError::Stopped)));
                assert_eq!(pool.used(), 0, "directory leaked at checkpoint {stop}");
            }
            let index = build(cx, &pool, source);
            assert!(matches!(
                index.edge_with(
                    cx,
                    CommitSeq(3),
                    EId(4),
                    &mut || Err(ControlledError::Stopped),
                    ControlledError::Source
                ),
                Err(ControlledError::Stopped)
            ));
            assert_eq!(
                root_ids(&index, cx, 3),
                [0, 2, 4, 7, 90, u128::MAX].map(EId)
            );
            drop(index);
            assert_eq!(pool.used(), 0);
        });
    }

    #[test]
    fn query_cancellation_precedes_external_control_and_releases_the_build() {
        for stop_at_last in [false, true] {
            let ((), report) = run_async_under_lab(0x7c6_312, move |root| async move {
                let cx = PurposeContexts::narrow_runtime_root(&root).query();
                let source = fixture();
                let bytes = SealedEdgeLocator::required_bytes(source.incidence_count()).unwrap();
                let pool = MemoryPool::new(bytes, 0).unwrap();
                let charge = pool.reserve(&cx, bytes).unwrap();
                let mut steps = 0;
                let index = SealedEdgeLocator::build_with(
                    source.clone(),
                    R,
                    charge,
                    &cx,
                    &mut || {
                        steps += 1;
                        Ok(())
                    },
                    ControlledError::Source,
                )
                .unwrap();
                drop(index);
                let stop = if stop_at_last { steps } else { 12 };
                let charge = pool.reserve(&cx, bytes).unwrap();
                let mut checks = 0;
                let result = SealedEdgeLocator::build_with(
                    source,
                    R,
                    charge,
                    &cx,
                    &mut || {
                        checks += 1;
                        if checks == stop {
                            root.set_cancel_reason(asupersync::CancelReason::new(
                                asupersync::CancelKind::User,
                            ));
                        }
                        Ok(())
                    },
                    ControlledError::Source,
                );
                assert!(matches!(
                    result,
                    Err(ControlledError::Source(SealedError::Interrupted(_)))
                ));
                assert_eq!(
                    checks, stop,
                    "caller control ran after QueryCx cancellation"
                );
                assert_eq!(pool.used(), 0);
            });
            assert!(report.invariant_violations.is_empty(), "{report:?}");
        }
    }
}
