//! Sorted-set gallop/seek and intersection kernels for Loom's `FreeJoin`
//! leapfrog triejoin (`fgdb-rc1111-loom-conformal-freejoin-txo5d`; plan §8.2, §8.7).
//!
//! Every kernel has a portable scalar specification (`*_scalar`) and a 4-wide
//! `core::simd::Simd<u64, 4>` vector path (`PortableSimd4`) built entirely in
//! safe Rust (`0` `unsafe` blocks). Every path in [`SORTED_SET_COMPILED_PATHS`]
//! is bit-identical to `PortableScalar` on all sorted inputs, including empty
//! slices, duplicates, skewed degree ratios, and boundary values (`0`, `u64::MAX`,
//! `u128::MAX`).

use core::simd::Simd;
use core::simd::cmp::{SimdPartialEq, SimdPartialOrd};

/// Width of one `u64` SIMD lane group.
pub const SIMD_U64_LANES: usize = 4;

/// Compiled sorted-set kernel backend.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum SortedSetDispatch {
    /// Portable scalar exponential-gallop + binary-search specification.
    PortableScalar,
    /// Four-lane `Simd<u64, 4>` block gallop + lane-mask refinement.
    PortableSimd4,
}

/// All sorted-set backends compiled into this build.
pub const SORTED_SET_COMPILED_PATHS: &[SortedSetDispatch] = &[
    SortedSetDispatch::PortableScalar,
    SortedSetDispatch::PortableSimd4,
];

/// Returns the default active sorted-set backend.
#[must_use]
pub const fn active_sorted_set_path() -> SortedSetDispatch {
    SortedSetDispatch::PortableSimd4
}

/// Portable scalar specification: returns `(first_index_ge_target, comparisons)`
/// in `sorted[start..]`, expressed as an absolute index in `0..=sorted.len()`.
#[must_use]
pub fn seek_ge_u64_scalar(sorted: &[u64], start: usize, target: u64) -> (usize, u64) {
    let len = sorted.len();
    if start >= len {
        return (len, 0);
    }
    let mut comparisons = 1_u64;
    if sorted[start] >= target {
        return (start, comparisons);
    }
    // Exponential gallop from `start` so skewed leapfrog intersections run in
    // `O(log(delta))` rather than `O(log N)`.
    let mut bound = 1_usize;
    let mut low = start;
    while let Some(probe) = start.checked_add(bound)
        && probe < len
    {
        comparisons += 1;
        if sorted[probe] < target {
            low = probe;
            bound = bound.saturating_mul(2);
        } else {
            let mut high = probe;
            low += 1;
            while low < high {
                let mid = low + (high - low) / 2;
                comparisons += 1;
                if sorted[mid] < target {
                    low = mid + 1;
                } else {
                    high = mid;
                }
            }
            return (low, comparisons);
        }
    }
    let mut high = len;
    low += 1;
    while low < high {
        let mid = low + (high - low) / 2;
        comparisons += 1;
        if sorted[mid] < target {
            low = mid + 1;
        } else {
            high = mid;
        }
    }
    (low, comparisons)
}

/// Four-lane `Simd<u64, 4>` gallop + vectorized window refinement.
/// Bit-identical index result to [`seek_ge_u64_scalar`].
#[must_use]
fn seek_ge_u64_simd4(sorted: &[u64], start: usize, target: u64) -> (usize, u64) {
    let len = sorted.len();
    if start >= len {
        return (len, 0);
    }
    let mut comparisons = 1_u64;
    if sorted[start] >= target {
        return (start, comparisons);
    }
    let target_vec = Simd::<u64, SIMD_U64_LANES>::splat(target);

    // Fast 4-lane local window check before exponential gallop: hits nearby
    // dense neighbors in a single vector comparison.
    if start + 1 + SIMD_U64_LANES <= len {
        let chunk =
            Simd::<u64, SIMD_U64_LANES>::from_slice(&sorted[start + 1..start + 1 + SIMD_U64_LANES]);
        comparisons += 1;
        let ge_mask = chunk.simd_ge(target_vec).to_bitmask();
        if ge_mask != 0 {
            return (start + 1 + (ge_mask.trailing_zeros() as usize), comparisons);
        }
    }

    let mut low = (start + 1 + SIMD_U64_LANES).min(len);
    if low >= len {
        if start + 1 < len {
            let (idx, scalar_cmp) = seek_ge_u64_scalar(sorted, start + 1, target);
            return (idx, comparisons + scalar_cmp);
        }
        return (len, comparisons);
    }

    let mut step = SIMD_U64_LANES * 2;
    let mut high = len;
    while let Some(probe) = low.checked_add(step) {
        if probe >= len {
            break;
        }
        comparisons += 1;
        if sorted[probe] < target {
            low = probe + 1;
            step = step.saturating_mul(2);
        } else {
            high = probe;
            break;
        }
    }

    // Refine `[low, high)` down to a 16-element window with binary search, then
    // finish with 4-wide SIMD comparisons.
    while high.saturating_sub(low) > SIMD_U64_LANES * 4 {
        let mid = low + (high - low) / 2;
        comparisons += 1;
        if sorted[mid] < target {
            low = mid + 1;
        } else {
            high = mid;
        }
    }
    while low + SIMD_U64_LANES <= high {
        let chunk = Simd::<u64, SIMD_U64_LANES>::from_slice(&sorted[low..low + SIMD_U64_LANES]);
        comparisons += 1;
        let ge_mask = chunk.simd_ge(target_vec).to_bitmask();
        if ge_mask != 0 {
            return (low + (ge_mask.trailing_zeros() as usize), comparisons);
        }
        low += SIMD_U64_LANES;
    }
    while low < high {
        comparisons += 1;
        if sorted[low] >= target {
            return (low, comparisons);
        }
        low += 1;
    }
    (low, comparisons)
}

