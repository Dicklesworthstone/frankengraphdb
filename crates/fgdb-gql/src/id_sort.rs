//! Sorting by engine identities in about two moves per item.
//!
//! Identities are keyed permutations, so their high bits are well spread. A
//! large unordered array is first distributed into buckets by the (at most 20) most
//! significant bits in which its leading keys differ, then each bucket is
//! sorted. Each item moves about twice, not once per quicksort level:
//! comparison-sorting a closure's 72-byte edges was a seventh of a pokec
//! reach_3hop and a fifth of a facebook two-hop (fgdb-61p1n). The buckets cost
//! a second array of the items while they fill. The first version is
//! NavyBrook's (parked as 01bef2c1); this one is generic over the item.
//!
//! Sorting is uncharged, and the result is the one `sort` gives the whole
//! array, so no answer, charge or refusal can tell the two apart.

/// At most this many items are sorted directly.
const DIRECT: usize = 1 << 12;

/// Sorts `items` exactly as `sort` sorts the whole slice, given that `sort`
/// orders by `key` first, ascending (items with equal keys may fall in any
/// order `sort` leaves them in). Large arrays are bucketed by the bits in
/// which their keys differ, and each bucket is then sorted by `sort`.
/// Already monotone identity groups sort only their secondary keys, without
/// allocating a second item array. Stable sorts retain equal-item order,
/// including when the identity groups initially arrive in descending order.
pub fn sort_by_leading_identity<T: Copy>(
    items: &mut Vec<T>,
    key: impl Fn(&T) -> u128,
    mut sort: impl FnMut(&mut [T]),
) {
    let Some(first) = items.first().map(&key) else {
        return;
    };
    if items.len() <= DIRECT {
        sort(items);
        return;
    }
    // Recognize canonical history runs during the existing key scan.
    // Once both monotone directions fail, retain the original OR fold.
    let mut differ = 0_u128;
    let (mut ascending, mut descending, mut repeated) = (true, true, false);
    let mut previous = first;
    let mut remaining = items.iter().skip(1);
    while let Some(item) = remaining.next() {
        let current = key(item);
        differ |= current ^ first;
        ascending &= previous <= current;
        descending &= previous >= current;
        repeated |= previous == current;
        previous = current;
        if !ascending && !descending {
            differ = remaining.fold(differ, |differ, item| differ | (key(item) ^ first));
            break;
        }
    }
    if ascending || descending {
        // One equal-key group still needs the complete secondary sort.
        if differ == 0 {
            sort(items);
        } else {
            if descending {
                items.reverse();
            }
            if repeated {
                for group in items.chunk_by_mut(|left, right| key(left) == key(right)) {
                    if descending {
                        // Restore each equal-key group's input order before
                        // invoking a potentially stable secondary sort.
                        group.reverse();
                    }
                    if group.len() > 1 {
                        sort(group);
                    }
                }
            }
        }
        return;
    }
    let top = u128::BITS - differ.leading_zeros();
    // About sixteen items per bucket, in at most 2^20 buckets.
    let bits = (usize::BITS - items.len().leading_zeros())
        .saturating_sub(4)
        .clamp(8, 20);
    if top <= bits {
        sort(items);
        return;
    }
    let shift = top - bits;
    let mask = (1_usize << bits) - 1;
    // The bits above top are equal; bucket order preserves key order.
    let bucket = |item: &T| ((key(item) >> shift) as usize) & mask;
    let mut starts = vec![0_usize; mask + 2];
    for item in items.iter() {
        starts[bucket(item) + 1] += 1;
    }
    for at in 1..starts.len() {
        starts[at] += starts[at - 1];
    }
    let mut next = starts.clone();
    let mut sorted = items.clone();
    for &item in items.iter() {
        let at = &mut next[bucket(&item)];
        sorted[*at] = item;
        *at += 1;
    }
    for range in starts.windows(2) {
        sort(&mut sorted[range[0]..range[1]]);
    }
    *items = sorted;
}

#[cfg(test)]
mod tests {
    use super::sort_by_leading_identity;

