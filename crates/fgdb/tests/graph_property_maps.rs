//! Actual graph properties, host naming, snapshot/overlay visibility and
//! capability masking through the native query owners (fgdb-2jw3z).

use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb::{
    Database, DatabaseKeys, GqlError, MemVfs, QueryResult, QueryValue, ReadError, WriteBatch,
};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId, SchemaEpoch};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GraphSymbol, GraphSymbolKind,
    GraphSymbolResolver, PreparedGraphText, ReverseSymbolCatalog,
};
use fgdb_types::{
    CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId,
};
use fgdb_warden::{Authority, Grant, QueryLimits, Scope};

const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x6d; 32]);
const THING: LabelId = LabelId(1);
const R: RelationId = RelationId(1);
const SCORE: PropertyKeyId = PropertyKeyId(1);
const NAME: PropertyKeyId = PropertyKeyId(2);
const NOTE: PropertyKeyId = PropertyKeyId(3);

#[derive(Clone, Copy)]
enum Catalog {
    Full,
    MissingScore,
    RenamedScore,
    DuplicateName,
}

impl GraphSymbolResolver for Catalog {
    fn resolve_symbol(&mut self, kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        match (kind, name) {
            (GraphSymbolKind::Label, "Thing") => Some(GraphSymbol::Label(THING)),
            (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
            (GraphSymbolKind::Property, "score") => Some(GraphSymbol::Property(SCORE)),
            (GraphSymbolKind::Property, "name") => Some(GraphSymbol::Property(NAME)),
            (GraphSymbolKind::Property, "edge-note") => Some(GraphSymbol::Property(NOTE)),
            _ => None,
        }
    }

    fn reverse_catalog(&self) -> Option<ReverseSymbolCatalog> {
        let mut catalog = ReverseSymbolCatalog::new();
        catalog.insert_label(THING, "Thing");
        catalog.insert_relation(R, "R");
        catalog.insert_property(NAME, "name");
        catalog.insert_property(NOTE, "edge-note");
        if !matches!(self, Self::MissingScore) {
            catalog.insert_property(
                SCORE,
                match self {
                    Self::RenamedScore => "renamed-score",
                    Self::DuplicateName => "name",
                    _ => "score",
                },
            );
        }
        Some(catalog)
    }
}

fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0x61; 32], NS, [0x62; 32])
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100_000, 1_000_000, 1_000_000)
}
fn int(value: i64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(value))
}
fn text(value: &str) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::ucs_basic_text(value).unwrap())
}
fn null() -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Null)
}
fn map(entries: &[(&str, GraphValue)]) -> GraphValue {
    GraphValue::map(
        entries
            .iter()
            .map(|(key, value)| (Box::<str>::from(*key), value.clone()))
            .collect(),
    )
    .unwrap()
}
fn list(values: &[&str]) -> GraphValue {
    GraphValue::List(values.iter().map(|value| text(value)).collect())
}
fn cells(result: QueryResult) -> Vec<Vec<GraphValue>> {
    let QueryResult::Rows { rows, .. } = result else {
        panic!("expected rows")
    };
    rows.into_iter()
        .map(|row| {
            row.into_iter()
                .map(|value| match value {
                    QueryValue::Value(value) => value,
                    QueryValue::Count(value) => int(i64::try_from(value).unwrap()),
                    QueryValue::Integer(value) => int(i64::try_from(value).unwrap()),
                    value => panic!("unexpected cell {value:?}"),
                })
                .collect()
        })
        .collect()
}

