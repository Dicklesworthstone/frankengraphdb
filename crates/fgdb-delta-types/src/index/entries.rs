//! Persistent ordered storage for a retained delta window.
//!
//! Generations share the tree, not just the batch payloads. A unique tree
//! mutates in place; a pinned generation copies only the balanced search path.
//! Keys remain explicit because imported/test-shaped windows can be sparse or
//! incoherent, and verification must diagnose their actual stored keys.

use std::sync::Arc;

type Link<T> = Option<Arc<Node<T>>>;

#[derive(Debug)]
struct Node<T> {
    key: u64,
    value: Arc<T>,
    left: Link<T>,
    right: Link<T>,
    height: u8,
    len: usize,
}

impl<T> Clone for Node<T> {
    fn clone(&self) -> Self {
        Self {
            key: self.key,
            value: Arc::clone(&self.value),
            left: self.left.clone(),
            right: self.right.clone(),
            height: self.height,
            len: self.len,
        }
    }
}

fn height<T>(link: &Link<T>) -> u8 {
    link.as_ref().map_or(0, |node| node.height)
}

fn len<T>(link: &Link<T>) -> usize {
    link.as_ref().map_or(0, |node| node.len)
}

impl<T> Node<T> {
    fn new(key: u64, value: Arc<T>, left: Link<T>, right: Link<T>) -> Self {
        let mut node = Self {
            key,
            value,
            left,
            right,
            height: 1,
            len: 1,
        };
        node.refresh();
        node
    }

    fn refresh(&mut self) {
        // An AVL tree over u64 keys cannot reach height 94. The number of
        // allocated nodes bounds len, so neither metadata sum can overflow.
        self.height = 1 + height(&self.left).max(height(&self.right));
        self.len = 1 + len(&self.left) + len(&self.right);
    }
}

fn rotate_left<T>(mut root: Arc<Node<T>>) -> Arc<Node<T>> {
    let mut right = Arc::make_mut(&mut root)
        .right
        .take()
        .expect("left rotation has a right child");
    let middle = Arc::make_mut(&mut right).left.take();
    let node = Arc::make_mut(&mut root);
    node.right = middle;
    node.refresh();
    let node = Arc::make_mut(&mut right);
    node.left = Some(root);
    node.refresh();
    right
}

fn rotate_right<T>(mut root: Arc<Node<T>>) -> Arc<Node<T>> {
    let mut left = Arc::make_mut(&mut root)
        .left
        .take()
        .expect("right rotation has a left child");
    let middle = Arc::make_mut(&mut left).right.take();
    let node = Arc::make_mut(&mut root);
    node.left = middle;
    node.refresh();
    let node = Arc::make_mut(&mut left);
    node.right = Some(root);
    node.refresh();
    left
}

fn balance<T>(mut root: Arc<Node<T>>) -> Arc<Node<T>> {
    let node = Arc::make_mut(&mut root);
    node.refresh();
    if height(&node.left) > height(&node.right) + 1 {
        let left = node
            .left
            .as_ref()
            .expect("left-heavy tree has a left child");
        if height(&left.right) > height(&left.left) {
            node.left = node.left.take().map(rotate_left);
        }
        rotate_right(root)
    } else if height(&node.right) > height(&node.left) + 1 {
        let right = node
            .right
            .as_ref()
            .expect("right-heavy tree has a right child");
        if height(&right.left) > height(&right.right) {
            node.right = node.right.take().map(rotate_right);
        }
        rotate_left(root)
    } else {
        root
    }
}

fn insert<T>(root: Link<T>, key: u64, value: Arc<T>) -> Arc<Node<T>> {
    let Some(mut root) = root else {
        return Arc::new(Node::new(key, value, None, None));
    };
    let node = Arc::make_mut(&mut root);
    match key.cmp(&node.key) {
        core::cmp::Ordering::Less => node.left = Some(insert(node.left.take(), key, value)),
        core::cmp::Ordering::Greater => node.right = Some(insert(node.right.take(), key, value)),
        core::cmp::Ordering::Equal => node.value = value,
    }
    balance(root)
}