/// Runs `seek_ge_u64` via the named backend.
#[must_use]
pub fn seek_ge_u64_via(
    path: SortedSetDispatch,
    sorted: &[u64],
    start: usize,
    target: u64,
) -> (usize, u64) {
    match path {
        SortedSetDispatch::PortableScalar => seek_ge_u64_scalar(sorted, start, target),
        SortedSetDispatch::PortableSimd4 => seek_ge_u64_simd4(sorted, start, target),
    }
}

/// Runs `seek_ge_u64` on the active backend.
#[must_use]
pub fn seek_ge_u64(sorted: &[u64], start: usize, target: u64) -> (usize, u64) {
    seek_ge_u64_via(active_sorted_set_path(), sorted, start, target)
}

/// Portable scalar specification for sorted `u128` slices (`VId(u128)`).
#[must_use]
pub fn seek_ge_u128_scalar(sorted: &[u128], start: usize, target: u128) -> (usize, u64) {
    let len = sorted.len();
    if start >= len {
        return (len, 0);
    }
    let mut comparisons = 1_u64;
    if sorted[start] >= target {
        return (start, comparisons);
    }
    let mut bound = 1_usize;
    let mut low = start;
    while let Some(probe) = start.checked_add(bound)
        && probe < len
    {
        comparisons += 1;
        if sorted[probe] < target {
            low = probe;
            bound = bound.saturating_mul(2);
        } else {
            let mut high = probe;
            low += 1;
            while low < high {
                let mid = low + (high - low) / 2;
                comparisons += 1;
                if sorted[mid] < target {
                    low = mid + 1;
                } else {
                    high = mid;
                }
            }
            return (low, comparisons);
        }
    }
    let mut high = len;
    low += 1;
    while low < high {
        let mid = low + (high - low) / 2;
        comparisons += 1;
        if sorted[mid] < target {
            low = mid + 1;
        } else {
            high = mid;
        }
    }
    (low, comparisons)
}

/// Four-lane `Simd<u64, 4>` high-word / low-word lexicographic gallop + seek
/// for sorted `u128` (`VId`) slices. Bit-identical index result to
/// [`seek_ge_u128_scalar`].
#[must_use]
fn seek_ge_u128_simd4(sorted: &[u128], start: usize, target: u128) -> (usize, u64) {
    let len = sorted.len();
    if start >= len {
        return (len, 0);
    }
    let mut comparisons = 1_u64;
    if sorted[start] >= target {
        return (start, comparisons);
    }
    let target_hi = (target >> 64) as u64;
    let target_lo = target as u64;
    let hi_vec = Simd::<u64, SIMD_U64_LANES>::splat(target_hi);
    let lo_vec = Simd::<u64, SIMD_U64_LANES>::splat(target_lo);

    let simd_chunk_ge_mask = |window: &[u128]| -> u64 {
        let hi = Simd::<u64, SIMD_U64_LANES>::from_array([
            (window[0] >> 64) as u64,
            (window[1] >> 64) as u64,
            (window[2] >> 64) as u64,
            (window[3] >> 64) as u64,
        ]);
        let lo = Simd::<u64, SIMD_U64_LANES>::from_array([
            window[0] as u64,
            window[1] as u64,
            window[2] as u64,
            window[3] as u64,
        ]);
        let gt_hi = hi.simd_gt(hi_vec).to_bitmask();
        let eq_hi = hi.simd_eq(hi_vec).to_bitmask();
        let ge_lo = lo.simd_ge(lo_vec).to_bitmask();
        gt_hi | (eq_hi & ge_lo)
    };

    if start + 1 + SIMD_U64_LANES <= len {
        comparisons += 1;
        let mask = simd_chunk_ge_mask(&sorted[start + 1..start + 1 + SIMD_U64_LANES]);
        if mask != 0 {
            return (start + 1 + (mask.trailing_zeros() as usize), comparisons);
        }
    }

    let mut low = (start + 1 + SIMD_U64_LANES).min(len);
    if low >= len {
        let (idx, scalar_cmp) = seek_ge_u128_scalar(sorted, start + 1, target);
        return (idx, comparisons + scalar_cmp);
    }

    let mut step = SIMD_U64_LANES * 2;
    let mut high = len;
    while let Some(probe) = low.checked_add(step) {
        if probe >= len {
            break;
        }
        comparisons += 1;
        if sorted[probe] < target {
            low = probe + 1;
            step = step.saturating_mul(2);
        } else {
            high = probe;
            break;
        }
    }

    while high.saturating_sub(low) > SIMD_U64_LANES * 4 {
        let mid = low + (high - low) / 2;
        comparisons += 1;
        if sorted[mid] < target {
            low = mid + 1;
        } else {
            high = mid;
        }
    }
    while low + SIMD_U64_LANES <= high {
        comparisons += 1;
        let mask = simd_chunk_ge_mask(&sorted[low..low + SIMD_U64_LANES]);
        if mask != 0 {
            return (low + (mask.trailing_zeros() as usize), comparisons);
        }
        low += SIMD_U64_LANES;
    }
    while low < high {
        comparisons += 1;
        if sorted[low] >= target {
            return (low, comparisons);
        }
        low += 1;
    }
    (low, comparisons)
}

