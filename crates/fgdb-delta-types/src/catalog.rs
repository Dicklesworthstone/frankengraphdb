//! Monotone symbol names within one graph/branch schema coordinate.
//!
//! This is the name-binding facet of the schema catalog, not a schema-gradient
//! implementation or a persistence authority. Its owner must publish it through
//! the ordinary transactional schema/root path. In particular, a bindings file
//! or an independently mutable sidecar cannot make this state durable.
//!
//! Names are exact UTF-8 strings. Identifier parsing and the pinned language's
//! normalization rules belong to the binder; this layer never consults a host
//! locale or changes a supplied spelling. Ordinals are the full native u64
//! domains, independently interned for labels, relations and property keys.

use crate::{LabelId, PropertyKeyId, RelationId};
use std::collections::BTreeMap;
use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CatalogSymbolKind {
    Label,
    Relation,
    Property,
}

impl CatalogSymbolKind {
    const fn index(self) -> usize {
        match self {
            Self::Label => 0,
            Self::Relation => 1,
            Self::Property => 2,
        }
    }

    const fn symbol(self, ordinal: u64) -> CatalogSymbol {
        match self {
            Self::Label => CatalogSymbol::Label(LabelId(ordinal)),
            Self::Relation => CatalogSymbol::Relation(RelationId(ordinal)),
            Self::Property => CatalogSymbol::Property(PropertyKeyId(ordinal)),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CatalogSymbol {
    Label(LabelId),
    Relation(RelationId),
    Property(PropertyKeyId),
}

impl CatalogSymbol {
    #[must_use]
    pub const fn kind(self) -> CatalogSymbolKind {
        match self {
            Self::Label(_) => CatalogSymbolKind::Label,
            Self::Relation(_) => CatalogSymbolKind::Relation,
            Self::Property(_) => CatalogSymbolKind::Property,
        }
    }

    #[must_use]
    pub const fn ordinal(self) -> u64 {
        match self {
            Self::Label(LabelId(ordinal))
            | Self::Relation(RelationId(ordinal))
            | Self::Property(PropertyKeyId(ordinal)) => ordinal,
        }
    }
}

/// A conflict identifies its namespace and axis without disclosing another
/// binding's name or ordinal. Authorized diagnostics may separately inspect
/// the owner's catalog; this error is safe to retain in a structural report.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CatalogBindingError {
    EmptyName,
    NameConflict { kind: CatalogSymbolKind },
    OrdinalConflict { kind: CatalogSymbolKind },
    OrdinalsExhausted { kind: CatalogSymbolKind },
    TooManyBindings,
}

impl core::fmt::Display for CatalogBindingError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::EmptyName => f.write_str("schema symbol name is empty"),
            Self::NameConflict { kind } => {
                write!(
                    f,
                    "schema binding conflict: {kind:?} name already has another ordinal"
                )
            }
            Self::OrdinalConflict { kind } => {
                write!(
                    f,
                    "schema binding conflict: {kind:?} ordinal already has another name"
                )
            }
            Self::OrdinalsExhausted { kind } => {
                write!(f, "schema {kind:?} automatic ordinals are exhausted")
            }
            Self::TooManyBindings => f.write_str("schema binding count cannot be represented"),
        }
    }
}

impl core::error::Error for CatalogBindingError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CatalogBindingChange {
    Existing,
    Added,
}

type Names = BTreeMap<Arc<str>, CatalogSymbol>;

/// An append-only bijection in each of the three symbol namespaces.
///
/// There is deliberately no remove/rebind API. A new schema coordinate may
/// clone a predecessor, but cannot change the meaning of an existing ordinal.
/// Clones share name bytes, never mutable map entries. This host-owned value
/// is not an authorization projection: callers must mask catalog observation
/// before handing a resolver or introspection result to an untrusted client.
#[derive(Clone, PartialEq, Eq)]
pub struct SymbolCatalog {
    labels: Names,
    relations: Names,
    properties: Names,
    reverse: BTreeMap<CatalogSymbol, Arc<str>>,
    next: [Option<u64>; 3],
    count: usize,
}

