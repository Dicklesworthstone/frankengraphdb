//! Resident shape of a sealed image. Descriptor records stay small: inline
//! payloads and CSR payloads have separate dense arenas, so the largest inline
//! enum variant does not inflate EVERY sealed-row descriptor.

use super::super::inline::{INLINE_CAPACITY, InlineAdjacency, InlineIncidence};
use super::{RowStorageKind, SealedError, SealedLimits, SealedScope, check_limit};
use crate::compact::Compaction;
use crate::edge_props::{EdgePropertyRow, admitted_row_bytes, validate_locator_sequence};
use crate::{AdjacencyEntry, DescriptorKey, Direction, VisibilityInterval};
use fgdb_codec::elias_fano::{EliasFano, EliasFanoSequentialCursor, EntryLimit};
use fgdb_codec::identity::{IdentityColumn, IdentityColumnLimits};
use fgdb_delta_types::RelationId;
use fgdb_types::{CommitSeq, EId, VId};

pub(super) fn reserved<T>(count: usize) -> Result<Vec<T>, SealedError> {
    let mut values = Vec::new();
    values
        .try_reserve_exact(count)
        .map_err(|_| SealedError::AllocationFailed)?;
    Ok(values)
}

pub(super) fn identity_limits(count: usize, limits: SealedLimits) -> IdentityColumnLimits {
    IdentityColumnLimits::new(count, count, limits.max_image_bytes)
}

pub(super) struct Image {
    pub dictionary: IdentityColumn<VId>,
    pub offsets: EliasFano,
    pub rows: Vec<Row>,
    pub inlines: Vec<InlineAdjacency>,
    pub runs: Vec<Run>,
    pub properties: Vec<EdgePropertyRow>,
    pub incidences: usize,
}

pub(super) struct Row {
    pub key: DescriptorKey,
    pub count: usize,
    pub storage: Storage,
}

#[derive(Clone, Copy)]
pub(super) enum Storage {
    Inline(usize),
    Run(usize),
}

pub(super) struct Run {
    pub neighbors: EliasFano,
    pub edge_ids: IdentityColumn<EId>,
    pub spans: Vec<VisibilityInterval>,
    pub locators: Vec<u32>,
}

/// Forward state for a retained row. CSR neighbors and visibility spans advance
/// together, avoiding a rank-directory search and span binary search per edge.
/// Positional seeks happen when opening a row or resuming after a long invisible
/// span. No decoded adjacency or property copy is retained.
pub(super) struct RowScan<'a> {
    image: &'a Image,
    row: &'a Row,
    pub position: usize,
    run: Option<RunScan<'a>>,
    #[cfg(test)]
    decoded: usize,
    #[cfg(test)]
    resumed_seeks: usize,
}

struct RunScan<'a> {
    run: &'a Run,
    // Long skipped history invalidates the old decode position. Reopen only
    // when a visible incidence actually needs its neighbor and identity.
    neighbors: Option<EliasFanoSequentialCursor<'a>>,
    span: usize,
}

pub(super) enum RowScanStep {
    Incidence(AdjacencyEntry, u32),
    /// The next incidences are hidden; none has advanced yet. The caller must
    /// admit their raw work before calling `skip_hidden` with a bounded prefix.
    Hidden(usize),
}

