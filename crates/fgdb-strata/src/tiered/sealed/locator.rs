//! Query-local EId access derived only from admitted immutable history.
//!
//! The directory retains content-version coordinates, not another adjacency or
//! property image. Root and incidence candidates include invisible history;
//! resolving a candidate is a separate exact-cut operation. A consumer needing
//! the complete publication's candidate domain admits a floor-zero image and
//! either its verified publication or the complete authenticated post-seal
//! suffix. A materialized visible delta overlay does not prove that domain.

use super::{Image, RowScan, SealedEdge, SealedError, SealedPartition, check_limit};
use crate::AdjacencyEntry;
use crate::store::NativeRunSuffix;
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
    suffix: Option<NativeRunSuffix>,
    relation: RelationId,
    versions: Vec<VersionRef>,
    outgoing: Vec<usize>,
    incoming: Vec<usize>,
    // Declared last: every directory allocation is freed before its refund.
    charge: MemoryCharge,
}

impl SealedEdgeLocator {
    /// Requested directory backing stores plus the explicit fixed owner.
    /// `incidences` counts every base version and raw suffix statement before
    /// restatement collapse, including invisible ones. Both source owners'
    /// allocations and allocator metadata are separately admitted.
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
        Self::build(source, relation, None, charge, cx, control, map_error)
    }

    /// Merge only coordinates from a complete, store-admitted suffix. The
    /// receipt owns its decoded blocks and their reservations and binds them
    /// to this exact base image and pinned envelope. It does not alter the
    /// base run's floor, horizon, original provenance, or property storage.
    pub(crate) fn build_with_suffix<E>(
        source: SealedPartition,
        relation: RelationId,
        suffix: NativeRunSuffix,
        charge: MemoryCharge,
        cx: &QueryCx,
        control: &mut impl FnMut() -> Result<(), E>,
        map_error: fn(SealedError) -> E,
    ) -> Result<Self, E> {
        Self::build(
            source,
            relation,
            Some(suffix),
            charge,
            cx,
            control,
            map_error,
        )
    }

    fn build<E>(
        source: SealedPartition,
        relation: RelationId,
        suffix: Option<NativeRunSuffix>,
        charge: MemoryCharge,
        cx: &QueryCx,
        control: &mut impl FnMut() -> Result<(), E>,
        map_error: fn(SealedError) -> E,
    ) -> Result<Self, E> {
        cx.with_restriction(|| {
            let mut checkpoint = || checked(cx, control, map_error);
            checkpoint()?;
            source.anchor.check_relation(relation).map_err(map_error)?;
            if let Some(suffix) = &suffix {
                suffix
                    .authorize(source.anchor, relation, source.anchor.scope.floor)
                    .map_err(map_error)?;
            }
            let mut count = 0usize;
            for row in &source.image.rows {
                checkpoint()?;
                if row.key.relation == relation {
                    count = count
                        .checked_add(row.count)
                        .ok_or_else(|| map_error(SealedError::SizeOverflow))?;
                }
            }
            if let Some(suffix) = &suffix {
                if suffix.blocks().len() != suffix.properties().len() {
                    return Err(map_error(SealedError::NonCanonical));
                }
                let mut raw = 0usize;
                for block in suffix.blocks() {
                    checkpoint()?;
                    if block.len() > crate::MAX_BLOCK_ENTRIES as usize {
                        return Err(map_error(SealedError::NonCanonical));
                    }
                    raw = raw
                        .checked_add(block.len())
                        .ok_or_else(|| map_error(SealedError::SizeOverflow))?;
                }
                if raw != suffix.incidence_count() {
                    return Err(map_error(SealedError::NonCanonical));
                }
                count = count
                    .checked_add(raw)
                    .ok_or_else(|| map_error(SealedError::SizeOverflow))?;
            }
            check_limit(
                "EId directory bytes",
                Self::required_bytes(count).map_err(map_error)?,
                charge.bytes(),
            )
            .map_err(map_error)?;
            let mut directory = Self {
                source,
                suffix,
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
            let suffix = directory.suffix.as_ref();
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
            if let Some(suffix) = suffix {
                // Receipt slots preserve ascending root publication ordinal.
                // The bounded row slot keeps every original property locator
                // intact while placing all suffix coordinates after the base.
                for (block_at, block) in suffix.blocks().iter().enumerate() {
                    checkpoint()?;
                    for (row_at, entry) in block.iter().enumerate() {
                        checkpoint()?;
                        if entry.relation != relation {
                            return Err(map_error(SealedError::NonCanonical));
                        }
                        let position = suffix_position(image.incidences, block_at, row_at)
                            .map_err(map_error)?;
                        directory.versions.push(VersionRef {
                            eid: entry.eid,
                            created_at: entry.created_at,
                            position,
                        });
                    }
                }
            }
            crate::tiered::edge_scan::sort(&mut directory.versions, &mut checkpoint)?;
            collapse_restatements(
                &mut directory.versions,
                image.incidences,
                &mut checkpoint,
                map_error,
            )?;
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
                    let left_endpoint =
                        endpoint(image, suffix, left, direction).map_err(map_error)?;
                    let right_endpoint =
                        endpoint(image, suffix, right, direction).map_err(map_error)?;
                    Ok((left_endpoint, left).cmp(&(right_endpoint, right)))
                })?;
            }
            checkpoint()?;
            Ok(directory)
        })
    }

    fn authorize(&self, as_of: CommitSeq) -> Result<(), SealedError> {
        match &self.suffix {
            Some(suffix) => suffix.authorize(self.source.anchor, self.relation, as_of),
            None => self.source.anchor.authorize(as_of),
        }
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
        self.authorize(as_of).map_err(map_error)?;
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
        self.authorize(as_of).map_err(map_error)?;
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
            let found = endpoint(&self.source.image, self.suffix.as_ref(), version, direction)
                .map_err(map_error)?;
            Ok(found < vertex
                || (found == vertex && after.is_some_and(|after| version.eid <= after)))
        })?;
        let Some(&position) = positions.get(at) else {
            return Ok(None);
        };
        checkpoint()?;
        let version = self.versions[position];
        let found = endpoint(&self.source.image, self.suffix.as_ref(), version, direction)
            .map_err(map_error)?;
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
        self.authorize(as_of).map_err(map_error)?;
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
        let (entry, locator) =
            incidence(&self.source.image, self.suffix.as_ref(), *version).map_err(map_error)?;
        if entry.relation != self.relation {
            return Err(map_error(SealedError::NonCanonical));
        }
        if !entry.visible_at(as_of) {
            return Ok(None);
        }
        let properties = if locator == 0 {
            &[][..]
        } else if let Some((block, _)) =
            suffix_location(self.source.image.incidences, version.position).map_err(map_error)?
        {
            self.suffix
                .as_ref()
                .and_then(|suffix| suffix.properties().get(block))
                .and_then(Option::as_ref)
                .and_then(|properties| properties.rows.get(locator as usize - 1))
                .ok_or_else(|| map_error(SealedError::NonCanonical))?
                .as_slice()
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

// The receipt's block slots are publication ordered; each block has at most
// MAX_BLOCK_ENTRIES rows. Gaps between short blocks are intentional. They
// avoid another resident offset table and cannot be mistaken for stable IDs.
fn suffix_position(base: usize, block: usize, row: usize) -> Result<u64, SealedError> {
    if row >= crate::MAX_BLOCK_ENTRIES as usize {
        return Err(SealedError::NonCanonical);
    }
    let base = u64::try_from(base).map_err(|_| SealedError::SizeOverflow)?;
    let block = u64::try_from(block).map_err(|_| SealedError::SizeOverflow)?;
    let row = u64::try_from(row).map_err(|_| SealedError::SizeOverflow)?;
    block
        .checked_mul(u64::from(crate::MAX_BLOCK_ENTRIES))
        .and_then(|offset| offset.checked_add(row))
        .and_then(|offset| base.checked_add(offset))
        .ok_or(SealedError::SizeOverflow)
}

fn suffix_location(base: usize, position: u64) -> Result<Option<(usize, usize)>, SealedError> {
    let base = u64::try_from(base).map_err(|_| SealedError::SizeOverflow)?;
    let Some(offset) = position.checked_sub(base) else {
        return Ok(None);
    };
    let width = u64::from(crate::MAX_BLOCK_ENTRIES);
    Ok(Some((
        usize::try_from(offset / width).map_err(|_| SealedError::SizeOverflow)?,
        usize::try_from(offset % width).map_err(|_| SealedError::SizeOverflow)?,
    )))
}

// Sorted (eid, birth, position) puts base before suffix and earlier published
// blocks before later ones. Collapse only exact content-version restatements;
// every other birth, including an invisible or retired identity, must remain.
fn collapse_restatements<E>(
    versions: &mut Vec<VersionRef>,
    base: usize,
    checkpoint: &mut impl FnMut() -> Result<(), E>,
    map_error: fn(SealedError) -> E,
) -> Result<(), E> {
    let base = u64::try_from(base).map_err(|_| map_error(SealedError::SizeOverflow))?;
    let mut keep = 0;
    for at in 0..versions.len() {
        checkpoint()?;
        let version = versions[at];
        if keep != 0
            && (versions[keep - 1].eid, versions[keep - 1].created_at)
                == (version.eid, version.created_at)
        {
            // A sealed image is already canonical. Only receipt-owned suffix
            // statements may replace a coordinate for the same version.
            if version.position < base {
                return Err(map_error(SealedError::NonCanonical));
            }
            versions[keep - 1] = version;
        } else {
            versions[keep] = version;
            keep += 1;
        }
    }
    versions.truncate(keep);
    Ok(())
}

fn incidence(
    image: &Image,
    suffix: Option<&NativeRunSuffix>,
    version: VersionRef,
) -> Result<(AdjacencyEntry, u32), SealedError> {
    let value = if let Some((block, row)) = suffix_location(image.incidences, version.position)? {
        let suffix = suffix.ok_or(SealedError::NonCanonical)?;
        let entry = suffix
            .blocks()
            .get(block)
            .and_then(|entries| entries.get(row))
            .copied()
            .ok_or(SealedError::NonCanonical)?;
        let properties = suffix
            .properties()
            .get(block)
            .ok_or(SealedError::NonCanonical)?;
        let locator = match properties {
            Some(properties) => u32::from(
                *properties
                    .locators
                    .get(row)
                    .ok_or(SealedError::NonCanonical)?,
            ),
            None => 0,
        };
        (entry, locator)
    } else {
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
        let local =
            usize::try_from(version.position - start).map_err(|_| SealedError::SizeOverflow)?;
        row.incidence(image, local)
            .ok_or(SealedError::NonCanonical)?
    };
    if value.0.eid != version.eid || value.0.created_at != version.created_at {
        return Err(SealedError::NonCanonical);
    }
    Ok(value)
}

fn endpoint(
    image: &Image,
    suffix: Option<&NativeRunSuffix>,
    version: VersionRef,
    direction: BufferedEdgeDirection,
) -> Result<VId, SealedError> {
    let (entry, _) = incidence(image, suffix, version)?;
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

    #[test]
    fn suffix_coordinates_keep_source_boundaries_and_original_row_slots() {
        let width = crate::MAX_BLOCK_ENTRIES as usize;
        for base in [0, 1, 17, 4096] {
            if base != 0 {
                assert_eq!(suffix_location(base, base as u64 - 1).unwrap(), None);
            }
            for block in [0, 1, 7] {
                for row in [0, 1, width - 1] {
                    let position = suffix_position(base, block, row).unwrap();
                    assert_eq!(position, (base + block * width + row) as u64);
                    assert_eq!(suffix_location(base, position).unwrap(), Some((block, row)));
                }
            }
        }
        assert!(matches!(
            suffix_position(17, 1, width),
            Err(SealedError::NonCanonical)
        ));
        if usize::BITS == u64::BITS {
            assert!(matches!(
                suffix_position(usize::MAX, 1, 0),
                Err(SealedError::SizeOverflow)
            ));
        }
    }

    #[test]
    fn suffix_restatement_collapse_keeps_every_birth_and_last_publication() {
        let version = |eid, birth, position| VersionRef {
            eid: EId(eid),
            created_at: CommitSeq(birth),
            position,
        };
        let base = 8;
        let earlier = suffix_position(base, 0, 7).unwrap();
        let later = suffix_position(base, 2, 0).unwrap();
        let future = suffix_position(base, 3, 1).unwrap();
        // Neighbor order need not follow EId order. The sparse later block
        // wins an exact birth even though its original row number is smaller.
        let mut versions = vec![
            version(90, 1, 6),
            version(4, 3, later),
            version(u128::MAX, 2, 7),
            version(0, 11, future),
            version(4, 1, 2),
            version(4, 3, earlier),
            version(4, 3, 3),
            version(4, 12, future + 1),
        ];
        crate::tiered::edge_scan::sort(&mut versions, &mut || Ok::<_, SealedError>(())).unwrap();
        let mut checkpoints = 0;
        collapse_restatements(
            &mut versions,
            base,
            &mut || {
                checkpoints += 1;
                Ok(())
            },
            |error| error,
        )
        .unwrap();
        assert_eq!(checkpoints, 8);
        assert_eq!(
            versions
                .iter()
                .map(|version| (version.eid.0, version.created_at.0, version.position))
                .collect::<Vec<_>>(),
            [
                (0, 11, future),
                (4, 1, 2),
                (4, 3, later),
                (4, 12, future + 1),
                (90, 1, 6),
                (u128::MAX, 2, 7),
            ]
        );

        let mut duplicate_base = vec![version(4, 1, 2), version(4, 1, 3)];
        assert!(matches!(
            collapse_restatements(&mut duplicate_base, base, &mut || Ok(()), |error| error),
            Err(SealedError::NonCanonical)
        ));
        let mut interrupted = vec![version(4, 1, 2), version(4, 1, earlier)];
        assert!(matches!(
            collapse_restatements(
                &mut interrupted,
                base,
                &mut || Err(ControlledError::Stopped),
                ControlledError::Source,
            ),
            Err(ControlledError::Stopped)
        ));
    }

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