async fn seed(db: &mut Database<MemVfs>, cx: &CommitCx, hidden: bool) -> CommitSeq {
    let mut batch = WriteBatch::new(R);
    let mut props = vec![
        (SCORE, CanonicalScalar::Int(10)),
        (NAME, CanonicalScalar::ucs_basic_text("one").unwrap()),
    ];
    if hidden {
        // These names are deliberately absent from the host catalog. A masked
        // property must not cause a naming refusal or a per-field charge.
        for id in 90..110 {
            props.push((
                PropertyKeyId(id),
                CanonicalScalar::ucs_basic_text("hidden payload").unwrap(),
            ));
        }
    }
    batch.create_vertex(VId(1), vec![THING], props);
    batch.create_vertex(VId(2), vec![THING], vec![]);
    let mut edge_props = vec![
        (SCORE, CanonicalScalar::Int(3)),
        (NOTE, CanonicalScalar::Bool(true)),
    ];
    if hidden {
        edge_props.push((PropertyKeyId(99), CanonicalScalar::Int(999)));
        batch.create_vertex(
            VId(3),
            vec![LabelId(99)],
            vec![(PropertyKeyId(99), CanonicalScalar::Int(999))],
        );
        batch.add_edge(
            EId(2),
            VId(1),
            VId(3),
            vec![(PropertyKeyId(99), CanonicalScalar::Int(999))],
        );
    }
    batch.add_edge(EId(1), VId(1), VId(2), edge_props);
    db.write(cx, batch).await.unwrap()
}