    /// Every shape identities can take sorts exactly as the comparison sort
    /// orders it, each item moving whole: random identities, sequential ones,
    /// ones sharing their high bits, a narrow run with one far outlier (one
    /// huge bucket), descending ones and ones differing in the top bit, at
    /// sizes on both sides of the direct sort; with distinct keys, and with
    /// pairs whose leading key repeats and whose whole pair may repeat.
    #[test]
    fn identity_order_is_the_comparison_sorts() {
        let mut state = 0x5eed_1d50_0000_0001_u64;
        let mut random = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            u128::from(state)
        };
        let mut checked = 0;
        for size in [0_usize, 1, 2, 4096, 4097, 70_000] {
            for shape in 0..6 {
                let mut identity = |at: u128| match shape {
                    0 => (random() << 64) | random(),
                    1 => at + 1,
                    2 => (0xabcd << 112) | (random() & 0xffff_ffff_ffff),
                    3 if at == 0 => u128::MAX,
                    3 => at * 3,
                    4 => u128::MAX - at * 7,
                    _ => ((at % 2) << 127) | at,
                };
                // Distinct leading keys, sorted by the key alone.
                let mut items: Vec<(u128, usize)> = (0..size as u128)
                    .map(|at| (identity(at), at as usize))
                    .collect();
                let mut expected = items.clone();
                expected.sort_unstable_by_key(|item| item.0);
                assert!(
                    expected.windows(2).all(|pair| pair[0].0 < pair[1].0),
                    "{size} {shape}: identities repeat"
                );
                sort_by_leading_identity(
                    &mut items,
                    |item| item.0,
                    |slice| {
                        slice.sort_unstable_by_key(|item| item.0);
                    },
                );
                assert_eq!(items, expected, "{size} {shape}");
                // Repeating leading keys and repeating whole pairs, sorted as
                // whole pairs.
                let mut pairs: Vec<(u128, u128)> = items
                    .iter()
                    .flat_map(|&(key, at)| {
                        let far = (at % 5) as u128;
                        [(key, far), (key, far ^ 1), (key, far)]
                    })
                    .collect();
                let mut expected = pairs.clone();
                expected.sort_unstable();
                sort_by_leading_identity(&mut pairs, |pair| pair.0, <[_]>::sort_unstable);
                assert_eq!(pairs, expected, "{size} {shape}: pairs");
                checked += 1;
            }
        }
        assert_eq!(checked, 36);
    }

    /// An array past 2^20 items takes more than sixteen bucket bits (a
    /// generation's lazy adjacency build sorts tens of millions of
    /// statements and incidences), and still sorts exactly as the
    /// comparison sort does.
    #[test]
    fn arrays_past_a_million_items_take_wider_buckets_and_the_same_order() {
        let mut state = 0x5eed_1d50_0000_0002_u64;
        let mut random = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            u128::from(state)
        };
        let mut items: Vec<(u128, u32)> = (0..1_100_000_u32)
            .map(|at| ((random() << 64) | random(), at))
            .collect();
        let mut expected = items.clone();
        expected.sort_unstable();
        sort_by_leading_identity(&mut items, |item| item.0, <[_]>::sort_unstable);
        assert_eq!(items, expected);
    }

    /// Grouped histories must retain secondary order and stable ties.
    #[test]
    fn monotone_history_groups_keep_stable_ties_and_the_original_buffer() {
        for descending in [false, true] {
            for versions in [1_usize, 2, 7] {
                let mut items = Vec::with_capacity(70_000);
                for group in 0..8_193_usize {
                    let group = if descending { 8_192 - group } else { group };
                    let identity = (0xcafe_u128 << 112) | ((group as u128) << 64) | 17;
                    for version in (0..versions).rev() {
                        items.push((identity, version, 2 * version));
                        items.push((identity, version, 2 * version + 1));
                    }
                }
                let mut expected = items.clone();
                expected.sort_by_key(|item| (item.0, item.1));
                let (buffer, capacity) = (items.as_ptr(), items.capacity());
                sort_by_leading_identity(
                    &mut items,
                    |item| item.0,
                    |slice| slice.sort_by_key(|item| (item.0, item.1)),
                );
                assert_eq!(
                    items, expected,
                    "descending={descending} versions={versions}"
                );
                assert_eq!(items.as_ptr(), buffer);
                assert_eq!(items.capacity(), capacity);
            }
        }
    }

    #[test]
    fn one_inversion_or_one_equal_identity_never_skips_secondary_sorting() {
        for size in [4_097_usize, 16_384] {
            for descending in [false, true] {
                for fault in [0, 1, size / 2, size - 2, size - 1] {
                    let mut items: Vec<_> = (0..size)
                        .map(|index| {
                            let identity = if descending { size - index } else { index };
                            ((identity as u128) << 64, size - index, index)
                        })
                        .collect();
                    if fault == 0 {
                        // One group exercises the all-equal fallback.
                        for item in &mut items {
                            item.0 = u128::MAX;
                        }
                    } else if fault + 1 == size {
                        items[fault].0 = items[fault - 1].0;
                    } else {
                        items.swap(fault, fault + 1);
                    }
                    let mut expected = items.clone();
                    expected.sort();
                    sort_by_leading_identity(&mut items, |item| item.0, <[_]>::sort);
                    assert_eq!(
                        items, expected,
                        "size={size} descending={descending} fault={fault}"
                    );
                }
            }
        }
    }
}
