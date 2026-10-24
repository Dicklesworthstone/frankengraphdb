//! The operator-declared name bindings a served database resolves through.
//!
//! The engine has no durable catalog yet (`fgdb-w4-schema-catalog-ea4e`), so a
//! label, relation or property name means exactly what the operator bound it
//! to when the database was served, the same contract as the CLI's
//! `--label/--relation/--property` flags. Request text can never introduce or
//! rebind a name: an unbound name is an ordinary statement refusal.

use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::{GraphSymbol, GraphSymbolKind, GraphSymbolResolver, ReverseSymbolCatalog};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Symbols {
    labels: BTreeMap<String, u32>,
    relations: BTreeMap<String, u32>,
    properties: BTreeMap<String, u32>,
}

/// A second binding of one name, or one id bound under two names, in a kind.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SymbolConflict {
    pub kind: GraphSymbolKind,
    pub name: String,
}

impl core::fmt::Display for SymbolConflict {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let kind = match self.kind {
            GraphSymbolKind::Label => "label",
            GraphSymbolKind::Relation => "relation",
            GraphSymbolKind::Property => "property",
        };
        write!(f, "conflicting {kind} binding for {:?}", self.name)
    }
}
impl core::error::Error for SymbolConflict {}

impl Symbols {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Bind `name` to `id` in `kind`. Names and ids are each unique per kind,
    /// so both directions of the mapping are total functions.
    pub fn bind(
        &mut self,
        kind: GraphSymbolKind,
        name: &str,
        id: u32,
    ) -> Result<(), SymbolConflict> {
        let table = match kind {
            GraphSymbolKind::Label => &mut self.labels,
            GraphSymbolKind::Relation => &mut self.relations,
            GraphSymbolKind::Property => &mut self.properties,
        };
        if table.contains_key(name) || table.values().any(|&bound| bound == id) {
            return Err(SymbolConflict {
                kind,
                name: name.to_owned(),
            });
        }
        table.insert(name.to_owned(), id);
        Ok(())
    }

    /// Resolve a name in exactly the requested kind; never across kinds.
    #[must_use]
    pub fn resolve(&self, kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        match kind {
            GraphSymbolKind::Label => self
                .labels
                .get(name)
                .map(|&id| GraphSymbol::Label(LabelId(u64::from(id)))),
            GraphSymbolKind::Relation => self
                .relations
                .get(name)
                .map(|&id| GraphSymbol::Relation(RelationId(u64::from(id)))),
            GraphSymbolKind::Property => self
                .properties
                .get(name)
                .map(|&id| GraphSymbol::Property(PropertyKeyId(u64::from(id)))),
        }
    }

    pub fn labels(&self) -> impl Iterator<Item = (&str, u32)> {
        self.labels.iter().map(|(name, &id)| (name.as_str(), id))
    }
    pub fn relations(&self) -> impl Iterator<Item = (&str, u32)> {
        self.relations.iter().map(|(name, &id)| (name.as_str(), id))
    }
    pub fn properties(&self) -> impl Iterator<Item = (&str, u32)> {
        self.properties
            .iter()
            .map(|(name, &id)| (name.as_str(), id))
    }
}

impl GraphSymbolResolver for Symbols {
    fn resolve_symbol(&mut self, kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        self.resolve(kind, name)
    }

    fn reverse_catalog(&self) -> Option<ReverseSymbolCatalog> {
        let mut catalog = ReverseSymbolCatalog::new();
        for (name, &id) in &self.labels {
            catalog.insert_label(LabelId(u64::from(id)), name.clone());
        }
        for (name, &id) in &self.relations {
            catalog.insert_relation(RelationId(u64::from(id)), name.clone());
        }
        for (name, &id) in &self.properties {
            catalog.insert_property(PropertyKeyId(u64::from(id)), name.clone());
        }
        Some(catalog)
    }

    fn reverse_label(&self, id: LabelId) -> Option<String> {
        self.labels
            .iter()
            .find_map(|(name, &raw)| (u64::from(raw) == id.0).then(|| name.clone()))
    }

    fn reverse_relation(&self, id: RelationId) -> Option<String> {
        self.relations
            .iter()
            .find_map(|(name, &raw)| (u64::from(raw) == id.0).then(|| name.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bindings_are_unique_in_both_directions_and_never_cross_kinds() {
        let mut symbols = Symbols::new();
        symbols.bind(GraphSymbolKind::Label, "Person", 1).unwrap();
        symbols
            .bind(GraphSymbolKind::Relation, "Person", 1)
            .unwrap();
        assert!(symbols.bind(GraphSymbolKind::Label, "Person", 2).is_err());
        assert!(symbols.bind(GraphSymbolKind::Label, "Company", 1).is_err());
        assert_eq!(
            symbols.resolve(GraphSymbolKind::Label, "Person"),
            Some(GraphSymbol::Label(LabelId(1)))
        );
        assert_eq!(symbols.resolve(GraphSymbolKind::Property, "Person"), None);
        assert_eq!(symbols.reverse_label(LabelId(1)).as_deref(), Some("Person"));
        assert_eq!(symbols.reverse_relation(RelationId(2)), None);
    }
}
