//! Short-lived ordered seeks for owners that cannot retain a borrowed iterator.

use super::{ZSet, ZWeight};
use std::ops::Bound::{Excluded, Unbounded};

impl<T: Ord> ZSet<T> {
    /// Borrow the first supported key strictly greater than `after`, or the
    /// first key when `after` is None. The bound need not itself be present.
    /// Negative weights remain support; consolidated zero weights are absent.
    ///
    /// This seeks the existing ordered index rather than scanning a consumed
    /// prefix. It copies no key or weight and requires no Clone bound. Returned
    /// references borrow only this value, not the search key. Like weight/iter,
    /// callers own comparison cost, admission, copying and cancellation checks.
    pub fn first_after(&self, after: Option<&T>) -> Option<(&T, &ZWeight)> {
        match after {
            None => self.entries.first_key_value(),
            Some(key) => self.entries.range::<T, _>((Excluded(key), Unbounded)).next(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::LimbLimit;
    use std::cell::Cell;
    use std::cmp::Ordering;

    #[test]
    fn seek_matches_independent_ordered_support_for_present_and_missing_bounds() {
        for mask in 0..256_u32 {
            let set = ZSet::from_updates(
                (0..8).map(|key| {
                    let weight = if mask & (1 << key) == 0 {
                        0
                    } else {
                        2 * (key % 2) - 1
                    };
                    (key * 3, ZWeight::from_i128(i128::from(weight)))
                }),
                LimbLimit::new(4),
                &mut |_| Ok::<_, ()>(()),
            )
            .unwrap();
            let expected: Vec<_> = (0..8)
                .filter(|key| mask & (1 << key) != 0)
                .map(|key| key * 3)
                .collect();
            for bound in std::iter::once(None).chain((-1..=25).map(Some)) {
                let want = expected
                    .iter()
                    .copied()
                    .find(|key| bound.is_none_or(|b| *key > b));
                let got = set.first_after(bound.as_ref());
                assert_eq!(got.map(|(key, _)| *key), want);
                if let Some((key, weight)) = got {
                    assert!(std::ptr::eq(weight, set.weight(key).unwrap()));
                    assert!(!weight.is_zero());
                }
            }
        }
    }

    #[test]
    fn successor_does_not_clone_keys_or_walk_the_already_consumed_prefix() {
        struct Key<'a> {
            value: usize,
            comparisons: &'a Cell<usize>,
        }
        impl PartialEq for Key<'_> {
            fn eq(&self, other: &Self) -> bool {
                self.value == other.value
            }
        }
        impl Eq for Key<'_> {}
        impl PartialOrd for Key<'_> {
            fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
                Some(self.cmp(other))
            }
        }
        impl Ord for Key<'_> {
            fn cmp(&self, other: &Self) -> Ordering {
                self.comparisons.set(self.comparisons.get() + 1);
                self.value.cmp(&other.value)
            }
        }
        let comparisons = Cell::new(0);
        let set = ZSet::from_updates(
            (0..16_384).map(|value| {
                (Key { value, comparisons: &comparisons }, ZWeight::ONE)
            }),
            LimbLimit::new(4),
            &mut |_| Ok::<_, ()>(()),
        )
        .unwrap();
        comparisons.set(0);
        let found = {
            let probe = Key { value: 16_382, comparisons: &comparisons };
            set.first_after(Some(&probe)).unwrap()
        };
        assert_eq!(found.0.value, 16_383);
        assert!(
            comparisons.get() < 128,
            "seek scanned prefix: {}",
            comparisons.get()
        );
        assert_eq!(*found.1, ZWeight::ONE);
    }
}