impl<'a> RowScan<'a> {
    pub fn new(image: &'a Image, row: &'a Row, position: usize) -> Self {
        let run = match row.storage {
            Storage::Inline(_) => None,
            Storage::Run(index) => {
                let run = &image.runs[index];
                Some(RunScan {
                    run,
                    neighbors: Some(run.neighbors.sequential_cursor_from(position)),
                    span: run
                        .spans
                        .partition_point(|span| span.end_row as usize <= position),
                })
            }
        };
        Self {
            image,
            row,
            position,
            run,
            #[cfg(test)]
            decoded: 0,
            #[cfg(test)]
            resumed_seeks: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.row.len()
    }

    #[inline]
    pub fn next(&mut self) -> Option<(AdjacencyEntry, u32)> {
        match self.next_inner::<false>(CommitSeq(0))? {
            RowScanStep::Incidence(entry, locator) => Some((entry, locator)),
            RowScanStep::Hidden(_) => unreachable!("an unfiltered scan never skips history"),
        }
    }

    /// Decode one incidence, or report an invisible CSR span without reading
    /// its neighbor, identity or locator columns. Inline rows remain bounded
    /// scalar decodes. This does not replace full physical image admission.
    #[inline]
    pub fn next_or_hidden_span(&mut self, as_of: CommitSeq) -> Option<RowScanStep> {
        self.next_inner::<true>(as_of)
    }

    /// Advance an admitted prefix of the hidden span just reported. Keep
    /// short version gaps sequential instead of seeking before every live
    /// edge; long spans need no further neighbor decode until visible work.
    pub fn skip_hidden(&mut self, count: usize) {
        let scan = self.run.as_mut().expect("only CSR spans are skipped");
        let remaining = scan.run.spans[scan.span].end_row as usize - self.position;
        debug_assert!(count > 0 && count <= remaining);
        if remaining <= 8 {
            if let Some(neighbors) = &mut scan.neighbors {
                for _ in 0..count {
                    neighbors.next().expect("admitted hidden neighbor");
                }
            }
        } else {
            scan.neighbors = None;
        }
        self.position += count;
    }

    #[cfg(test)]
    pub fn decoded_incidences(&self) -> usize {
        self.decoded
    }

    #[cfg(test)]
    pub fn resumed_seeks(&self) -> usize {
        self.resumed_seeks
    }

    #[inline]
    fn next_inner<const FILTER: bool>(&mut self, as_of: CommitSeq) -> Option<RowScanStep> {
        if self.position >= self.row.len() {
            return None;
        }
        let value = if let Some(scan) = &mut self.run {
            while scan.run.spans.get(scan.span)?.end_row as usize <= self.position {
                scan.span += 1;
            }
            let span = scan.run.spans.get(scan.span)?;
            if self.position < span.start_row as usize {
                return None;
            }
            if FILTER
                && (as_of < span.created_at || span.retired_at.is_some_and(|end| as_of >= end))
            {
                return Some(RowScanStep::Hidden(span.end_row as usize - self.position));
            }
            #[cfg(test)]
            if scan.neighbors.is_none() {
                self.resumed_seeks += 1;
            }
            let position = self.position;
            let run = scan.run;
            let neighbors = scan
                .neighbors
                .get_or_insert_with(|| run.neighbors.sequential_cursor_from(position));
            let ordinal = usize::try_from(neighbors.next()?).ok()?;
            (
                AdjacencyEntry {
                    src: self.row.key.src,
                    relation: self.row.key.relation,
                    dst: self.image.dictionary.get(ordinal)?,
                    eid: scan.run.edge_ids.get(self.position)?,
                    created_at: span.created_at,
                    retired_at: span.retired_at,
                },
                *scan.run.locators.get(self.position)?,
            )
        } else {
            self.row.incidence(self.image, self.position)?
        };
        self.position += 1;
        #[cfg(test)]
        {
            self.decoded += 1;
        }
        Some(RowScanStep::Incidence(value.0, value.1))
    }
}

impl Row {
    pub fn len(&self) -> usize {
        self.count
    }

    pub fn kind(&self) -> RowStorageKind {
        match self.storage {
            Storage::Inline(_) => RowStorageKind::Inline,
            Storage::Run(_) => RowStorageKind::SealedCsr,
        }
    }

