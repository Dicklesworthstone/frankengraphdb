//! Immutable native catalog names with explicit operator declarations.
//!
//! A served database discovers its durable catalog before admitting requests.
//! Operator declarations may add names or repeat an exact durable binding;
//! neither axis of an existing binding may change. Request text can never
//! introduce or rebind a name: an unbound name is a statement refusal.

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
    // Preserve provenance: refreshing or selecting an older snapshot must
    // overlay only operator declarations, never another snapshot's names.
    declarations: Arc<SymbolCatalog>,
    // Hydration borrows the same immutable name generation without rebuilding
    // a catalog on each result row or charging for unrelated hidden names.
    reverse: Arc<ReverseSymbolCatalog>,
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
        // Repeated explicit inputs refuse, including exact duplicates. A
        // first declaration of an already hydrated durable pair is valid and
        // must retain its explicit provenance for later historical rebinding.
        if CatalogSymbolResolver::new(&self.declarations)
            .resolve_symbol(kind, name)
            .is_some()
        {
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
            .map_err(|_| SymbolConflict {
                kind,
                name: name.to_owned(),
            })?;
        // Declarations are a subset of the effective catalog. The admission
        // above therefore establishes both uniqueness axes here as well.
        Arc::make_mut(&mut self.declarations)
            .bind(name, symbol)
            .map_err(|_| SymbolConflict {
                kind,
                name: name.to_owned(),
            })?;
        let reverse = Arc::make_mut(&mut self.reverse);
        match symbol {
            CatalogSymbol::Label(id) => reverse.insert_label(id, name),
            CatalogSymbol::Relation(id) => reverse.insert_relation(id, name),
            CatalogSymbol::Property(id) => reverse.insert_property(id, name),
        }
        Ok(())
    }

    /// Merge these declarations with one selected immutable durable catalog.
    /// Exact redeclarations are harmless; a different name or ordinal refuses
    /// without changing either input. Durable allocator floors remain intact.
    ///
    /// A hydrated resolver retains declaration provenance. Rebinding it to an
    /// earlier snapshot therefore drops later durable names while preserving
    /// explicit operator names. Both name directions share that selected set.
    pub fn with_catalog(&self, durable: &SymbolCatalog) -> Result<Self, SymbolConflict> {
        if self.catalog.as_ref() == durable {
            return Ok(self.clone());
        }
        let mut catalog = durable.clone();
        for (symbol, name) in self.declarations.bindings() {
            catalog.bind(name, symbol).map_err(|_| SymbolConflict {
                kind: match symbol {
                    CatalogSymbol::Label(_) => GraphSymbolKind::Label,
                    CatalogSymbol::Relation(_) => GraphSymbolKind::Relation,
                    CatalogSymbol::Property(_) => GraphSymbolKind::Property,
                },
                name: name.to_owned(),
            })?;
        }
        let mut reverse = ReverseSymbolCatalog::new();
        for (symbol, name) in catalog.bindings() {
            match symbol {
                CatalogSymbol::Label(id) => reverse.insert_label(id, name),
                CatalogSymbol::Relation(id) => reverse.insert_relation(id, name),
                CatalogSymbol::Property(id) => reverse.insert_property(id, name),
            }
        }
        Ok(Self {
            catalog: Arc::new(catalog),
            declarations: Arc::clone(&self.declarations),
            reverse: Arc::new(reverse),
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
    pub(crate) fn borrowed_reverse_catalog(&self) -> &ReverseSymbolCatalog {
        &self.reverse
    }
}

impl GraphSymbolResolver for Symbols {
    fn resolve_symbol(&mut self, kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        self.resolve(kind, name)
    }

    fn reverse_catalog(&self) -> Option<ReverseSymbolCatalog> {
        Some((*self.reverse).clone())
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
    use fgdb_delta_types::catalog::CatalogSymbolKind;
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
    fn durable_names_merge_with_declarations_without_changing_pinned_generations() {
        let mut durable = SymbolCatalog::default();
        durable
            .bind_all([
                ("Person", CatalogSymbol::Label(LabelId(7))),
                ("KNOWS", CatalogSymbol::Relation(RelationId(u64::MAX))),
                ("name", CatalogSymbol::Property(PropertyKeyId(3))),
            ])
            .unwrap();
        durable.reserve_through(CatalogSymbol::Label(LabelId(500)));
        durable.reserve_through(CatalogSymbol::Property(PropertyKeyId(u64::MAX)));
        let durable_before = durable.clone();
        let mut declarations = Symbols::new();
        declarations
            .bind(GraphSymbolKind::Label, "Person", 7)
            .unwrap();
        declarations
            .bind(GraphSymbolKind::Property, "ephemeral", 4)
            .unwrap();
        let declarations_before = declarations.clone();

        let pinned = declarations.with_catalog(&durable).unwrap();
        assert_eq!(pinned.labels().collect::<Vec<_>>(), vec![("Person", 7)]);
        assert_eq!(
            pinned.relations().collect::<Vec<_>>(),
            vec![("KNOWS", u64::MAX)]
        );
        assert_eq!(
            pinned.properties().collect::<Vec<_>>(),
            vec![("ephemeral", 4), ("name", 3)]
        );
        let reverse = pinned.reverse_catalog().unwrap();
        assert_eq!(reverse.labels[&LabelId(7)], "Person");
        assert_eq!(reverse.relations[&RelationId(u64::MAX)], "KNOWS");
        assert_eq!(reverse.properties[&PropertyKeyId(3)], "name");
        assert_eq!(reverse.properties[&PropertyKeyId(4)], "ephemeral");
        assert_eq!(
            pinned.catalog.high_water_mark(CatalogSymbolKind::Label),
            500
        );
        assert_eq!(
            pinned.catalog.high_water_mark(CatalogSymbolKind::Relation),
            u64::MAX
        );
        assert_eq!(
            pinned.catalog.high_water_mark(CatalogSymbolKind::Property),
            u64::MAX
        );
        assert_eq!(durable, durable_before);
        assert_eq!(declarations, declarations_before);

        let mut next = durable.clone();
        next.bind("later", CatalogSymbol::Property(PropertyKeyId(8)))
            .unwrap();
        let refreshed = pinned.with_catalog(&next).unwrap();
        assert_eq!(pinned.resolve(GraphSymbolKind::Property, "later"), None);
        assert_eq!(
            refreshed.resolve(GraphSymbolKind::Property, "later"),
            Some(GraphSymbol::Property(PropertyKeyId(8)))
        );
        assert_eq!(
            refreshed.with_catalog(&durable).unwrap().reverse_catalog(),
            Some(reverse)
        );
        assert_eq!(refreshed.with_catalog(&durable).unwrap(), pinned);
        assert_eq!(
            declarations.resolve(GraphSymbolKind::Relation, "KNOWS"),
            None
        );

        // Host declarations added after hydration are not mistaken for
        // snapshot-owned names, including a first exact durable redeclaration.
        let mut declared_later = refreshed.clone();
        declared_later
            .bind(GraphSymbolKind::Property, "later", 8)
            .unwrap();
        declared_later
            .bind(GraphSymbolKind::Label, "EphemeralLabel", 9)
            .unwrap();
        let before_duplicate = declared_later.clone();
        assert!(
            declared_later
                .bind(GraphSymbolKind::Property, "later", 8)
                .is_err()
        );
        assert_eq!(declared_later, before_duplicate);
        let selected_old = declared_later.with_catalog(&durable).unwrap();
        assert_eq!(
            selected_old.resolve(GraphSymbolKind::Property, "later"),
            Some(GraphSymbol::Property(PropertyKeyId(8)))
        );
        assert_eq!(
            selected_old.resolve(GraphSymbolKind::Label, "EphemeralLabel"),
            Some(GraphSymbol::Label(LabelId(9)))
        );
        assert_eq!(pinned.resolve(GraphSymbolKind::Property, "later"), None);
    }

    #[test]
    fn durable_name_and_ordinal_conflicts_refuse_atomically_in_every_namespace() {
        for (kind, durable_symbol) in [
            (
                GraphSymbolKind::Label,
                CatalogSymbol::Label(LabelId(u64::MAX)),
            ),
            (
                GraphSymbolKind::Relation,
                CatalogSymbol::Relation(RelationId(u64::MAX)),
            ),
            (
                GraphSymbolKind::Property,
                CatalogSymbol::Property(PropertyKeyId(u64::MAX)),
            ),
        ] {
            let mut durable = SymbolCatalog::default();
            durable.bind("bound", durable_symbol).unwrap();
            let durable_before = durable.clone();
            for (name, id) in [("bound", 7), ("alias", u64::MAX)] {
                let mut declarations = Symbols::new();
                declarations.bind(kind, "a_extra", 2).unwrap();
                declarations.bind(kind, name, id).unwrap();
                let before = declarations.clone();
                assert_eq!(
                    declarations.with_catalog(&durable),
                    Err(SymbolConflict {
                        kind,
                        name: name.to_owned(),
                    })
                );
                assert_eq!(declarations, before);
                assert_eq!(durable, durable_before);
                assert_eq!(durable.name(durable_symbol), Some("bound"));
            }
            let mut exact = Symbols::new();
            exact.bind(kind, "bound", u64::MAX).unwrap();
            assert_eq!(exact.with_catalog(&durable).unwrap(), exact);
        }
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
            // The shared FGP/HTTP/Bolt writer must retain the operator's
            // reverse catalog for metadata, including property names
            // absent from the statement. Masked properties never enter the map.
            let update = Execute {
                mode: ExecuteMode::Write,
                statement: "MATCH (a:Person)-[:MENTORS]->(b) SET a.name = properties(b).name"
                    .into(),
                parameters: vec![],
            };
            crate::execute::write(&root, db, &token, &update)
                .await
                .unwrap_or_else(|error| panic!("{}", error.message));
            let copied = crate::execute::read(&root, db, &token, &statement)
                .await
                .unwrap_or_else(|error| panic!("{}", error.message));
            assert_eq!(copied.rows[0][0], WireValue::Text("Grace".into()));
            assert_eq!(
                copied.rows[0][3],
                WireValue::Map(vec![("name".into(), WireValue::Text("Grace".into()))])
            );
            {
                let guard = db.db.read(&root).await.unwrap();
                assert!(
                    guard
                        .vertex(VId(1))
                        .unwrap()
                        .unwrap()
                        .props
                        .contains(&(PropertyKeyId(hidden), CanonicalScalar::Int(917).into()))
                );
            }
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