#[test]
fn graph_maps_cover_vertices_relationships_optional_scopes_and_row_composition() {
    let ((), report) = run_async_under_lab(0x6d61_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit, false).await;
        let params = GqlParameters::new();
        let node = map(&[("name", text("one")), ("score", int(10))]);
        let edge = map(&[
            ("edge-note", GraphValue::Scalar(CanonicalScalar::Bool(true))),
            ("score", int(3)),
        ]);
        for (statement, expected) in [
            (
                "MATCH (n:Thing) RETURN n.score AS score, properties(n) AS props, keys(n) AS keys ORDER BY score",
                vec![
                    vec![int(10), node.clone(), list(&["name", "score"])],
                    vec![null(), map(&[]), list(&[])],
                ],
            ),
            (
                "MATCH (a)-[r:R]->(b) RETURN properties(r) AS props, keys(r) AS keys",
                vec![vec![edge.clone(), list(&["edge-note", "score"])]],
            ),
            (
                "MATCH (n:Thing) OPTIONAL MATCH (n)-[r:R]->(m) RETURN n.score AS score, properties(r) AS props, keys(r) AS keys, m{.*} AS target ORDER BY score",
                vec![
                    vec![
                        int(10),
                        edge.clone(),
                        list(&["edge-note", "score"]),
                        map(&[]),
                    ],
                    vec![null(), null(), null(), null()],
                ],
            ),
            (
                "MATCH (n:Thing) WITH properties(n) AS m RETURN m.score AS score, keys(m) AS keys ORDER BY score",
                vec![
                    vec![int(10), list(&["name", "score"])],
                    vec![null(), list(&[])],
                ],
            ),
            (
                "MATCH (n:Thing) WITH n RETURN n.score AS score, properties(n) AS props ORDER BY score",
                vec![vec![int(10), node.clone()], vec![null(), map(&[])]],
            ),
            (
                "MATCH (n:Thing) WHERE n.score=10 RETURN n{.*, score: 11, extra: 'kept'} AS props",
                vec![vec![map(&[
                    ("extra", text("kept")),
                    ("name", text("one")),
                    ("score", int(11)),
                ])]],
            ),
            (
                "MATCH (a)-[r:R]->(b) RETURN r{.*, score: NULL} AS props",
                vec![vec![map(&[
                    ("edge-note", GraphValue::Scalar(CanonicalScalar::Bool(true))),
                    ("score", null()),
                ])]],
            ),
            (
                "RETURN properties({b:2,a:1}) AS m, properties(NULL) AS absent, keys(NULL) AS keys",
                vec![vec![map(&[("a", int(1)), ("b", int(2))]), null(), null()]],
            ),
        ] {
            assert_eq!(
                cells(
                    db.query(&cx, statement, &params, Catalog::Full, policy())
                        .expect(statement)
                ),
                expected,
                "{statement}"
            );
        }
        let grouped = cells(
            db.query(
                &cx,
                "MATCH (n:Thing) RETURN properties(n) AS props, count(*) AS count ORDER BY props",
                &params,
                Catalog::Full,
                policy(),
            )
            .unwrap(),
        );
        assert_eq!(
            grouped,
            vec![vec![map(&[]), int(1)], vec![node.clone(), int(1)]]
        );
        let collected = cells(
            db.query(
                &cx,
                "MATCH (n:Thing) RETURN collect(properties(n)) AS props",
                &params,
                Catalog::Full,
                policy(),
            )
            .unwrap(),
        );
        let [row] = collected.as_slice() else {
            panic!("one aggregate row")
        };
        let [GraphValue::List(values)] = row.as_slice() else {
            panic!("collected maps")
        };
        let mut actual = values.to_vec();
        let mut expected = vec![node, map(&[])];
        actual.sort();
        expected.sort();
        assert_eq!(actual, expected);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn actual_property_ids_must_be_named_even_for_an_empty_selected_page() {
    let ((), report) = run_async_under_lab(0x6d61_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit, false).await;
        for tail in ["", " LIMIT 0"] {
            let statement = format!("MATCH (n:Thing) RETURN properties(n) AS props{tail}");
            let query = PreparedGraphText::prepare(&statement, Catalog::MissingScore)
                .unwrap()
                .bind_parameters(&GqlParameters::new())
                .unwrap();
            assert!(matches!(
                db.execute_graph_pattern_governed(&cx, &query, policy()),
                Err(GqlQueryError::Source(GqlError::Read(
                    ReadError::UnmappedProperty(SCORE)
                )))
            ));
        }
        let query = PreparedGraphText::prepare(
            "MATCH (n:Thing) RETURN properties(n) AS props",
            Catalog::DuplicateName,
        )
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
        assert!(matches!(
            db.execute_graph_pattern_governed(&cx, &query, policy()),
            Err(GqlQueryError::Source(GqlError::Read(
                ReadError::InvalidPropertyMap
            )))
        ));
        for statement in [
            "MATCH (n:Thing) RETURN properties(n) AS props",
            "MATCH (n:Thing) RETURN n",
        ] {
            let bind = |catalog| {
                PreparedGraphText::prepare(statement, catalog)
                    .unwrap()
                    .bind_parameters(&GqlParameters::new())
                    .unwrap()
                    .canonical_bytes()
            };
            if statement.contains("properties") {
                assert_ne!(bind(Catalog::Full), bind(Catalog::RenamedScore));
            } else {
                assert_eq!(bind(Catalog::Full), bind(Catalog::RenamedScore));
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn property_maps_follow_effective_overlay_pins_history_compaction_and_reopen() {
    let ((), report) = run_async_under_lab(0x6d61_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let basis = seed(&mut db, &commit, false).await;
        let pinned = db.read_session().unwrap();
        let params = GqlParameters::new();
        let statement = "MATCH (n)-[r:R]->(m) RETURN properties(n) AS node, properties(r) AS edge";
        let before = cells(
            db.query(&cx, statement, &params, Catalog::Full, policy())
                .unwrap(),
        );
        let mut txn = db.begin(&txcx).unwrap();
        let mut edit = WriteBatch::new(R);
        edit.set_vertex_property(VId(1), SCORE, Some(CanonicalScalar::Int(20)));
        edit.set_vertex_property(VId(1), NAME, None);
        edit.set_vertex_property(
            VId(1),
            NOTE,
            Some(CanonicalScalar::ucs_basic_text("added").unwrap()),
        );
        edit.set_edge_property(EId(1), SCORE, None);
        edit.set_edge_property(
            EId(1),
            NAME,
            Some(CanonicalScalar::ucs_basic_text("edge name").unwrap()),
        );
        txn.write(&mut db, edit).unwrap();
        let expected = vec![vec![
            map(&[("edge-note", text("added")), ("score", int(20))]),
            map(&[
                ("edge-note", GraphValue::Scalar(CanonicalScalar::Bool(true))),
                ("name", text("edge name")),
            ]),
        ]];
        assert_eq!(
            cells(
                txn.query(&db, &cx, statement, &params, Catalog::Full, policy())
                    .unwrap()
            ),
            expected
        );
        assert_eq!(
            cells(
                db.query(&cx, statement, &params, Catalog::Full, policy())
                    .unwrap()
            ),
            before
        );
        txn.commit(&mut db, &commit).await.unwrap();
        db.compact(&commit).await.unwrap();
        drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        assert_eq!(
            cells(
                db.query(&cx, statement, &params, Catalog::Full, policy())
                    .unwrap()
            ),
            expected
        );
        assert_eq!(
            cells(
                pinned
                    .query(&cx, statement, &params, Catalog::Full, policy())
                    .unwrap()
            ),
            before
        );
        let query = PreparedGraphText::prepare(statement, Catalog::Full)
            .unwrap()
            .bind_parameters(&params)
            .unwrap();
        let old = db
            .execute_graph_pattern_governed_at(&cx, &query, basis, policy())
            .unwrap()
            .value;
        assert_eq!(
            old.iter()
                .map(|row| row.values().to_vec())
                .collect::<Vec<_>>(),
            before
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn property_maps_mask_hidden_fields_before_naming_allocation_and_budget_charges() {
    let ((), report) = run_async_under_lab(0x6d61_0004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let mut hidden = Database::open_memory(&commit, keys()).await.unwrap();
        let mut removed = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut hidden, &commit, true).await;
        seed(&mut removed, &commit, false).await;
        let issuer = Authority::new(
            AuthKey::from_seed(0x6d61_0010),
            NS,
            "graph",
            SchemaEpoch(0),
            1,
        )
        .unwrap();
        let mut grant = Grant::read_only(
            "main",
            1000,
            QueryLimits {
                max_nodes: 100_000,
                max_work: 10_000_000,
                max_rows: 100_000,
            },
        );
        grant.labels = Scope::only([THING]);
        grant.relations = Scope::only([R]);
        grant.properties = Scope::only([SCORE, NAME, NOTE]);
        let token = issuer.issue_at(&grant, 100).unwrap();
        let params = GqlParameters::new();
        for statement in [
            "MATCH (n:Thing) RETURN properties(n) AS props, keys(n) AS keys ORDER BY props",
            "MATCH (a)-[r]->(b) RETURN r{.*} AS props, keys(r) AS keys",
            "MATCH (n:Thing) RETURN properties(n) AS props, count(*) AS count ORDER BY props",
        ] {
            let read = |db: &Database<MemVfs>, maximum| {
                db.query_authorized(
                    &cx,
                    &issuer,
                    &token,
                    "main",
                    statement,
                    &params,
                    Catalog::Full,
                    GqlQueryPolicy::new(100_000, 100_000, maximum, 1_000_000),
                    || 100,
                )
            };
            let expected = cells(
                removed
                    .query(&cx, statement, &params, Catalog::Full, policy())
                    .unwrap(),
            );
            assert_eq!(
                cells(read(&hidden, 1_000_000).unwrap()),
                expected,
                "{statement}"
            );
            assert_eq!(
                cells(read(&removed, 1_000_000).unwrap()),
                expected,
                "{statement}"
            );
            let minimum = |db: &Database<MemVfs>| {
                let (mut low, mut high) = (0, 1_000_000);
                while low < high {
                    let middle = low + (high - low) / 2;
                    if read(db, middle).is_ok() {
                        high = middle;
                    } else {
                        low = middle + 1;
                    }
                }
                low
            };
            let threshold = minimum(&removed);
            assert_eq!(
                minimum(&hidden),
                threshold,
                "hidden field charges: {statement}"
            );
            assert!(threshold > 0);
            assert!(read(&hidden, threshold - 1).is_err());
            assert_eq!(cells(read(&hidden, threshold).unwrap()), expected);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
