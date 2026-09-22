//! Multiplicity-preserving CSR indexes for Strata's sealed incidence columns.
//!
//! A neighbor set is not a graph adjacency list: parallel edges and retained
//! content versions can occupy the same destination. This index compresses the
//! DISTINCT destinations with the existing Elias-Fano kernel and keeps a second
//! Elias-Fano directory of their incidence ranges. EIDs, version markers and
//! properties stay position-aligned in the owning Strata object. Intersections
//! return ranges, not a deduplicated replacement for those identity columns.
//!
//! This module assigns no object kind, ordinal-map authority, visibility policy
//! or retention floor. Its scalars are meaningful only under the owner's pinned
//! maps. It is an immutable physical index, not an authoritative graph store.

use core::fmt;
use core::iter::FusedIterator;
use core::ops::Range;

use crate::elias_fano::{EliasFano, EliasFanoError, EntryLimit};

/// Bounds for index construction. Input slices remain owned by the caller.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CsrLimits {
    pub max_rows: usize,
    pub max_incidences: usize,
    /// Logical 64-bit storage words, INCLUDING all EF rank directories.
    /// This is not an allocator/RSS claim; row metadata and construction
    /// scratch are bounded separately by `max_rows` and `max_incidences`.
    pub max_storage_words: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CsrError {
    RowLimit { rows: usize, limit: usize },
    IncidenceLimit { incidences: usize, limit: usize },
    StorageLimit { words: usize, limit: usize },
    NonMonotone { row: usize, index: usize },
    SizeOverflow,
    AllocationFailed { units: usize },
    EliasFano(EliasFanoError),
}

impl fmt::Display for CsrError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "sealed CSR index: {self:?}")
    }
}

impl std::error::Error for CsrError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::EliasFano(error) => Some(error),
            _ => None,
        }
    }
}

impl From<EliasFanoError> for CsrError {
    fn from(error: EliasFanoError) -> Self {
        Self::EliasFano(error)
    }
}

/// Immutable compressed row and multiplicity directories.
///
/// `row_offsets` locate incidences; `group_offsets` locate distinct destination
/// groups; `incidence_offsets` turn one group into a range in the owner's flat
/// EID/version/property columns. Empty rows occupy no destination payload.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MultigraphCsr {
    row_offsets: EliasFano,
    group_offsets: EliasFano,
    incidence_offsets: EliasFano,
    neighbors: Vec<EliasFano>,
    incidences: usize,
    storage_words: usize,
}

fn reserve<T>(units: usize) -> Result<Vec<T>, CsrError> {
    let mut result = Vec::new();
    result
        .try_reserve_exact(units)
        .map_err(|_| CsrError::AllocationFailed { units })?;
    Ok(result)
}

fn add(left: usize, right: usize) -> Result<usize, CsrError> {
    left.checked_add(right).ok_or(CsrError::SizeOverflow)
}

/// Exact size law of the EXISTING scalar kernel, evaluated before allocation.
/// Keep the differential size law below when changing that kernel's directory.
fn ef_words(count: usize, maximum: u64) -> Result<usize, CsrError> {
    if count == 0 {
        return Ok(0);
    }
    // The scalar kernel's rank domain is u32, even on a 64-bit host.
    u32::try_from(count).map_err(|_| CsrError::SizeOverflow)?;
    let count_u64 = u64::try_from(count).map_err(|_| CsrError::SizeOverflow)?;
    let ratio = maximum / count_u64;
    let low_bits = if ratio == 0 {
        0
    } else {
        u64::BITS - 1 - ratio.leading_zeros()
    };
    let low = count
        .checked_mul(low_bits as usize)
        .ok_or(CsrError::SizeOverflow)?
        .div_ceil(64);
    let high_bits = (maximum >> low_bits)
        .checked_add(count_u64)
        .ok_or(CsrError::SizeOverflow)?;
    let high = usize::try_from(high_bits)
        .map_err(|_| CsrError::SizeOverflow)?
        .div_ceil(64);
    add(add(low, high)?, high.div_ceil(2))
}

