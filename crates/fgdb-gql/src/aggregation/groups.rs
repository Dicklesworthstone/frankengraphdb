//! Groups under construction for the ordinary grouped visitor.
//!
//! A tree keyed by borrowed values compares through two pointers per step:
//! the key's buffer, then the property payload it borrows. Building 264K
//! `a.id` groups that way cost about 1.3 µs per group, more than the edge
//! scan feeding them (road-ny top-10 out-degree: 620 ms against 243 ms when
//! grouped by the vertex itself). Keys of at most two nulls, booleans,
//! integers or identities are found by hash instead and sorted once into the
//! same tree. Their order and equality are exactly `ValueRef`'s (pinned
//! below), and callers charge the same events in the same order whichever
//! table holds a group, so the finished map is the map the tree alone would
//! have built. The first key of any other kind moves every group into the
//! tree, which then builds the rest.

use super::*;
use crate::id_hash::IdMap;

/// Flat keys hold this many columns; wider keys use the tree throughout.
const FLAT_COLUMNS: usize = 2;

/// One key value whose order and equality are those of a plain value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(super) enum Flat {
    // Declaration order is `ValueRef`'s: scalars by type rank (null, boolean,
    // integer), then vertices, then edges.
    Null,
    Bool(bool),
    Int(i64),
    Vertex(VId),
    Edge(EId),
}

impl Flat {
    fn of(value: ValueRef<'_>) -> Option<Self> {
        Some(match value {
            ValueRef::Scalar(CanonicalScalar::Null) => Self::Null,
            ValueRef::Scalar(CanonicalScalar::Bool(value)) => Self::Bool(*value),
            ValueRef::Scalar(CanonicalScalar::Int(value)) => Self::Int(*value),
            ValueRef::Vertex(vertex) => Self::Vertex(vertex),
            ValueRef::Edge(edge) => Self::Edge(edge),
            _ => return None,
        })
    }
}

/// Unused columns stay null; every key of one table has the same width.
pub(super) type FlatKey = [Flat; FLAT_COLUMNS];