/// Join two balanced trees around a separating entry. Unlike one rotation
/// after a prefix deletion, this handles an arbitrary height difference.
fn join<T>(left: Link<T>, key: u64, value: Arc<T>, right: Link<T>) -> Arc<Node<T>> {
    if height(&left) > height(&right) + 1 {
        let mut root = left.expect("taller left tree is nonempty");
        let node = Arc::make_mut(&mut root);
        node.right = Some(join(node.right.take(), key, value, right));
        balance(root)
    } else if height(&right) > height(&left) + 1 {
        let mut root = right.expect("taller right tree is nonempty");
        let node = Arc::make_mut(&mut root);
        node.left = Some(join(left, key, value, node.left.take()));
        balance(root)
    } else {
        Arc::new(Node::new(key, value, left, right))
    }
}

fn split<T>(root: Link<T>, through: u64) -> (Link<T>, Link<T>) {
    let Some(root) = root else {
        return (None, None);
    };
    let Node {
        key,
        value,
        left,
        right,
        ..
    } = Arc::unwrap_or_clone(root);
    if key <= through {
        let (middle, retained) = split(right, through);
        (Some(join(left, key, value, middle)), retained)
    } else {
        let (retired, middle) = split(left, through);
        (retired, Some(join(middle, key, value, right)))
    }
}

fn drain<T>(root: Link<T>, values: &mut Vec<Arc<T>>) {
    if let Some(root) = root {
        let Node {
            value, left, right, ..
        } = Arc::unwrap_or_clone(root);
        drain(left, values);
        values.push(value);
        drain(right, values);
    }
}

pub(super) struct Entries<T> {
    root: Link<T>,
}

impl<T: core::fmt::Debug> core::fmt::Debug for Entries<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_map().entries(self.iter()).finish()
    }
}

impl<T> Clone for Entries<T> {
    fn clone(&self) -> Self {
        Self {
            root: self.root.clone(),
        }
    }
}

impl<T: PartialEq> PartialEq for Entries<T> {
    fn eq(&self, other: &Self) -> bool {
        self.len() == other.len() && self.iter().eq(other.iter())
    }
}

impl<T> Entries<T> {
    pub(super) fn new() -> Self {
        Self { root: None }
    }

    pub(super) fn len(&self) -> usize {
        len(&self.root)
    }

    pub(super) fn is_empty(&self) -> bool {
        self.root.is_none()
    }

    pub(super) fn get(&self, key: &u64) -> Option<&Arc<T>> {
        let mut current = self.root.as_deref();
        while let Some(node) = current {
            match key.cmp(&node.key) {
                core::cmp::Ordering::Less => current = node.left.as_deref(),
                core::cmp::Ordering::Greater => current = node.right.as_deref(),
                core::cmp::Ordering::Equal => return Some(&node.value),
            }
        }
        None
    }

    pub(super) fn contains_key(&self, key: &u64) -> bool {
        self.get(key).is_some()
    }

    pub(super) fn insert(&mut self, key: u64, value: Arc<T>) {
        self.root = Some(insert(self.root.take(), key, value));
    }

    pub(super) fn iter(&self) -> Iter<'_, T> {
        Iter::new(&self.root, None)
    }

    pub(super) fn values(&self) -> impl Iterator<Item = &Arc<T>> {
        self.iter().map(|(_, value)| value)
    }

    pub(super) fn after(&self, after: u64) -> Iter<'_, T> {
        Iter::new(&self.root, Some(after))
    }

    /// Detach one prefix by copying only its boundary path, then transfer
    /// the retired values in order. Untouched suffix subtrees stay shared.
    pub(super) fn take_through(&mut self, through: u64) -> Vec<Arc<T>> {
        let mut first = self.root.as_deref();
        while let Some(node) = first {
            if let Some(left) = node.left.as_deref() {
                first = Some(left);
            } else if node.key > through {
                return Vec::new();
            } else {
                break;
            }
        }
        let (retired, retained) = split(self.root.take(), through);
        self.root = retained;
        let mut values = Vec::with_capacity(len(&retired));
        drain(retired, &mut values);
        values
    }
}