impl MultigraphCsr {
    /// Construct from nondecreasing rows, preserving EVERY repeated scalar.
    ///
    /// All shape, ordering and encoded-size limits are checked before creating
    /// any index payload. Input is never sorted or silently deduplicated.
    pub fn try_new(rows: &[&[u64]], limits: CsrLimits) -> Result<Self, CsrError> {
        if rows.len() > limits.max_rows {
            return Err(CsrError::RowLimit {
                rows: rows.len(),
                limit: limits.max_rows,
            });
        }
        let directory_len = add(rows.len(), 1)?;
        let mut incidences = 0usize;
        let mut groups = 0usize;
        let mut words = 0usize;
        for (row_index, row) in rows.iter().enumerate() {
            incidences = add(incidences, row.len())?;
            if incidences > limits.max_incidences {
                return Err(CsrError::IncidenceLimit {
                    incidences,
                    limit: limits.max_incidences,
                });
            }
            let mut distinct = usize::from(!row.is_empty());
            for (index, pair) in row.windows(2).enumerate() {
                if pair[0] > pair[1] {
                    return Err(CsrError::NonMonotone {
                        row: row_index,
                        index: index + 1,
                    });
                }
                distinct = add(distinct, usize::from(pair[0] != pair[1]))?;
            }
            groups = add(groups, distinct)?;
            words = add(words, ef_words(distinct, row.last().copied().unwrap_or(0))?)?;
        }
        let incidence_max = u64::try_from(incidences).map_err(|_| CsrError::SizeOverflow)?;
        let group_max = u64::try_from(groups).map_err(|_| CsrError::SizeOverflow)?;
        words = add(words, ef_words(directory_len, incidence_max)?)?;
        words = add(words, ef_words(directory_len, group_max)?)?;
        words = add(words, ef_words(add(groups, 1)?, incidence_max)?)?;
        if words > limits.max_storage_words {
            return Err(CsrError::StorageLimit {
                words,
                limit: limits.max_storage_words,
            });
        }

        let mut row_offsets = reserve(directory_len)?;
        let mut group_offsets = reserve(directory_len)?;
        let mut incidence_offsets = reserve(add(groups, 1)?)?;
        let mut neighbors = reserve(rows.len())?;
        let mut position = 0usize;
        let mut group_count = 0usize;
        row_offsets.push(0);
        group_offsets.push(0);
        for row in rows {
            let distinct_count = usize::from(!row.is_empty())
                + row.windows(2).filter(|pair| pair[0] != pair[1]).count();
            let mut distinct = reserve(distinct_count)?;
            for (local, &neighbor) in row.iter().enumerate() {
                if local == 0 || row[local - 1] != neighbor {
                    distinct.push(neighbor);
                    incidence_offsets.push(
                        u64::try_from(add(position, local)?).map_err(|_| CsrError::SizeOverflow)?,
                    );
                }
            }
            group_count = add(group_count, distinct.len())?;
            neighbors.push(EliasFano::try_new(
                &distinct,
                EntryLimit::new(distinct.len()),
            )?);
            position = add(position, row.len())?;
            row_offsets.push(u64::try_from(position).map_err(|_| CsrError::SizeOverflow)?);
            group_offsets.push(u64::try_from(group_count).map_err(|_| CsrError::SizeOverflow)?);
        }
        incidence_offsets.push(incidence_max);
        let result = Self {
            row_offsets: EliasFano::try_new(&row_offsets, EntryLimit::new(directory_len))?,
            group_offsets: EliasFano::try_new(&group_offsets, EntryLimit::new(directory_len))?,
            incidence_offsets: EliasFano::try_new(
                &incidence_offsets,
                EntryLimit::new(add(groups, 1)?),
            )?,
            neighbors,
            incidences,
            storage_words: words,
        };
        debug_assert_eq!(result.measured_storage_words(), words);
        Ok(result)
    }

    pub fn row_count(&self) -> usize {
        self.neighbors.len()
    }

    pub const fn incidence_count(&self) -> usize {
        self.incidences
    }

    pub const fn is_empty(&self) -> bool {
        self.incidences == 0
    }

    pub const fn logical_storage_words(&self) -> usize {
        self.storage_words
    }

    fn measured_storage_words(&self) -> usize {
        self.row_offsets.logical_storage_words()
            + self.group_offsets.logical_storage_words()
            + self.incidence_offsets.logical_storage_words()
            + self
                .neighbors
                .iter()
                .map(EliasFano::logical_storage_words)
                .sum::<usize>()
    }

    /// Borrow one row. No decoding of the incidence columns or allocation.
    pub fn row(&self, row: usize) -> Option<CsrRow<'_>> {
        let neighbors = self.neighbors.get(row)?;
        Some(CsrRow {
            index: self,
            neighbors,
            group_start: self.group_offsets.select(row)? as usize,
            start: self.row_offsets.select(row)? as usize,
            end: self.row_offsets.select(row + 1)? as usize,
        })
    }
}

/// A compressed row under the parent index's generation.
#[derive(Clone, Copy)]
pub struct CsrRow<'a> {
    index: &'a MultigraphCsr,
    neighbors: &'a EliasFano,
    group_start: usize,
    start: usize,
    end: usize,
}

/// All incidences with one destination, in the original canonical order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NeighborGroup {
    pub neighbor: u64,
    pub incidences: Range<usize>,
}

impl<'a> CsrRow<'a> {
    pub const fn len(self) -> usize {
        self.end - self.start
    }