/// Runs `seek_ge_u128` via the named backend.
#[must_use]
pub fn seek_ge_u128_via(
    path: SortedSetDispatch,
    sorted: &[u128],
    start: usize,
    target: u128,
) -> (usize, u64) {
    match path {
        SortedSetDispatch::PortableScalar => seek_ge_u128_scalar(sorted, start, target),
        SortedSetDispatch::PortableSimd4 => seek_ge_u128_simd4(sorted, start, target),
    }
}

/// Runs `seek_ge_u128` on the active backend.
#[must_use]
pub fn seek_ge_u128(sorted: &[u128], start: usize, target: u128) -> (usize, u64) {
    seek_ge_u128_via(active_sorted_set_path(), sorted, start, target)
}

/// Distinct sorted-set intersection over `u64` via the named backend.
/// Appends strictly ascending distinct values present in both `left` and `right`
/// to `out` and returns total work comparisons.
pub fn intersect_sorted_u64_via(
    path: SortedSetDispatch,
    left: &[u64],
    right: &[u64],
    out: &mut Vec<u64>,
) -> u64 {
    let mut comparisons = 0_u64;
    let mut i = 0_usize;
    let mut j = 0_usize;
    while i < left.len() && j < right.len() {
        let a = left[i];
        let b = right[j];
        comparisons += 1;
        if a == b {
            if out.last().copied() != Some(a) {
                out.push(a);
            }
            let (next_i, ci) = match a.checked_add(1) {
                Some(next) => seek_ge_u64_via(path, left, i + 1, next),
                None => (left.len(), 0),
            };
            let (next_j, cj) = match b.checked_add(1) {
                Some(next) => seek_ge_u64_via(path, right, j + 1, next),
                None => (right.len(), 0),
            };
            comparisons += ci + cj;
            i = next_i;
            j = next_j;
        } else if a < b {
            let (next_i, ci) = seek_ge_u64_via(path, left, i + 1, b);
            comparisons += ci;
            i = next_i;
        } else {
            let (next_j, cj) = seek_ge_u64_via(path, right, j + 1, a);
            comparisons += cj;
            j = next_j;
        }
    }
    comparisons
}

/// Portable scalar sorted-set intersection over `u64`.
pub fn intersect_sorted_u64_scalar(left: &[u64], right: &[u64], out: &mut Vec<u64>) -> u64 {
    intersect_sorted_u64_via(SortedSetDispatch::PortableScalar, left, right, out)
}

/// Active-backend sorted-set intersection over `u64`.
pub fn intersect_sorted_u64(left: &[u64], right: &[u64], out: &mut Vec<u64>) -> u64 {
    intersect_sorted_u64_via(active_sorted_set_path(), left, right, out)
}