    pub fn incidence(&self, image: &Image, at: usize) -> Option<(AdjacencyEntry, u32)> {
        if at >= self.count {
            return None;
        }
        match self.storage {
            Storage::Inline(index) => {
                let slot = image.inlines.get(index)?.slots().nth(at)?;
                Some((slot.entry(self.key), slot.property_locator))
            }
            Storage::Run(index) => {
                let run = image.runs.get(index)?;
                let ordinal = usize::try_from(run.neighbors.select(at)?).ok()?;
                let span = run.spans.get(
                    run.spans
                        .partition_point(|span| span.end_row as usize <= at),
                )?;
                if at < span.start_row as usize {
                    return None;
                }
                Some((
                    AdjacencyEntry {
                        src: self.key.src,
                        relation: self.key.relation,
                        dst: image.dictionary.get(ordinal)?,
                        eid: run.edge_ids.get(at)?,
                        created_at: span.created_at,
                        retired_at: span.retired_at,
                    },
                    *run.locators.get(at)?,
                ))
            }
        }
    }

    pub fn lower_bound(&self, image: &Image, target: VId) -> usize {
        match self.storage {
            Storage::Inline(index) => image.inlines[index]
                .slots()
                .position(|slot| slot.dst >= target)
                .unwrap_or(self.count),
            Storage::Run(index) => image.runs[index]
                .neighbors
                .rank_lt(image.destination_rank(target) as u64),
        }
    }
}

impl Image {
    /// Measure the completed immutable owner once. Property collections are
    /// traversed under live control; no wire encoding or decoded clone is made.
    pub fn retained_bytes(
        &self,
        checkpoint: &mut impl FnMut() -> Result<(), SealedError>,
    ) -> Result<usize, SealedError> {
        fn bytes<T>(values: &Vec<T>) -> Result<usize, SealedError> {
            values
                .capacity()
                .checked_mul(size_of::<T>())
                .ok_or(SealedError::SizeOverflow)
        }
        fn add(left: usize, right: usize) -> Result<usize, SealedError> {
            left.checked_add(right).ok_or(SealedError::SizeOverflow)
        }
        checkpoint()?;
        let mut retained = size_of::<Self>();
        for bytes in [
            self.dictionary
                .owned_heap_bytes()
                .ok_or(SealedError::SizeOverflow)?,
            self.offsets
                .owned_heap_bytes()
                .ok_or(SealedError::SizeOverflow)?,
            bytes(&self.rows)?,
            bytes(&self.inlines)?,
            bytes(&self.runs)?,
            bytes(&self.properties)?,
        ] {
            retained = add(retained, bytes)?;
        }
        for run in &self.runs {
            checkpoint()?;
            retained = add(
                retained,
                run.neighbors
                    .owned_heap_bytes()
                    .ok_or(SealedError::SizeOverflow)?,
            )?;
            retained = add(
                retained,
                run.edge_ids
                    .owned_heap_bytes()
                    .ok_or(SealedError::SizeOverflow)?,
            )?;
            retained = add(retained, bytes(&run.spans)?)?;
            retained = add(retained, bytes(&run.locators)?)?;
        }
        for row in &self.properties {
            checkpoint()?;
            retained = add(retained, bytes(row)?)?;
            for (_, value) in row {
                retained = add(
                    retained,
                    value
                        .owned_heap_bytes_controlled(checkpoint)?
                        .ok_or(SealedError::SizeOverflow)?,
                )?;
            }
        }
        checkpoint()?;
        Ok(retained)
    }

    pub fn find_row(&self, src: VId, relation: RelationId) -> Option<&Row> {
        let key = DescriptorKey {
            src,
            relation,
            direction: Direction::Outbound,
        };
        self.rows
            .binary_search_by_key(&key, |row| row.key)
            .ok()
            .map(|at| &self.rows[at])
    }

    fn destination_rank(&self, target: VId) -> usize {
        let mut low = 0;
        let mut high = self.dictionary.len();
        while low < high {
            let mid = low + (high - low) / 2;
            if self
                .dictionary
                .get(mid)
                .expect("admitted dictionary position")
                < target
            {
                low = mid + 1;
            } else {
                high = mid;
            }
        }
        low
    }

