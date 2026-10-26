//! The operator-declared name bindings a served database resolves through.
//!
//! The engine has no durable catalog yet (`fgdb-w4-schema-catalog-ea4e`), so a
//! label, relation or property name means exactly what the operator bound it
//! to when the database was served, the same contract as the CLI's
//! `--label/--relation/--property` flags. Request text can never introduce or
//! rebind a name: an unbound name is an ordinary statement refusal.

use fgdb_delta_types::catalog::{CatalogSymbol, SymbolCatalog};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::symbol_bindings::CatalogSymbolResolver;
use fgdb_gql::{GraphSymbol, GraphSymbolKind, GraphSymbolResolver, ReverseSymbolCatalog};
use std::sync::Arc;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Symbols {
    // Sessions retain this immutable catalog. Operator changes fork its state
    // only when an older resolver still owns a reference.
    catalog: Arc<SymbolCatalog>,
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
        id: u64,
    ) -> Result<(), SymbolConflict> {
        // Explicit operator inputs retain their duplicate-input refusal,
        // including exact redeclarations. The catalog itself also serves
        // transactional owners, for whom exact redeclarations are idempotent.
        if self.resolve(kind, name).is_some() {
            return Err(SymbolConflict {
                kind,
                name: name.to_owned(),
            });
        }
        let symbol = match kind {
            GraphSymbolKind::Label => CatalogSymbol::Label(LabelId(id)),
            GraphSymbolKind::Relation => CatalogSymbol::Relation(RelationId(id)),
            GraphSymbolKind::Property => CatalogSymbol::Property(PropertyKeyId(id)),
        };
        Arc::make_mut(&mut self.catalog)
            .bind(name, symbol)
            .map(|_| ())
            .map_err(|_| SymbolConflict {
                kind,
                name: name.to_owned(),
            })
    }

    /// Resolve a name in exactly the requested kind; never across kinds.
    #[must_use]
    pub fn resolve(&self, kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        CatalogSymbolResolver::new(&self.catalog).resolve_symbol(kind, name)
    }

    pub fn labels(&self) -> impl Iterator<Item = (&str, u64)> {
        self.catalog
            .bindings()
            .filter_map(|(symbol, name)| match symbol {
                CatalogSymbol::Label(id) => Some((name, id.0)),
                _ => None,
            })
    }
    pub fn relations(&self) -> impl Iterator<Item = (&str, u64)> {
        self.catalog
            .bindings()
            .filter_map(|(symbol, name)| match symbol {
                CatalogSymbol::Relation(id) => Some((name, id.0)),
                _ => None,
            })
    }
    pub fn properties(&self) -> impl Iterator<Item = (&str, u64)> {
        self.catalog
            .bindings()
            .filter_map(|(symbol, name)| match symbol {
                CatalogSymbol::Property(id) => Some((name, id.0)),
                _ => None,
            })
    }
}

impl GraphSymbolResolver for Symbols {
    fn resolve_symbol(&mut self, kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        self.resolve(kind, name)
    }

    fn reverse_catalog(&self) -> Option<ReverseSymbolCatalog> {
        CatalogSymbolResolver::new(&self.catalog).reverse_catalog()
    }

    fn reverse_label(&self, id: LabelId) -> Option<String> {
        CatalogSymbolResolver::new(&self.catalog).reverse_label(id)
    }