fn flat_key(key: &[ValueRef<'_>]) -> Option<FlatKey> {
    if key.len() > FLAT_COLUMNS {
        return None;
    }
    let mut flat = [Flat::Null; FLAT_COLUMNS];
    for (column, value) in flat.iter_mut().zip(key) {
        *column = Flat::of(*value)?;
    }
    Some(flat)
}

type Group<'a> = (Vec<ValueRef<'a>>, Vec<Accumulator<'a>>);
type Tree<'a> = BTreeMap<Vec<ValueRef<'a>>, Vec<Accumulator<'a>>>;

pub(super) enum GroupTable<'a> {
    /// Probed, never iterated before its keys are sorted.
    Flat(IdMap<FlatKey, Group<'a>>),
    Tree(Tree<'a>),
}

impl<'a> GroupTable<'a> {
    /// A table for keys of `columns` values. A key-less aggregate's single
    /// group gains nothing from hashing.
    pub(super) fn new(columns: usize) -> Self {
        if (1..=FLAT_COLUMNS).contains(&columns) {
            Self::Flat(IdMap::default())
        } else {
            Self::Tree(BTreeMap::new())
        }
    }

    /// The group of `key`, if one exists. A key that is not flat first moves
    /// every group into the tree.
    pub(super) fn get_mut(&mut self, key: &[ValueRef<'a>]) -> Option<&mut Vec<Accumulator<'a>>> {
        let flat = self.flat_key(key);
        match self {
            Self::Flat(groups) => groups
                .get_mut(&flat.expect("a flat table is probed by flat keys"))
                .map(|(_, state)| state),
            Self::Tree(groups) => groups.get_mut(key),
        }
    }

    /// Adds `key`'s group unless it exists, as `entry(key).or_insert(state)`.
    pub(super) fn insert(
        &mut self,
        key: Vec<ValueRef<'a>>,
        state: Vec<Accumulator<'a>>,
    ) -> &mut Vec<Accumulator<'a>> {
        let flat = self.flat_key(&key);
        match self {
            Self::Flat(groups) => {
                &mut groups
                    .entry(flat.expect("a flat table holds flat keys"))
                    .or_insert((key, state))
                    .1
            }
            Self::Tree(groups) => groups.entry(key).or_insert(state),
        }
    }

    /// The tree of every group, in canonical key order.
    pub(super) fn into_tree(self) -> Tree<'a> {
        match self {
            Self::Flat(groups) => {
                let mut groups: Vec<_> = groups.into_iter().collect();
                // Flat order is the tree's order, compared without chasing a
                // pointer; the tree then bulk-builds from one ascending run.
                groups.sort_unstable_by_key(|(flat, _)| *flat);
                groups.into_iter().map(|(_, group)| group).collect()
            }
            Self::Tree(groups) => groups,
        }
    }

    /// `key` as a flat key while this table is flat; otherwise moves every
    /// group into the tree and answers `None`.
    fn flat_key(&mut self, key: &[ValueRef<'a>]) -> Option<FlatKey> {
        let Self::Flat(groups) = self else {
            return None;
        };
        let flat = flat_key(key);
        if flat.is_none() {
            let tree = core::mem::take(groups).into_values().collect();
            *self = Self::Tree(tree);
        }
        flat
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::cmp::Ordering;
    use fgdb_types::CanonicalF64;

    fn flat_values(scalars: &[CanonicalScalar]) -> Vec<ValueRef<'_>> {
        let mut values: Vec<_> = scalars.iter().map(ValueRef::Scalar).collect();
        values.extend([VId(0), VId(1), VId(u128::MAX)].map(ValueRef::Vertex));
        values.extend([EId(0), EId(7), EId(u128::MAX)].map(ValueRef::Edge));
        values
    }

    #[test]
    fn flat_keys_order_and_equate_exactly_as_borrowed_values() {
        let scalars = [
            CanonicalScalar::Null,
            CanonicalScalar::Bool(false),
            CanonicalScalar::Bool(true),
            CanonicalScalar::Int(i64::MIN),
            CanonicalScalar::Int(-1),
            CanonicalScalar::Int(0),
            CanonicalScalar::Int(1),
            CanonicalScalar::Int(i64::MAX),
        ];
        let values = flat_values(&scalars);
        for a in &values {
            for b in &values {
                let (flat_a, flat_b) = (Flat::of(*a).unwrap(), Flat::of(*b).unwrap());
                assert_eq!(flat_a.cmp(&flat_b), a.cmp(b), "{flat_a:?} {flat_b:?}");
                assert_eq!(flat_a == flat_b, a.cmp(b) == Ordering::Equal);
            }
        }
        // Keys compare column by column, as the tree's key vectors do.
        for width in 1..=FLAT_COLUMNS {
            let keys: Vec<Vec<ValueRef<'_>>> = values
                .iter()
                .flat_map(|first| {
                    values
                        .iter()
                        .step_by(3)
                        .map(move |second| [*first, *second][..width].to_vec())
                })
                .collect();
            for a in &keys {
                for b in &keys {
                    let (flat_a, flat_b) = (flat_key(a).unwrap(), flat_key(b).unwrap());
                    assert_eq!(flat_a.cmp(&flat_b), a.cmp(b));
                    assert_eq!(flat_a == flat_b, a == b);
                }
            }
        }
        // Values whose equality is not plain, and wider keys, stay in the tree.
        let text = CanonicalScalar::ucs_basic_text("a").unwrap();
        let float = CanonicalScalar::Float(CanonicalF64::new(1.0));
        let bytes = CanonicalScalar::bytes(vec![1]).unwrap();
        for scalar in [&text, &float, &bytes] {
            assert!(Flat::of(ValueRef::Scalar(scalar)).is_none());
        }
        let wide = [ValueRef::Vertex(VId(1)); FLAT_COLUMNS + 1];
        assert!(flat_key(&wide).is_none());
    }

    fn count(state: &[Accumulator<'_>]) -> u64 {
        match state {
            [Accumulator::Count(count)] => *count,
            _ => panic!("one count per group"),
        }
    }

    #[test]
    fn tables_build_the_trees_their_probes_would_have_built() {
        let scalars = [
            CanonicalScalar::Null,
            CanonicalScalar::Bool(true),
            CanonicalScalar::Int(-4),
            CanonicalScalar::Int(9),
            CanonicalScalar::ucs_basic_text("t").unwrap(),
        ];
        let mut values = flat_values(&scalars);
        let text = values.remove(scalars.len() - 1);
        // A deterministic mix of repeats and new keys of one and two columns,
        // with and without a later non-flat key that moves every group.
        for width in 1..=FLAT_COLUMNS {
            for spill in [None, Some(17), Some(0)] {
                let mut table = GroupTable::new(width);
                let mut tree = Tree::new();
                let mut seed = 0x2545_f491_4f6c_dd1d_u64;
                for step in 0..200 {
                    seed ^= seed << 13;
                    seed ^= seed >> 7;
                    seed ^= seed << 17;
                    let mut key: Vec<_> = (0..width)
                        .map(|column| values[(seed >> (8 * column)) as usize % values.len()])
                        .collect();
                    if spill == Some(step) {
                        key[0] = text;
                    }
                    let probes: [&mut dyn Probe<'_>; 2] = [&mut table, &mut tree];
                    for groups in probes {
                        match groups.probe(&key) {
                            Some(state) => {
                                let [Accumulator::Count(count)] = state.as_mut_slice() else {
                                    unreachable!()
                                };
                                *count += 1;
                            }
                            None => {
                                groups.add(key.clone(), vec![Accumulator::Count(1)]);
                            }
                        }
                    }
                }
                assert_eq!(matches!(table, GroupTable::Tree(_)), spill.is_some());
                let built = table.into_tree();
                assert!(built.keys().eq(tree.keys()), "{width} {spill:?}");
                assert!(
                    built
                        .values()
                        .zip(tree.values())
                        .all(|(a, b)| count(a) == count(b))
                );
            }
        }
    }

    /// The probe protocol of `execute_governed`'s grouped visitor.
    trait Probe<'a> {
        fn probe(&mut self, key: &[ValueRef<'a>]) -> Option<&mut Vec<Accumulator<'a>>>;
        fn add(&mut self, key: Vec<ValueRef<'a>>, state: Vec<Accumulator<'a>>);
    }
    impl<'a> Probe<'a> for GroupTable<'a> {
        fn probe(&mut self, key: &[ValueRef<'a>]) -> Option<&mut Vec<Accumulator<'a>>> {
            self.get_mut(key)
        }
        fn add(&mut self, key: Vec<ValueRef<'a>>, state: Vec<Accumulator<'a>>) {
            self.insert(key, state);
        }
    }
    impl<'a> Probe<'a> for Tree<'a> {
        fn probe(&mut self, key: &[ValueRef<'a>]) -> Option<&mut Vec<Accumulator<'a>>> {
            self.get_mut(key)
        }
        fn add(&mut self, key: Vec<ValueRef<'a>>, state: Vec<Accumulator<'a>>) {
            self.entry(key).or_insert(state);
        }
    }
}