    pub fn validate(
        &self,
        scope: SealedScope,
        limits: SealedLimits,
        checkpoint: &mut impl FnMut() -> Result<(), SealedError>,
    ) -> Result<(), SealedError> {
        check_limit("descriptors", self.rows.len(), limits.max_rows)?;
        check_limit("incidences", self.incidences, limits.max_incidences)?;
        if scope.floor > scope.publication
            || self.rows.len() > self.incidences
            || self.dictionary.len() > self.incidences
            || self.properties.len() > self.incidences
        {
            return Err(SealedError::NonCanonical);
        }
        let mut previous = None;
        for at in 0..self.dictionary.len() {
            if at % 256 == 0 {
                checkpoint()?;
            }
            let destination = self.dictionary.get(at).ok_or(SealedError::NonCanonical)?;
            if previous.is_some_and(|previous| previous >= destination) {
                return Err(SealedError::NonCanonical);
            }
            previous = Some(destination);
        }
        if self.offsets.len() != self.rows.len() + 1
            || self.offsets.select(0) != Some(0)
            || self.offsets.select(self.rows.len()) != Some(self.incidences as u64)
        {
            return Err(SealedError::NonCanonical);
        }
        let mut used = reserved(self.dictionary.len())?;
        used.resize(self.dictionary.len(), false);
        let mut next_inline = 0;
        let mut next_run = 0;
        let mut position = 0usize;
        let mut property_locator = 0usize;
        let mut previous_key = None;
        for (row_index, row) in self.rows.iter().enumerate() {
            checkpoint()?;
            if row.count == 0 || previous_key.is_some_and(|key| key >= row.key) {
                return Err(SealedError::NonCanonical);
            }
            previous_key = Some(row.key);
            if self.offsets.select(row_index) != Some(position as u64) {
                return Err(SealedError::NonCanonical);
            }
            position = position
                .checked_add(row.count)
                .ok_or(SealedError::SizeOverflow)?;
            if self.offsets.select(row_index + 1) != Some(position as u64) {
                return Err(SealedError::NonCanonical);
            }
            match row.storage {
                Storage::Inline(index) => {
                    if index != next_inline || row.count > INLINE_CAPACITY {
                        return Err(SealedError::NonCanonical);
                    }
                    let inline = self.inlines.get(index).ok_or(SealedError::NonCanonical)?;
                    if inline.len() != row.count || inline.descriptor() != row.key {
                        return Err(SealedError::NonCanonical);
                    }
                    next_inline += 1;
                }
                Storage::Run(index) => {
                    if index != next_run || row.count <= INLINE_CAPACITY {
                        return Err(SealedError::NonCanonical);
                    }
                    let run = self.runs.get(index).ok_or(SealedError::NonCanonical)?;
                    if run.neighbors.len() != row.count
                        || run.edge_ids.len() != row.count
                        || run.locators.len() != row.count
                    {
                        return Err(SealedError::NonCanonical);
                    }
                    crate::validate_spans(&run.spans, row.count).map_err(SealedError::Entry)?;
                    if run.spans.windows(2).any(|pair| {
                        pair[0].created_at == pair[1].created_at
                            && pair[0].retired_at == pair[1].retired_at
                    }) {
                        return Err(SealedError::NonCanonical);
                    }
                    next_run += 1;
                }
            }
            let mut previous = None;
            for at in 0..row.count {
                if at % 128 == 0 {
                    checkpoint()?;
                }
                let (entry, locator) = row.incidence(self, at).ok_or(SealedError::NonCanonical)?;
                crate::validate_entry(at, &entry).map_err(SealedError::Entry)?;
                let key = (entry.dst, entry.eid, entry.created_at);
                if previous.is_some_and(|previous| previous >= key)
                    || entry.created_at > scope.publication
                    || entry.retired_at.is_some_and(|retired| {
                        retired > scope.publication || retired <= scope.floor
                    })
                {
                    return Err(SealedError::NonCanonical);
                }
                previous = Some(key);
                let rank = self.destination_rank(entry.dst);
                if self.dictionary.get(rank) != Some(entry.dst) {
                    return Err(SealedError::NonCanonical);
                }
                used[rank] = true;
                if locator != 0 {
                    property_locator = property_locator
                        .checked_add(1)
                        .ok_or(SealedError::SizeOverflow)?;
                    if locator as usize != property_locator {
                        return Err(SealedError::NonCanonical);
                    }
                }
            }
        }
        if position != self.incidences
            || next_inline != self.inlines.len()
            || next_run != self.runs.len()
            || property_locator != self.properties.len()
            || used.iter().any(|used| !used)
        {
            return Err(SealedError::NonCanonical);
        }
        let mut property_bytes = 0usize;
        for row in &self.properties {
            checkpoint()?;
            if row.is_empty() {
                return Err(SealedError::NonCanonical);
            }
            let bytes = usize::try_from(admitted_row_bytes(row).map_err(SealedError::Property)?)
                .map_err(|_| SealedError::SizeOverflow)?;
            property_bytes = property_bytes
                .checked_add(bytes)
                .ok_or(SealedError::SizeOverflow)?;
            check_limit("property bytes", property_bytes, limits.max_property_bytes)?;
        }
        Ok(())
    }
}

pub(super) fn build(
    compacted: Compaction,
    limits: SealedLimits,
    checkpoint: &mut impl FnMut() -> Result<(), SealedError>,
) -> Result<Image, SealedError> {
    let count = compacted
        .blocks
        .iter()
        .try_fold(0usize, |sum, block| sum.checked_add(block.len()))
        .ok_or(SealedError::SizeOverflow)?;
    check_limit("incidences", count, limits.max_incidences)?;
    u32::try_from(count).map_err(|_| SealedError::SizeOverflow)?;
    if compacted.blocks.len() != compacted.block_props.len() {
        return Err(SealedError::NonCanonical);
    }
    let mut flat = reserved(count)?;
    for (block, mut props) in compacted.blocks.into_iter().zip(compacted.block_props) {
        checkpoint()?;
        if let Some(props) = &props
            && (props.locators.len() != block.len()
                || validate_locator_sequence(&props.locators).map_err(SealedError::Property)?
                    != props.rows.len())
        {
            return Err(SealedError::NonCanonical);
        }
        for (at, entry) in block.into_iter().enumerate() {
            let row = if let Some(props) = props.as_mut() {
                let locator = props.locators[at];
                if locator == 0 {
                    Vec::new()
                } else {
                    core::mem::take(&mut props.rows[usize::from(locator) - 1])
                }
            } else {
                Vec::new()
            };
            flat.push((entry, row));
        }
    }
    flat.sort_unstable_by_key(|(entry, _)| {
        (
            entry.src,
            entry.relation,
            entry.dst,
            entry.eid,
            entry.created_at,
        )
    });
    checkpoint()?;
    let mut destinations = reserved(count)?;
    destinations.extend(flat.iter().map(|(entry, _)| entry.dst));
    destinations.sort_unstable();
    destinations.dedup();
    let dictionary =
        IdentityColumn::try_new(&destinations, identity_limits(destinations.len(), limits))
            .map_err(SealedError::Identity)?;
    let row_count = flat
        .iter()
        .enumerate()
        .filter(|(at, (entry, _))| {
            *at == 0
                || (flat[*at - 1].0.src, flat[*at - 1].0.relation) != (entry.src, entry.relation)
        })
        .count();
    check_limit("descriptors", row_count, limits.max_rows)?;
    let mut rows = reserved(row_count)?;
    let mut inlines = Vec::new();
    let mut runs = Vec::new();
    let mut properties = Vec::new();
    let mut offsets = reserved(row_count.checked_add(1).ok_or(SealedError::SizeOverflow)?)?;
    offsets.push(0u64);
    let mut start = 0;
    let mut property_bytes = 0usize;
    while start < flat.len() {
        checkpoint()?;
        let first = flat[start].0;
        let key = DescriptorKey {
            src: first.src,
            relation: first.relation,
            direction: Direction::Outbound,
        };
        let mut end = start + 1;
        while end < flat.len() && (flat[end].0.src, flat[end].0.relation) == (key.src, key.relation)
        {
            end += 1;
        }
        let count = end - start;
        let mut locators = reserved(count)?;
        for (_, row) in &mut flat[start..end] {
            if row.is_empty() {
                locators.push(0);
            } else {
                let bytes =
                    usize::try_from(admitted_row_bytes(row).map_err(SealedError::Property)?)
                        .map_err(|_| SealedError::SizeOverflow)?;
                property_bytes = property_bytes
                    .checked_add(bytes)
                    .ok_or(SealedError::SizeOverflow)?;
                check_limit("property bytes", property_bytes, limits.max_property_bytes)?;
                properties
                    .try_reserve(1)
                    .map_err(|_| SealedError::AllocationFailed)?;
                properties.push(core::mem::take(row));
                locators
                    .push(u32::try_from(properties.len()).map_err(|_| SealedError::SizeOverflow)?);
            }
        }
        let storage = if count <= INLINE_CAPACITY {
            let mut slots = reserved(count)?;
            for (at, (entry, _)) in flat[start..end].iter().enumerate() {
                slots.push(InlineIncidence {
                    dst: entry.dst,
                    eid: entry.eid,
                    created_at: entry.created_at,
                    retired_at: entry.retired_at,
                    property_locator: locators[at],
                });
            }
            let row = InlineAdjacency::try_new(key, &slots).map_err(SealedError::Inline)?;
            inlines
                .try_reserve(1)
                .map_err(|_| SealedError::AllocationFailed)?;
            let index = inlines.len();
            inlines.push(row);
            Storage::Inline(index)
        } else {
            let mut neighbors = reserved(count)?;
            let mut edge_ids = reserved(count)?;
            let mut spans: Vec<VisibilityInterval> = Vec::new();
            for (at, (entry, _)) in flat[start..end].iter().enumerate() {
                if at % 128 == 0 {
                    checkpoint()?;
                }
                neighbors.push(
                    destinations
                        .binary_search(&entry.dst)
                        .expect("dictionary built from all entries") as u64,
                );
                edge_ids.push(entry.eid);
                if let Some(last) = spans.last_mut().filter(|last| {
                    last.created_at == entry.created_at && last.retired_at == entry.retired_at
                }) {
                    last.end_row = at as u32 + 1;
                } else {
                    spans
                        .try_reserve(1)
                        .map_err(|_| SealedError::AllocationFailed)?;
                    spans.push(VisibilityInterval {
                        start_row: at as u32,
                        end_row: at as u32 + 1,
                        created_at: entry.created_at,
                        retired_at: entry.retired_at,
                    });
                }
            }
            let neighbors = EliasFano::try_new(&neighbors, EntryLimit::new(count))
                .map_err(SealedError::EliasFano)?;
            let edge_ids = IdentityColumn::try_new(&edge_ids, identity_limits(count, limits))
                .map_err(SealedError::Identity)?;
            runs.try_reserve(1)
                .map_err(|_| SealedError::AllocationFailed)?;
            let index = runs.len();
            runs.push(Run {
                neighbors,
                edge_ids,
                spans,
                locators,
            });
            Storage::Run(index)
        };
        rows.push(Row {
            key,
            count,
            storage,
        });
        offsets.push(end as u64);
        start = end;
    }
    let offsets = EliasFano::try_new(&offsets, EntryLimit::new(row_count + 1))
        .map_err(SealedError::EliasFano)?;
    Ok(Image {
        dictionary,
        offsets,
        rows,
        inlines,
        runs,
        properties,
        incidences: count,
    })
}