impl Default for SymbolCatalog {
    fn default() -> Self {
        Self {
            labels: Names::new(),
            relations: Names::new(),
            properties: Names::new(),
            reverse: BTreeMap::new(),
            // Explicit ordinal zero stays readable, but automatic allocation
            // starts at one and never fills gaps beneath its high-water mark.
            next: [Some(1); 3],
            count: 0,
        }
    }
}

impl core::fmt::Debug for SymbolCatalog {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SymbolCatalog")
            .field("labels", &self.labels.len())
            .field("relations", &self.relations.len())
            .field("properties", &self.properties.len())
            .finish_non_exhaustive()
    }
}

impl SymbolCatalog {
    #[must_use]
    pub const fn len(&self) -> usize {
        self.count
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.count == 0
    }

    fn names(&self, kind: CatalogSymbolKind) -> &Names {
        match kind {
            CatalogSymbolKind::Label => &self.labels,
            CatalogSymbolKind::Relation => &self.relations,
            CatalogSymbolKind::Property => &self.properties,
        }
    }

    fn names_mut(&mut self, kind: CatalogSymbolKind) -> &mut Names {
        match kind {
            CatalogSymbolKind::Label => &mut self.labels,
            CatalogSymbolKind::Relation => &mut self.relations,
            CatalogSymbolKind::Property => &mut self.properties,
        }
    }

    #[must_use]
    pub fn resolve(&self, kind: CatalogSymbolKind, name: &str) -> Option<CatalogSymbol> {
        self.names(kind).get(name).copied()
    }

    #[must_use]
    pub fn name(&self, symbol: CatalogSymbol) -> Option<&str> {
        self.reverse.get(&symbol).map(AsRef::as_ref)
    }

    /// Canonical namespace-then-name order, independent of insertion order.
    pub fn bindings(&self) -> impl Iterator<Item = (CatalogSymbol, &str)> {
        self.labels
            .iter()
            .chain(&self.relations)
            .chain(&self.properties)
            .map(|(name, symbol)| (*symbol, name.as_ref()))
    }

    /// Admit an explicit binding. Every typed refusal precedes mutation;
    /// redeclaring the exact pair does not allocate or advance an ordinal.
    pub fn bind(
        &mut self,
        name: &str,
        symbol: CatalogSymbol,
    ) -> Result<CatalogBindingChange, CatalogBindingError> {
        if name.is_empty() {
            return Err(CatalogBindingError::EmptyName);
        }
        let kind = symbol.kind();
        if let Some(existing) = self.resolve(kind, name) {
            return if existing == symbol {
                Ok(CatalogBindingChange::Existing)
            } else {
                Err(CatalogBindingError::NameConflict { kind })
            };
        }
        if self.reverse.contains_key(&symbol) {
            return Err(CatalogBindingError::OrdinalConflict { kind });
        }
        let count = self
            .count
            .checked_add(1)
            .ok_or(CatalogBindingError::TooManyBindings)?;
        let name: Arc<str> = Arc::from(name);
        self.names_mut(kind).insert(name.clone(), symbol);
        self.reverse.insert(symbol, name);
        self.reserve_through(symbol);
        self.count = count;
        Ok(CatalogBindingChange::Added)
    }

    /// Account for an ordinal already used or issued by this coordinate,
    /// including data whose symbol name has not yet been adopted. This does
    /// not invent a name or forbid explicitly naming that existing ordinal.
    /// A catalog owner must retain these high-water marks in its state and
    /// seed them from existing data before enabling automatic allocation.
    pub fn reserve_through(&mut self, symbol: CatalogSymbol) {
        let next = &mut self.next[symbol.kind().index()];
        if next.is_some_and(|next| symbol.ordinal() >= next) {
            *next = symbol.ordinal().checked_add(1);
        }
    }