    pub const fn is_empty(self) -> bool {
        self.start == self.end
    }

    pub fn distinct_len(self) -> usize {
        self.neighbors.len()
    }

    pub fn incidence_range(self) -> Range<usize> {
        self.start..self.end
    }

    fn group(self, local: usize) -> Option<NeighborGroup> {
        let neighbor = self.neighbors.select(local)?;
        let global = self.group_start + local;
        let start = self.index.incidence_offsets.select(global)? as usize;
        let end = self.index.incidence_offsets.select(global + 1)? as usize;
        Some(NeighborGroup {
            neighbor,
            incidences: start..end,
        })
    }

    /// Locate ALL parallel incidences, not just an arbitrary first EID.
    pub fn find(self, neighbor: u64) -> Option<Range<usize>> {
        let group = self.group(self.neighbors.rank_lt(neighbor))?;
        (group.neighbor == neighbor).then_some(group.incidences)
    }

    pub fn cursor(self) -> CsrCursor<'a> {
        CsrCursor { row: self, next: 0 }
    }

    /// Direct lower-bound over compressed neighbors; no full-list decode.
    /// Cost inherits the scalar EF kernel's documented logarithmic select/rank.
    pub fn lower_bound(self, neighbor: u64) -> CsrCursor<'a> {
        CsrCursor {
            row: self,
            next: self.neighbors.rank_lt(neighbor),
        }
    }

    /// Return matching destination groups with BOTH multiplicity ranges.
    /// The join consumer chooses its output semantics; no Cartesian product
    /// or lossy DISTINCT projection is materialized in this index.
    pub fn intersect(self, other: CsrRow<'a>) -> CsrIntersection<'a> {
        CsrIntersection {
            left: self.cursor().peekable(),
            right: other.cursor().peekable(),
        }
    }
}

pub struct CsrCursor<'a> {
    row: CsrRow<'a>,
    next: usize,
}

impl CsrCursor<'_> {
    /// Seek forward without ever revisiting already-consumed incidences.
    pub fn seek(&mut self, neighbor: u64) {
        self.next = self.next.max(self.row.neighbors.rank_lt(neighbor));
    }
}

impl Iterator for CsrCursor<'_> {
    type Item = NeighborGroup;

    fn next(&mut self) -> Option<Self::Item> {
        let group = self.row.group(self.next)?;
        self.next += 1;
        Some(group)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.row.distinct_len() - self.next;
        (remaining, Some(remaining))
    }
}

impl ExactSizeIterator for CsrCursor<'_> {}
impl FusedIterator for CsrCursor<'_> {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NeighborIntersection {
    pub neighbor: u64,
    pub left_incidences: Range<usize>,
    pub right_incidences: Range<usize>,
}

pub struct CsrIntersection<'a> {
    left: core::iter::Peekable<CsrCursor<'a>>,
    right: core::iter::Peekable<CsrCursor<'a>>,
}

impl Iterator for CsrIntersection<'_> {
    type Item = NeighborIntersection;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let left = self.left.peek()?.neighbor;
            let right = self.right.peek()?.neighbor;
            match left.cmp(&right) {
                core::cmp::Ordering::Less => {
                    self.left.next();
                }
                core::cmp::Ordering::Greater => {
                    self.right.next();
                }
                core::cmp::Ordering::Equal => {
                    return Some(NeighborIntersection {
                        neighbor: left,
                        left_incidences: self.left.next()?.incidences,
                        right_incidences: self.right.next()?.incidences,
                    });
                }
            }
        }
    }
}