pub(super) struct Iter<'a, T> {
    // An AVL tree with distinct u64 keys has height below 94, even at the
    // entire key-domain bound. Suffix cursors need no heap allocation.
    path: [Option<&'a Node<T>>; 94],
    len: usize,
    remaining: usize,
}

impl<'a, T> Iter<'a, T> {
    fn new(root: &'a Link<T>, after: Option<u64>) -> Self {
        let mut iter = Self {
            path: [None; 94],
            len: 0,
            remaining: len(root),
        };
        let mut current = root.as_deref();
        while let Some(node) = current {
            if after.is_some_and(|after| node.key <= after) {
                iter.remaining -= 1 + len(&node.left);
                current = node.right.as_deref();
            } else {
                iter.push(node);
                current = node.left.as_deref();
            }
        }
        iter
    }

    fn push(&mut self, node: &'a Node<T>) {
        self.path[self.len] = Some(node);
        self.len += 1;
    }
}

impl<'a, T> Iterator for Iter<'a, T> {
    type Item = (&'a u64, &'a Arc<T>);

    fn next(&mut self) -> Option<Self::Item> {
        if self.len == 0 {
            return None;
        }
        self.remaining -= 1;
        self.len -= 1;
        let node = self.path[self.len]
            .take()
            .expect("iterator path entries below len are populated");
        let mut current = node.right.as_deref();
        while let Some(next) = current {
            self.push(next);
            current = next.left.as_deref();
        }
        Some((&node.key, &node.value))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};

    fn validate<T>(root: &Link<T>, lower: Option<u64>, upper: Option<u64>) -> (u8, usize) {
        let Some(node) = root else { return (0, 0) };
        assert!(lower.is_none_or(|lower| lower < node.key));
        assert!(upper.is_none_or(|upper| node.key < upper));
        let (left_height, left_len) = validate(&node.left, lower, Some(node.key));
        let (right_height, right_len) = validate(&node.right, Some(node.key), upper);
        assert!(left_height.abs_diff(right_height) <= 1);
        assert_eq!(node.height, 1 + left_height.max(right_height));
        assert_eq!(node.len, 1 + left_len + right_len);
        (node.height, node.len)
    }

    fn check_iter(mut actual: Iter<'_, u64>, expected: impl Iterator<Item = (u64, u64)> + Clone) {
        let mut remaining = expected.clone().count();
        assert_eq!(actual.size_hint(), (remaining, Some(remaining)));
        for entry in expected {
            assert_eq!(
                actual.next().map(|(&key, value)| (key, **value)),
                Some(entry)
            );
            remaining -= 1;
            assert_eq!(actual.size_hint(), (remaining, Some(remaining)));
        }
        assert!(actual.next().is_none());
        assert!(actual.next().is_none());
        assert_eq!(actual.size_hint(), (0, Some(0)));
    }

    fn check(actual: &Entries<u64>, expected: &BTreeMap<u64, u64>) {
        validate(&actual.root, None, None);
        assert_eq!(actual.len(), expected.len());
        assert_eq!(actual.is_empty(), expected.is_empty());
        check_iter(
            actual.iter(),
            expected.iter().map(|(&key, &value)| (key, value)),
        );
        assert_eq!(
            actual.values().size_hint(),
            (expected.len(), Some(expected.len()))
        );
        assert!(
            actual
                .values()
                .map(|value| **value)
                .eq(expected.values().copied())
        );
        for (&key, &value) in expected {
            assert!(actual.contains_key(&key));
            assert_eq!(actual.get(&key).map(|value| **value), Some(value));
        }
        for after in [0, 1, 7, 127, 1024, u64::MAX / 2, u64::MAX - 1, u64::MAX] {
            check_iter(
                actual.after(after),
                expected
                    .range((std::ops::Bound::Excluded(after), std::ops::Bound::Unbounded))
                    .map(|(&key, &value)| (key, value)),
            );
        }
    }