/// Distinct sorted-set intersection over `u128` (`VId`) via the named backend.
pub fn intersect_sorted_u128_via(
    path: SortedSetDispatch,
    left: &[u128],
    right: &[u128],
    out: &mut Vec<u128>,
) -> u64 {
    let mut comparisons = 0_u64;
    let mut i = 0_usize;
    let mut j = 0_usize;
    while i < left.len() && j < right.len() {
        let a = left[i];
        let b = right[j];
        comparisons += 1;
        if a == b {
            if out.last().copied() != Some(a) {
                out.push(a);
            }
            let (next_i, ci) = match a.checked_add(1) {
                Some(next) => seek_ge_u128_via(path, left, i + 1, next),
                None => (left.len(), 0),
            };
            let (next_j, cj) = match b.checked_add(1) {
                Some(next) => seek_ge_u128_via(path, right, j + 1, next),
                None => (right.len(), 0),
            };
            comparisons += ci + cj;
            i = next_i;
            j = next_j;
        } else if a < b {
            let (next_i, ci) = seek_ge_u128_via(path, left, i + 1, b);
            comparisons += ci;
            i = next_i;
        } else {
            let (next_j, cj) = seek_ge_u128_via(path, right, j + 1, a);
            comparisons += cj;
            j = next_j;
        }
    }
    comparisons
}

/// Portable scalar sorted-set intersection over `u128`.
pub fn intersect_sorted_u128_scalar(left: &[u128], right: &[u128], out: &mut Vec<u128>) -> u64 {
    intersect_sorted_u128_via(SortedSetDispatch::PortableScalar, left, right, out)
}

/// Active-backend sorted-set intersection over `u128`.
pub fn intersect_sorted_u128(left: &[u128], right: &[u128], out: &mut Vec<u128>) -> u64 {
    intersect_sorted_u128_via(active_sorted_set_path(), left, right, out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simd4_and_scalar_seek_and_intersect_are_bit_identical_across_boundary_and_skewed_sets() {
        let u64_sets: &[&[u64]] = &[
            &[],
            &[0],
            &[u64::MAX],
            &[0, 1, 1, 2, 3, 5, 8, 13, 21, 34, 55, 89, 144, u64::MAX],
            &[
                2, 4, 6, 8, 10, 12, 14, 16, 18, 20, 22, 24, 26, 28, 30, 32, 34, 36, 38, 40, 55,
                144, 999,
            ],
        ];
        let dense: Vec<u64> = (0..513).map(|i| i * 3).collect();
        let skewed: Vec<u64> = vec![0, 27, 300, 1536, u64::MAX];
        let mut all_u64: Vec<Vec<u64>> = u64_sets.iter().map(|s| s.to_vec()).collect();
        all_u64.push(dense);
        all_u64.push(skewed);

        for set in &all_u64 {
            for start in 0..=set.len() {
                for target in [0, 1, 2, 13, 27, 55, 145, 300, 1536, u64::MAX - 1, u64::MAX] {
                    let (expected, _) = seek_ge_u64_scalar(set, start, target);
                    for &path in SORTED_SET_COMPILED_PATHS {
                        let (actual, _) = seek_ge_u64_via(path, set, start, target);
                        assert_eq!(actual, expected, "u64 seek mismatch path={path:?}");
                    }
                }
            }
        }
        for left in &all_u64 {
            for right in &all_u64 {
                let mut expected = Vec::new();
                intersect_sorted_u64_scalar(left, right, &mut expected);
                for &path in SORTED_SET_COMPILED_PATHS {
                    let mut actual = Vec::new();
                    intersect_sorted_u64_via(path, left, right, &mut actual);
                    assert_eq!(actual, expected, "u64 intersect mismatch path={path:?}");
                }
            }
        }

        // u128 differential including high-word boundaries (`> u64::MAX`)
        let hi_base = 1_u128 << 64;
        let u128_a: Vec<u128> = (0..130_u128)
            .map(|i| {
                if i < 64 {
                    i * 5
                } else {
                    hi_base + (i - 64) * 7
                }
            })
            .collect();
        let u128_b: Vec<u128> = vec![
            0,
            25,
            315,
            hi_base,
            hi_base + 49,
            hi_base + 441,
            u128::MAX - 1,
            u128::MAX,
        ];
        let mut u128_c = u128_a.clone();
        u128_c.push(u128::MAX);
        for set in [&u128_a, &u128_b, &u128_c] {
            for start in [
                0,
                1,
                3,
                4,
                5,
                17,
                63,
                64,
                set.len().saturating_sub(1),
                set.len(),
            ] {
                for &target in &u128_b {
                    let (expected, _) = seek_ge_u128_scalar(set, start, target);
                    for &path in SORTED_SET_COMPILED_PATHS {
                        let (actual, _) = seek_ge_u128_via(path, set, start, target);
                        assert_eq!(actual, expected, "u128 seek mismatch path={path:?}");
                    }
                }
            }
        }
        let mut expected_128 = Vec::new();
        intersect_sorted_u128_scalar(&u128_c, &u128_b, &mut expected_128);
        for &path in SORTED_SET_COMPILED_PATHS {
            let mut actual_128 = Vec::new();
            intersect_sorted_u128_via(path, &u128_c, &u128_b, &mut actual_128);
            assert_eq!(actual_128, expected_128, "u128 intersect mismatch");
        }
    }
}