    /// Highest ordinal reserved or bound in this namespace. Zero denotes
    /// the initial automatic-allocation floor as well as an explicit zero.
    #[must_use]
    pub fn high_water_mark(&self, kind: CatalogSymbolKind) -> u64 {
        self.next[kind.index()].map_or(u64::MAX, |next| next - 1)
    }

    /// Validate a complete proposed extension off-side. A conflict anywhere
    /// leaves both maps and all allocator high-water marks unchanged.
    /// Returns the number of genuinely new bindings, excluding redeclarations.
    pub fn bind_all<S: AsRef<str>>(
        &mut self,
        bindings: impl IntoIterator<Item = (S, CatalogSymbol)>,
    ) -> Result<usize, CatalogBindingError> {
        let mut next = self.clone();
        for (name, symbol) in bindings {
            next.bind(name.as_ref(), symbol)?;
        }
        let added = next.len() - self.len();
        *self = next;
        Ok(added)
    }

    /// Resolve an existing name or allocate the next unspent automatic
    /// ordinal in that namespace. Exhaustion never wraps or searches gaps.
    pub fn intern(
        &mut self,
        kind: CatalogSymbolKind,
        name: &str,
    ) -> Result<CatalogSymbol, CatalogBindingError> {
        if name.is_empty() {
            return Err(CatalogBindingError::EmptyName);
        }
        if let Some(existing) = self.resolve(kind, name) {
            return Ok(existing);
        }
        let ordinal =
            self.next[kind.index()].ok_or(CatalogBindingError::OrdinalsExhausted { kind })?;
        let symbol = kind.symbol(ordinal);
        self.bind(name, symbol)?;
        Ok(symbol)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_ordinals_and_namespaces_remain_distinct_and_bidirectional() {
        let mut catalog = SymbolCatalog::default();
        let ordinal = u64::from(u32::MAX) + 19;
        let expected = [
            ("Person", CatalogSymbol::Label(LabelId(ordinal))),
            ("Person", CatalogSymbol::Relation(RelationId(ordinal))),
            ("Person", CatalogSymbol::Property(PropertyKeyId(ordinal))),
        ];
        assert_eq!(catalog.bind_all(expected).unwrap(), 3);
        for (name, symbol) in expected {
            assert_eq!(catalog.resolve(symbol.kind(), name), Some(symbol));
            assert_eq!(catalog.name(symbol), Some(name));
        }
        assert_eq!(catalog.bind_all(expected).unwrap(), 0);
        assert_eq!(catalog.len(), 3);
        assert!(!catalog.is_empty());
        assert_eq!(
            catalog.intern(CatalogSymbolKind::Property, "next").unwrap(),
            CatalogSymbol::Property(PropertyKeyId(ordinal + 1))
        );
        assert_eq!(catalog.name(CatalogSymbol::Label(LabelId(0))), None);
        assert_eq!(catalog.resolve(CatalogSymbolKind::Label, "person"), None);
    }

    #[test]
    fn conflicts_and_failed_extensions_preserve_the_complete_predecessor() {
        let mut catalog = SymbolCatalog::default();
        catalog
            .bind("Person", CatalogSymbol::Label(LabelId(7)))
            .unwrap();
        let before = catalog.clone();
        for (name, symbol, expected) in [
            (
                "Person",
                CatalogSymbol::Label(LabelId(8)),
                CatalogBindingError::NameConflict {
                    kind: CatalogSymbolKind::Label,
                },
            ),
            (
                "Other",
                CatalogSymbol::Label(LabelId(7)),
                CatalogBindingError::OrdinalConflict {
                    kind: CatalogSymbolKind::Label,
                },
            ),
            (
                "",
                CatalogSymbol::Property(PropertyKeyId(1)),
                CatalogBindingError::EmptyName,
            ),
        ] {
            assert_eq!(catalog.bind(name, symbol), Err(expected));
            assert_eq!(catalog, before);
        }
        assert_eq!(
            catalog.bind_all([
                ("future", CatalogSymbol::Property(PropertyKeyId(u64::MAX))),
                ("first", CatalogSymbol::Label(LabelId(99))),
                ("Person", CatalogSymbol::Label(LabelId(100))),
            ]),
            Err(CatalogBindingError::NameConflict {
                kind: CatalogSymbolKind::Label
            })
        );
        assert_eq!(catalog, before);
        assert_eq!(
            catalog.intern(CatalogSymbolKind::Label, "next").unwrap(),
            CatalogSymbol::Label(LabelId(8))
        );
        assert_eq!(
            catalog.intern(CatalogSymbolKind::Property, "next").unwrap(),
            CatalogSymbol::Property(PropertyKeyId(1))
        );
    }

    #[test]
    fn canonical_iteration_and_clone_isolation_do_not_depend_on_binding_order() {
        let bindings = [
            ("z", CatalogSymbol::Property(PropertyKeyId(8))),
            ("é", CatalogSymbol::Label(LabelId(3))),
            ("a", CatalogSymbol::Label(LabelId(2))),
            ("R", CatalogSymbol::Relation(RelationId(7))),
        ];
        let mut first = SymbolCatalog::default();
        first.bind_all(bindings).unwrap();
        let mut second = SymbolCatalog::default();
        second.bind_all(bindings.into_iter().rev()).unwrap();
        assert_eq!(first, second);
        assert_eq!(
            first.bindings().collect::<Vec<_>>(),
            vec![bindings[2], bindings[1], bindings[3], bindings[0]]
                .into_iter()
                .map(|(name, symbol)| (symbol, name))
                .collect::<Vec<_>>()
        );
        second.intern(CatalogSymbolKind::Label, "later").unwrap();
        assert_eq!(first.len(), 4);
        assert_eq!(first.resolve(CatalogSymbolKind::Label, "later"), None);
        assert_eq!(first.bindings().count(), first.len());
        let debug = format!("{first:?}");
        assert!(!debug.contains('é'));
    }

    #[test]
    fn exhausted_namespace_still_resolves_old_names_and_never_wraps_or_fills_gaps() {
        let mut catalog = SymbolCatalog::default();
        catalog
            .bind("last", CatalogSymbol::Label(LabelId(u64::MAX)))
            .unwrap();
        catalog
            .bind("zero", CatalogSymbol::Label(LabelId(0)))
            .unwrap();
        let before = catalog.clone();
        assert_eq!(
            catalog.intern(CatalogSymbolKind::Label, "new"),
            Err(CatalogBindingError::OrdinalsExhausted {
                kind: CatalogSymbolKind::Label
            })
        );
        assert_eq!(catalog, before);
        assert_eq!(
            catalog.intern(CatalogSymbolKind::Label, "last").unwrap(),
            CatalogSymbol::Label(LabelId(u64::MAX))
        );
        assert_eq!(
            catalog.bind("last", CatalogSymbol::Label(LabelId(u64::MAX))),
            Ok(CatalogBindingChange::Existing)
        );
        assert_eq!(
            catalog.intern(CatalogSymbolKind::Relation, "R").unwrap(),
            CatalogSymbol::Relation(RelationId(1))
        );
        assert_eq!(
            catalog.intern(CatalogSymbolKind::Label, ""),
            Err(CatalogBindingError::EmptyName)
        );
    }

    #[test]
    fn adopting_unnamed_data_never_reissues_its_reserved_ordinals() {
        let mut catalog = SymbolCatalog::default();
        catalog.reserve_through(CatalogSymbol::Label(LabelId(40)));
        catalog.reserve_through(CatalogSymbol::Property(PropertyKeyId(u64::MAX)));
        assert!(catalog.is_empty());
        assert_eq!(catalog.high_water_mark(CatalogSymbolKind::Label), 40);
        assert_eq!(
            catalog.high_water_mark(CatalogSymbolKind::Property),
            u64::MAX
        );
        // Adopting a known old name is not a new identity allocation.
        catalog
            .bind("old", CatalogSymbol::Label(LabelId(7)))
            .unwrap();
        assert_eq!(
            catalog.intern(CatalogSymbolKind::Label, "new").unwrap(),
            CatalogSymbol::Label(LabelId(41))
        );
        catalog.reserve_through(CatalogSymbol::Label(LabelId(9)));
        assert_eq!(catalog.high_water_mark(CatalogSymbolKind::Label), 41);
        assert_eq!(
            catalog.intern(CatalogSymbolKind::Property, "new"),
            Err(CatalogBindingError::OrdinalsExhausted {
                kind: CatalogSymbolKind::Property
            })
        );
        assert_eq!(
            catalog.intern(CatalogSymbolKind::Relation, "R").unwrap(),
            CatalogSymbol::Relation(RelationId(1))
        );
    }

    #[test]
    fn binding_sequences_agree_with_an_independent_linear_bijection_model() {
        for seed in 0_u64..32 {
            let mut catalog = SymbolCatalog::default();
            // A deliberately linear list of accepted declarations, not the
            // production forward/reverse trees or their helper methods.
            let mut model: Vec<(CatalogSymbolKind, String, CatalogSymbol)> = Vec::new();
            let mut high_water = [0_u64; 3];
            let mut state = seed;
            for _ in 0..128 {
                state = state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1);
                let namespace = ((state >> 16) % 3) as usize;
                let ordinal = match (state >> 24) % 8 {
                    0 => u64::MAX,
                    1 => u64::from(u32::MAX) + 7,
                    value => value - 2,
                };
                let (kind, symbol) = match namespace {
                    0 => (
                        CatalogSymbolKind::Label,
                        CatalogSymbol::Label(LabelId(ordinal)),
                    ),
                    1 => (
                        CatalogSymbolKind::Relation,
                        CatalogSymbol::Relation(RelationId(ordinal)),
                    ),
                    _ => (
                        CatalogSymbolKind::Property,
                        CatalogSymbol::Property(PropertyKeyId(ordinal)),
                    ),
                };
                let name = format!("name_{}", (state >> 32) % 16);
                let expected = if let Some((_, _, old)) =
                    model.iter().find(|(k, n, _)| *k == kind && *n == name)
                {
                    if *old == symbol {
                        Ok(CatalogBindingChange::Existing)
                    } else {
                        Err(CatalogBindingError::NameConflict { kind })
                    }
                } else if model.iter().any(|(_, _, old)| *old == symbol) {
                    Err(CatalogBindingError::OrdinalConflict { kind })
                } else {
                    model.push((kind, name.clone(), symbol));
                    high_water[namespace] = high_water[namespace].max(ordinal);
                    Ok(CatalogBindingChange::Added)
                };
                assert_eq!(catalog.bind(&name, symbol), expected, "seed={seed}");
                assert_eq!(catalog.len(), model.len(), "seed={seed}");
                for (at, kind) in [
                    CatalogSymbolKind::Label,
                    CatalogSymbolKind::Relation,
                    CatalogSymbolKind::Property,
                ]
                .into_iter()
                .enumerate()
                {
                    assert_eq!(catalog.high_water_mark(kind), high_water[at]);
                    for key in 0..16 {
                        let name = format!("name_{key}");
                        let expected = model
                            .iter()
                            .find(|(k, n, _)| *k == kind && *n == name)
                            .map(|(_, _, value)| *value);
                        assert_eq!(catalog.resolve(kind, &name), expected, "seed={seed}");
                    }
                }
                for (_, name, symbol) in &model {
                    assert_eq!(catalog.name(*symbol), Some(name.as_str()), "seed={seed}");
                }
                let mut expected = model
                    .iter()
                    .map(|(kind, name, symbol)| (*kind, name.as_str(), *symbol))
                    .collect::<Vec<_>>();
                expected.sort_by(|left, right| (left.0, left.1).cmp(&(right.0, right.1)));
                assert_eq!(
                    catalog.bindings().collect::<Vec<_>>(),
                    expected
                        .into_iter()
                        .map(|(_, name, symbol)| (symbol, name))
                        .collect::<Vec<_>>()
                );
            }
        }
    }
}