    fn retire_model(expected: &mut BTreeMap<u64, u64>, through: u64) -> Vec<u64> {
        let keys: Vec<_> = expected.range(..=through).map(|(&key, _)| key).collect();
        keys.into_iter()
            .map(|key| expected.remove(&key).unwrap())
            .collect()
    }

    #[test]
    fn sparse_and_full_width_keys_match_the_ordered_map_under_every_tree_shape() {
        for direction in 0..3 {
            let mut actual = Entries::new();
            let mut expected = BTreeMap::new();
            for step in 0..1024_u64 {
                let key = match direction {
                    0 => step,
                    1 => u64::MAX - step,
                    _ => step.wrapping_mul(0x9e37_79b9_7f4a_7c15),
                };
                actual.insert(key, Arc::new(step));
                expected.insert(key, step);
                if step % 31 == 0 {
                    check(&actual, &expected);
                }
            }
            check(&actual, &expected);
            assert_eq!(format!("{actual:?}"), format!("{expected:?}"));
            assert_eq!(format!("{actual:#?}"), format!("{expected:#?}"));
            // Equal maps may have different tree shapes. Replacement changes
            // one explicit key, never the node count or its ordering.
            let mut reversed = Entries::new();
            for (&key, &value) in expected.iter().rev() {
                reversed.insert(key, Arc::new(value));
            }
            assert_eq!(actual, reversed);
            actual.insert(0, Arc::new(9999));
            expected.insert(0, 9999);
            check(&actual, &expected);
            for through in [
                0,
                511,
                1023,
                u64::MAX / 3,
                u64::MAX / 2,
                u64::MAX - 1,
                u64::MAX,
            ] {
                assert_eq!(
                    actual
                        .take_through(through)
                        .into_iter()
                        .map(|value| *value)
                        .collect::<Vec<_>>(),
                    retire_model(&mut expected, through),
                );
                check(&actual, &expected);
            }
        }
    }

    #[test]
    fn branching_generations_and_prefix_retirement_match_independent_maps() {
        let mut state = 0xd371_a17e_5eed_u64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let mut versions = vec![(Entries::new(), BTreeMap::new())];
        for step in 0..2048 {
            let basis = next() as usize % versions.len();
            let (mut actual, mut expected) = versions[basis].clone();
            if step % 5 == 0 {
                let through = next();
                assert_eq!(
                    actual
                        .take_through(through)
                        .into_iter()
                        .map(|value| *value)
                        .collect::<Vec<_>>(),
                    retire_model(&mut expected, through),
                );
            } else {
                for _ in 0..16 {
                    let key = next();
                    let value = next();
                    actual.insert(key, Arc::new(value));
                    expected.insert(key, value);
                }
            }
            check(&actual, &expected);
            check(&versions[basis].0, &versions[basis].1);
            versions.push((actual, expected));
            if versions.len() > 64 {
                versions.remove(next() as usize % versions.len());
            }
        }
    }

    #[test]
    fn every_prefix_split_keeps_balanced_order_and_the_pinned_generation() {
        for n in [0, 1, 2, 3, 7, 8, 31, 32, 63, 64, 127, 128, 255, 256, 1023] {
            for reversed in [false, true] {
                let mut original = Entries::new();
                for at in 0..n {
                    let key = if reversed { n - at } else { at + 1 };
                    original.insert(key, Arc::new(key));
                }
                for through in 0..=n {
                    let mut selected = original.clone();
                    let retired = selected.take_through(through);
                    assert!(retired.iter().map(|value| **value).eq(1..=through));
                    assert!(selected.iter().map(|(&key, _)| key).eq(through + 1..=n));
                    assert_eq!(selected.len(), (n - through) as usize);
                    validate(&selected.root, None, None);
                    assert!(original.iter().map(|(&key, _)| key).eq(1..=n));
                }
            }
        }
    }