    fn reverse_relation(&self, id: RelationId) -> Option<String> {
        CatalogSymbolResolver::new(&self.catalog).reverse_relation(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use asupersync::lab::run_async_under_lab;
    use fgdb::WriteBatch;
    use fgdb_protocol::body::{Execute, ExecuteMode, WireValue};
    use fgdb_types::{CanonicalScalar, EId, PurposeContexts, VId};
    use fgdb_warden::{Restriction, Scope};
    use std::sync::Arc;

    #[test]
    fn bindings_are_unique_in_both_directions_and_never_cross_kinds() {
        let mut symbols = Symbols::new();
        symbols.bind(GraphSymbolKind::Label, "Person", 1).unwrap();
        symbols
            .bind(GraphSymbolKind::Relation, "Person", 1)
            .unwrap();
        let before = symbols.clone();
        assert!(symbols.bind(GraphSymbolKind::Label, "Person", 1).is_err());
        assert!(symbols.bind(GraphSymbolKind::Label, "Person", 2).is_err());
        assert!(symbols.bind(GraphSymbolKind::Label, "Company", 1).is_err());
        assert!(symbols.bind(GraphSymbolKind::Property, "", 1).is_err());
        assert_eq!(symbols, before);
        assert_eq!(
            symbols.resolve(GraphSymbolKind::Label, "Person"),
            Some(GraphSymbol::Label(LabelId(1)))
        );
        assert_eq!(symbols.resolve(GraphSymbolKind::Property, "Person"), None);
        assert_eq!(symbols.reverse_label(LabelId(1)).as_deref(), Some("Person"));
        assert_eq!(symbols.reverse_relation(RelationId(2)), None);
    }

    #[test]
    fn native_ordinals_survive_every_served_catalog_observation() {
        let mut symbols = Symbols::new();
        let id = u64::MAX;
        for kind in [
            GraphSymbolKind::Label,
            GraphSymbolKind::Relation,
            GraphSymbolKind::Property,
        ] {
            symbols.bind(kind, "native", id).unwrap();
        }
        assert_eq!(symbols.labels().collect::<Vec<_>>(), vec![("native", id)]);
        assert_eq!(
            symbols.relations().collect::<Vec<_>>(),
            vec![("native", id)]
        );
        assert_eq!(
            symbols.properties().collect::<Vec<_>>(),
            vec![("native", id)]
        );
        assert_eq!(
            symbols.resolve(GraphSymbolKind::Property, "native"),
            Some(GraphSymbol::Property(PropertyKeyId(id)))
        );
        assert_eq!(
            symbols.reverse_label(LabelId(id)).as_deref(),
            Some("native")
        );
        assert_eq!(
            symbols.reverse_relation(RelationId(id)).as_deref(),
            Some("native")
        );
        let catalog = symbols.reverse_catalog().unwrap();
        assert_eq!(catalog.labels[&LabelId(id)], "native");
        assert_eq!(catalog.relations[&RelationId(id)], "native");
        assert_eq!(catalog.properties[&PropertyKeyId(id)], "native");
        let pinned = symbols.clone();
        assert!(Arc::ptr_eq(&symbols.catalog, &pinned.catalog));
        symbols.bind(GraphSymbolKind::Property, "later", 1).unwrap();
        assert!(!Arc::ptr_eq(&symbols.catalog, &pinned.catalog));
        assert_eq!(pinned.resolve(GraphSymbolKind::Property, "later"), None);
        assert_eq!(
            symbols.resolve(GraphSymbolKind::Property, "later"),
            Some(GraphSymbol::Property(PropertyKeyId(1)))
        );
        assert_eq!(pinned.reverse_catalog().unwrap(), catalog);
    }

    #[test]
    fn served_queries_and_capability_schema_preserve_native_catalog_ids() {
        let ((), report) = run_async_under_lab(0x7a8c_0010, |root| async move {
            let (mut server, token, _) =
                crate::recovery::tests::served(&root, "native-symbol-catalog").await;
            let native = u64::MAX;
            let hidden = native - 1;
            {
                let db = Arc::get_mut(server.databases.get_mut("test").unwrap()).unwrap();
                for (kind, name, id) in [
                    (GraphSymbolKind::Label, "Person", native),
                    (GraphSymbolKind::Label, "Secret", hidden),
                    (GraphSymbolKind::Relation, "MENTORS", native),
                    (GraphSymbolKind::Relation, "SECRET", hidden),
                    (GraphSymbolKind::Property, "name", native),
                    (GraphSymbolKind::Property, "secret", hidden),
                ] {
                    db.symbols.bind(kind, name, id).unwrap();
                }
            }
            let db = &server.databases["test"];
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            {
                let mut guard = db.db.write(&root).await.unwrap();
                let mut batch = WriteBatch::new(RelationId(native));
                for (id, name) in [(1, "Ada"), (2, "Grace")] {
                    batch.create_vertex(
                        VId(id),
                        vec![LabelId(native)],
                        vec![
                            (PropertyKeyId(hidden), CanonicalScalar::Int(917)),
                            (
                                PropertyKeyId(native),
                                CanonicalScalar::ucs_basic_text(name).unwrap(),
                            ),
                        ],
                    );
                }
                batch.add_edge(EId(1), VId(1), VId(2), vec![]);
                guard.write(&contexts.commit(), batch).await.unwrap();
            }
            let token = token
                .attenuate(Restriction::Labels(Scope::only([LabelId(native)])))
                .unwrap()
                .attenuate(Restriction::Relations(Scope::only([RelationId(native)])))
                .unwrap()
                .attenuate(Restriction::Properties(Scope::only([PropertyKeyId(
                    native,
                )])))
                .unwrap();
            let schema = crate::execute::schema(db, &token)
                .unwrap_or_else(|error| panic!("{}", error.message));
            assert_eq!(schema.labels, vec!["Person"]);
            assert_eq!(schema.relations, vec!["MENTORS"]);
            assert_eq!(schema.properties, vec!["name"]);
            let statement = Execute {
                mode: ExecuteMode::Read,
                statement: "MATCH (a:Person)-[r:MENTORS]->(b) RETURN a.name AS first, type(r) AS relation, b.name AS second, properties(a) AS props, labels(a) AS labels, keys(a) AS keys".into(),
                parameters: vec![],
            };
            let answer = crate::execute::read(&root, db, &token, &statement)
                .await
                .unwrap_or_else(|error| panic!("{}", error.message));
            assert_eq!(
                answer.rows,
                vec![vec![
                    WireValue::Text("Ada".into()),
                    WireValue::Text("MENTORS".into()),
                    WireValue::Text("Grace".into()),
                    WireValue::Map(vec![("name".into(), WireValue::Text("Ada".into()))]),
                    WireValue::List(vec![WireValue::Text("Person".into())]),
                    WireValue::List(vec![WireValue::Text("name".into())]),
                ]]
            );
            // This ordinal shares the low 32 bits with `native`. A narrowed
            // catalog or scope must not make it authorize the native symbol.
            let low = u64::from(u32::MAX);
            let denied = token
                .attenuate(Restriction::Labels(Scope::only([LabelId(low)])))
                .unwrap()
                .attenuate(Restriction::Relations(Scope::only([RelationId(low)])))
                .unwrap()
                .attenuate(Restriction::Properties(Scope::only([PropertyKeyId(low)])))
                .unwrap();
            let schema = crate::execute::schema(db, &denied)
                .unwrap_or_else(|error| panic!("{}", error.message));
            assert!(schema.labels.is_empty());
            assert!(schema.relations.is_empty());
            assert!(schema.properties.is_empty());
            server.join_database_workers(&root).await;
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }
}