impl FusedIterator for CsrIntersection<'_> {}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits() -> CsrLimits {
        CsrLimits {
            max_rows: 1024,
            max_incidences: 65536,
            max_storage_words: 65536,
        }
    }

    #[test]
    fn parallel_edges_and_empty_rows_keep_exact_flat_positions() {
        let csr = MultigraphCsr::try_new(&[&[], &[2, 2, 2, 9], &[], &[9, 9]], limits()).unwrap();
        assert_eq!(csr.incidence_count(), 6);
        assert_eq!(csr.row_count(), 4);
        assert_eq!(csr.row(0).unwrap().incidence_range(), 0..0);
        assert_eq!(csr.row(1).unwrap().find(2), Some(0..3));
        assert_eq!(csr.row(1).unwrap().find(9), Some(3..4));
        assert_eq!(csr.row(2).unwrap().incidence_range(), 4..4);
        assert_eq!(csr.row(3).unwrap().find(9), Some(4..6));
        assert_eq!(csr.row(3).unwrap().find(8), None);
        assert!(csr.row(4).is_none());
    }

    #[test]
    fn intersection_carries_both_bag_multiplicities() {
        let csr = MultigraphCsr::try_new(&[&[1, 3, 3, 8], &[3, 3, 3, 5, 8, 8]], limits()).unwrap();
        let groups: Vec<_> = csr.row(0).unwrap().intersect(csr.row(1).unwrap()).collect();
        assert_eq!(
            groups,
            vec![
                NeighborIntersection {
                    neighbor: 3,
                    left_incidences: 1..3,
                    right_incidences: 4..7
                },
                NeighborIntersection {
                    neighbor: 8,
                    left_incidences: 3..4,
                    right_incidences: 8..10
                },
            ]
        );
    }

    #[test]
    fn seek_is_forward_only_and_exhaustion_is_fused() {
        let csr = MultigraphCsr::try_new(&[&[0, 0, 7, u64::MAX, u64::MAX]], limits()).unwrap();
        let mut cursor = csr.row(0).unwrap().lower_bound(1);
        assert_eq!(cursor.next().unwrap().neighbor, 7);
        cursor.seek(0);
        assert_eq!(cursor.len(), 1);
        assert_eq!(cursor.next().unwrap().incidences, 3..5);
        cursor.seek(u64::MAX);
        assert_eq!(cursor.next(), None);
        assert_eq!(cursor.next(), None);
        assert_eq!(cursor.len(), 0);
    }

    #[test]
    fn zero_rows_and_only_empty_rows_are_distinct() {
        let none = MultigraphCsr::try_new(&[], limits()).unwrap();
        let empty = MultigraphCsr::try_new(&[&[], &[]], limits()).unwrap();
        assert!(none.is_empty() && empty.is_empty());
        assert_eq!(none.row_count(), 0);
        assert_eq!(empty.row_count(), 2);
        assert_eq!(empty.row(1).unwrap().cursor().next(), None);
    }

    #[test]
    fn order_and_every_resource_ceiling_fail_closed() {
        assert!(matches!(
            MultigraphCsr::try_new(&[&[2, 1]], limits()),
            Err(CsrError::NonMonotone { row: 0, index: 1 })
        ));
        assert!(matches!(
            MultigraphCsr::try_new(
                &[&[]],
                CsrLimits {
                    max_rows: 0,
                    ..limits()
                }
            ),
            Err(CsrError::RowLimit { .. })
        ));
        assert!(matches!(
            MultigraphCsr::try_new(
                &[&[1, 1]],
                CsrLimits {
                    max_incidences: 1,
                    ..limits()
                }
            ),
            Err(CsrError::IncidenceLimit { .. })
        ));
        let rows: &[&[u64]] = &[&[1, 1, u64::MAX]];
        let exact = MultigraphCsr::try_new(rows, limits())
            .unwrap()
            .logical_storage_words();
        assert!(
            MultigraphCsr::try_new(
                rows,
                CsrLimits {
                    max_storage_words: exact,
                    ..limits()
                }
            )
            .is_ok()
        );
        assert!(matches!(
            MultigraphCsr::try_new(
                rows,
                CsrLimits {
                    max_storage_words: exact - 1,
                    ..limits()
                }
            ),
            Err(CsrError::StorageLimit { .. })
        ));
    }

    #[test]
    fn ef_preflight_matches_the_actual_kernel_including_u64_extremes() {
        for values in [
            vec![],
            vec![0],
            vec![u64::MAX],
            vec![0, u64::MAX],
            vec![9; 257],
            (0..1000).collect(),
        ] {
            let ef = EliasFano::try_new(&values, EntryLimit::new(values.len())).unwrap();
            assert_eq!(
                ef_words(values.len(), values.last().copied().unwrap_or(0)).unwrap(),
                ef.logical_storage_words()
            );
        }
    }

    #[test]
    fn deterministic_generated_multigraphs_round_trip_group_ranges() {
        let mut state = 0x6a09_e667_f3bc_c909u64;
        for row_count in 0..32 {
            let mut rows = Vec::new();
            for row in 0..row_count {
                let mut values = Vec::new();
                for _ in 0..(row * 13 % 97) {
                    state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
                    values.push((state >> 32) % 31);
                }
                values.sort_unstable();
                rows.push(values);
            }
            let borrowed: Vec<&[u64]> = rows.iter().map(Vec::as_slice).collect();
            let csr = MultigraphCsr::try_new(&borrowed, limits()).unwrap();
            let mut position = 0;
            for (row, expected) in rows.iter().enumerate() {
                let view = csr.row(row).unwrap();
                assert_eq!(view.incidence_range(), position..position + expected.len());
                let mut actual = Vec::new();
                for group in view.cursor() {
                    assert_eq!(group.incidences.start, position);
                    actual.extend(core::iter::repeat_n(group.neighbor, group.incidences.len()));
                    position = group.incidences.end;
                }
                assert_eq!(&actual, expected);
            }
            assert_eq!(position, csr.incidence_count());
        }
    }
}