    fn addresses<T>(root: &Link<T>, output: &mut BTreeSet<usize>) {
        if let Some(node) = root {
            output.insert(Arc::as_ptr(node) as usize);
            addresses(&node.left, output);
            addresses(&node.right, output);
        }
    }

    #[test]
    fn pinning_is_constant_and_append_and_retirement_share_unaffected_subtrees() {
        for n in [127, 1023, 8191] {
            let mut live = Entries::new();
            for key in 0..n {
                live.insert(key, Arc::new(key));
            }
            let pinned = live.clone();
            assert!(Arc::ptr_eq(
                live.root.as_ref().unwrap(),
                pinned.root.as_ref().unwrap()
            ));
            let old_height = usize::from(height(&pinned.root));
            let mut old = BTreeSet::new();
            addresses(&pinned.root, &mut old);
            live.insert(n, Arc::new(n));
            let mut appended = BTreeSet::new();
            addresses(&live.root, &mut appended);
            assert!(appended.difference(&old).count() <= old_height + 3);
            assert_eq!(pinned.len(), n as usize);
            assert!(pinned.get(&n).is_none());
            let before_retirement = live.clone();
            let mut before = BTreeSet::new();
            addresses(&before_retirement.root, &mut before);
            let through = n / 2 - 3;
            let retired = live.take_through(through);
            assert_eq!(retired.len(), through as usize + 1);
            let mut retained = BTreeSet::new();
            addresses(&live.root, &mut retained);
            assert!(retained.difference(&before).count() <= 2 * old_height + 3);
            assert_eq!(live.len(), (n - through) as usize);
            validate(&live.root, None, None);
            for key in through + 1..=n {
                assert!(Arc::ptr_eq(
                    live.get(&key).unwrap(),
                    before_retirement.get(&key).unwrap()
                ));
            }
            let after_retirement = live.clone();
            assert!(live.take_through(through).is_empty());
            assert!(Arc::ptr_eq(
                live.root.as_ref().unwrap(),
                after_retirement.root.as_ref().unwrap()
            ));
        }
    }

    #[test]
    fn unique_retirement_moves_values_and_shared_retirement_keeps_pinned_owners() {
        let mut live = Entries::new();
        let mut payloads = Vec::new();
        for key in 0..128 {
            let value = vec![key; 16];
            payloads.push(value.as_ptr());
            live.insert(key, Arc::new(value));
        }
        let retired = live.take_through(31);
        for (key, value) in retired.into_iter().enumerate() {
            let value = Arc::unwrap_or_clone(value);
            assert_eq!(value.as_ptr(), payloads[key]);
        }
        let pinned = live.clone();
        for (offset, value) in live.take_through(63).into_iter().enumerate() {
            assert!(Arc::ptr_eq(
                &value,
                pinned.get(&(offset as u64 + 32)).unwrap()
            ));
            let owned = Arc::unwrap_or_clone(value);
            assert_ne!(owned.as_ptr(), payloads[offset + 32]);
        }
        assert_eq!(pinned.len(), 96);
        assert_eq!(live.len(), 64);
    }

    #[test]
    fn sharing_does_not_override_value_partial_equality() {
        let value = Arc::new(f64::NAN);
        let mut actual = Entries::new();
        actual.insert(1, Arc::clone(&value));
        let expected = BTreeMap::from([(1, value)]);
        assert_eq!(actual == actual.clone(), expected == expected.clone());
    }
}
