//! Bounded, canonical schema unions shared by preparation and graph sources.

/// Maximum distinct labels or relation types in one induced graph selection.
pub const MAX_SCHEMA_SELECTION_IDS: usize = 16;

/// A schema selector narrows an already admitted graph; it grants no authority.
/// All (including an empty input list) preserves every admitted identity.
/// Nonempty selectors are sorted sets. Their inline representation needs no
/// allocation and has one canonical value regardless of input order/repeats.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SchemaSelection<T> {
    selected: Option<Members<T>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Members<T> {
    ids: [T; MAX_SCHEMA_SELECTION_IDS],
    len: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SchemaSelectionLimit;

impl core::fmt::Display for SchemaSelectionLimit {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "schema selection exceeds {MAX_SCHEMA_SELECTION_IDS} distinct identities"
        )
    }
}
impl core::error::Error for SchemaSelectionLimit {}

impl<T> SchemaSelection<T> {
    pub const ALL: Self = Self { selected: None };

    pub const fn is_all(&self) -> bool {
        self.selected.is_none()
    }

    pub fn members(&self) -> Option<&[T]> {
        self.selected
            .as_ref()
            .map(|members| &members.ids[..usize::from(members.len)])
    }
}

impl<T: Copy> SchemaSelection<T> {
    pub const fn one(id: T) -> Self {
        Self {
            selected: Some(Members {
                ids: [id; MAX_SCHEMA_SELECTION_IDS],
                len: 1,
            }),
        }
    }

    pub fn single(&self) -> Option<T> {
        self.members().and_then(|members| match members {
            [id] => Some(*id),
            _ => None,
        })
    }
}

impl<T: Copy + Ord> SchemaSelection<T> {
    /// Deduplicate before applying the bound. The unused inline suffix is
    /// filled with the smallest member, so equality is semantic equality.
    pub fn any_of(ids: &[T]) -> Result<Self, SchemaSelectionLimit> {
        let Some(&first) = ids.first() else {
            return Ok(Self::ALL);
        };
        let mut values = [first; MAX_SCHEMA_SELECTION_IDS];
        let mut len = 0usize;
        for &id in ids {
            match values[..len].binary_search(&id) {
                Ok(_) => {}
                Err(at) => {
                    if len == MAX_SCHEMA_SELECTION_IDS {
                        return Err(SchemaSelectionLimit);
                    }
                    values.copy_within(at..len, at + 1);
                    values[at] = id;
                    len += 1;
                }
            }
        }
        let smallest = values[0];
        values[len..].fill(smallest);
        Ok(Self {
            selected: Some(Members {
                ids: values,
                len: len as u8,
            }),
        })
    }

    pub fn contains(&self, id: T) -> bool {
        self.members()
            .is_none_or(|members| members.binary_search(&id).is_ok())
    }

    /// ANY selected label may admit a vertex; it need not carry every label.
    pub fn intersects_sorted(&self, ids: &[T]) -> bool {
        self.members()
            .is_none_or(|members| members.iter().any(|id| ids.binary_search(id).is_ok()))
    }
}

impl<T: Copy> From<Option<T>> for SchemaSelection<T> {
    fn from(value: Option<T>) -> Self {
        value.map_or(Self::ALL, Self::one)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_unions_preserve_all_singleton_full_width_and_any_semantics() {
        let a = SchemaSelection::any_of(&[u64::MAX, 2, 1, 2]).unwrap();
        let b = SchemaSelection::any_of(&[1, 2, u64::MAX]).unwrap();
        assert_eq!(a, b);
        assert_eq!(a.members(), Some([1, 2, u64::MAX].as_slice()));
        assert!(a.intersects_sorted(&[2, 7]));
        assert!(!a.intersects_sorted(&[3, 7]));
        assert!(!a.intersects_sorted(&[]));
        assert!(SchemaSelection::<u64>::ALL.intersects_sorted(&[]));
        assert_eq!(
            SchemaSelection::<u64>::any_of(&[]),
            Ok(SchemaSelection::ALL)
        );
        assert_eq!(
            SchemaSelection::any_of(&[7, 7]),
            Ok(SchemaSelection::one(7))
        );
        assert_eq!(a.single(), None);
        assert_eq!(
            SchemaSelection::one(u64::MAX).single(),
            Some(u64::MAX)
        );
    }

    #[test]
    fn capacity_is_a_distinct_identity_bound_and_never_truncates() {
        let at: Vec<_> = (0..MAX_SCHEMA_SELECTION_IDS as u64).collect();
        let repeated: Vec<_> = at.iter().rev().chain(at.iter()).copied().collect();
        assert_eq!(
            SchemaSelection::any_of(&at),
            SchemaSelection::any_of(&repeated)
        );
        let over: Vec<_> = (0..=MAX_SCHEMA_SELECTION_IDS as u64).collect();
        assert_eq!(SchemaSelection::any_of(&over), Err(SchemaSelectionLimit));
    }
}
