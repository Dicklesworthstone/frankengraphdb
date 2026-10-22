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
    GqlParameters, GqlQueryError, GqlQueryPolicy, GraphMutationPolicy, GraphSymbol,
    GraphSymbolKind, GraphSymbolResolver, GraphVertexMergePolicy, GraphVertexUpsertPolicy,
    GraphWriteProgramPolicy, PreparedGraphMutationQueryText, PreparedGraphText,
    PreparedGraphVertexUpsertQueryText, ReverseSymbolCatalog,
};
use fgdb_types::{
    CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId,
};
use fgdb_warden::{Authority, Grant, QueryLimits, Rights, Scope};

const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x6d; 32]);
const THING: LabelId = LabelId(1);
const FOO: LabelId = LabelId(2);
const BAR: LabelId = LabelId(3);
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
            (GraphSymbolKind::Label, "Foo") => Some(GraphSymbol::Label(FOO)),
            (GraphSymbolKind::Label, "Bar") => Some(GraphSymbol::Label(BAR)),
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
        catalog.insert_label(FOO, "Foo");
        catalog.insert_label(BAR, "Bar");
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
fn write_policy() -> GraphWriteProgramPolicy {
    GraphWriteProgramPolicy::new(policy(), 100_000, 100_000, 100_000)
}
fn upsert_policy() -> GraphVertexUpsertPolicy {
    GraphVertexUpsertPolicy::new(GraphVertexMergePolicy::new(policy()), 100_000)
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
            // An UNWIND right after MATCH or WITH reads a carried element's
            // keys and properties (TCK Graph8 [1] to [6]).
            (
                "MATCH (n:Thing) UNWIND keys(n) AS k RETURN k ORDER BY k",
                vec![vec![text("name")], vec![text("score")]],
            ),
            (
                "MATCH (n:Thing) WHERE n.score = 10 WITH n UNWIND [n.score, n.score + 1] AS v \
                 RETURN v ORDER BY v",
                vec![vec![int(10)], vec![int(11)]],
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
            // A literal NULL argument to a metadata function is NULL, beside
            // the same function of an unmatched optional binding (TCK Graph3
            // [7], Graph4 [3]).
            (
                "RETURN labels(NULL) AS l, type(null) AS t, length(NULL) AS n",
                vec![vec![null(), null(), null()]],
            ),
            (
                "OPTIONAL MATCH (n:Foo) RETURN labels(n) AS l, labels(null) AS absent",
                vec![vec![null(), null()]],
            ),
            (
                "MATCH (a:Thing) OPTIONAL MATCH (a)-[r:R]->(b:Foo) \
                 RETURN type(r) AS t, type(null) AS absent",
                vec![vec![null(), null()], vec![null(), null()]],
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
            let query = PreparedGraphText::prepare_with_resolver(&statement, Catalog::MissingScore)
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
        let query = PreparedGraphText::prepare_with_resolver(
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
                PreparedGraphText::prepare_with_resolver(statement, catalog)
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
        let query = PreparedGraphText::prepare_with_resolver(statement, Catalog::Full)
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

#[test]
fn mutation_return_maps_use_final_node_and_edge_assignments_before_publication() {
    let ((), report) = run_async_under_lab(0x6d61_0005, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, cx, txcx) = (contexts.commit(), contexts.query(), contexts.txn());
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let before = seed(&mut db, &commit, false).await;
        let pinned = db.read_session().unwrap();
        let params = GqlParameters::new();
        let result = db
            .query_write_engine(
                &txcx,
                &cx,
                &commit,
                "MATCH (a:Thing)-[r:R]->(b:Thing) \
                 SET a.score = a.score + 1, r.score = r.score + 2 \
                 RETURN properties(a) AS node, properties(r) AS edge, \
                 a{.*, score:a.score * 10} AS projected, keys(a) AS keys",
                &params,
                Catalog::Full,
                R,
                write_policy(),
            )
            .await
            .unwrap();
        let node = map(&[("name", text("one")), ("score", int(11))]);
        let edge = map(&[
            ("edge-note", GraphValue::Scalar(CanonicalScalar::Bool(true))),
            ("score", int(5)),
        ]);
        assert_eq!(
            cells(result),
            vec![vec![
                node.clone(),
                edge.clone(),
                map(&[("name", text("one")), ("score", int(110))]),
                list(&["name", "score"]),
            ]]
        );
        assert_eq!(db.frontier().unwrap().0, before.0 + 1);
        let read = "MATCH (a)-[r:R]->(b) RETURN properties(a) AS node, properties(r) AS edge";
        assert_eq!(
            cells(
                db.query(&cx, read, &params, Catalog::Full, policy())
                    .unwrap()
            ),
            vec![vec![node, edge]],
        );
        assert_eq!(
            cells(
                pinned
                    .query(&cx, read, &params, Catalog::Full, policy())
                    .unwrap()
            ),
            vec![vec![
                map(&[("name", text("one")), ("score", int(10))]),
                map(&[
                    ("edge-note", GraphValue::Scalar(CanonicalScalar::Bool(true))),
                    ("score", int(3)),
                ]),
            ]],
        );
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

/// openCypher `SET n = {...}` (TCK Set4): the target keeps exactly the map's
/// properties. Every other property it carries is unset, named through the
/// host's reverse catalog, which must name each property identity once.
#[test]
fn set_to_a_literal_map_replaces_every_property() {
    let ((), report) = run_async_under_lab(0x6d61_000a, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, cx, txcx) = (contexts.commit(), contexts.query(), contexts.txn());
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit, false).await;
        let params = GqlParameters::new();
        let write = async |db: &mut Database<MemVfs>, text: &str, catalog: Catalog| {
            db.query_write_engine(
                &txcx,
                &cx,
                &commit,
                text,
                &params,
                catalog,
                R,
                write_policy(),
            )
            .await
        };
        let replaced = write(
            &mut db,
            "MATCH (a:Thing {name: 'one'}) SET a = {score: 40} \
             RETURN properties(a) AS node, a.name AS name",
            Catalog::Full,
        )
        .await
        .unwrap();
        assert_eq!(
            cells(replaced),
            vec![vec![map(&[("score", int(40))]), null()]]
        );
        assert_eq!(
            db.vertex(VId(1)).unwrap().unwrap().props,
            vec![(SCORE, CanonicalScalar::Int(40).into())]
        );
        let emptied = write(
            &mut db,
            "MATCH (a:Thing) WHERE a.score = 40 SET a = {} RETURN keys(a) AS keys",
            Catalog::Full,
        )
        .await
        .unwrap();
        assert_eq!(cells(emptied), vec![vec![list(&[])]]);
        assert!(db.vertex(VId(1)).unwrap().unwrap().props.is_empty());
        let absent = write(
            &mut db,
            "OPTIONAL MATCH (a:Foo) SET a = {score: 1} RETURN a",
            Catalog::Full,
        )
        .await
        .unwrap();
        assert_eq!(cells(absent), vec![vec![null()]]);
        let frontier = db.frontier().unwrap();
        for (refused, catalog) in [
            (
                "MATCH (a:Thing) SET a = {score: 1, score: 2}",
                Catalog::Full,
            ),
            // Two identities named "name": a removal could hit either.
            ("MATCH (a:Thing) SET a = {}", Catalog::DuplicateName),
        ] {
            assert!(write(&mut db, refused, catalog).await.is_err(), "{refused}");
        }
        assert_eq!(db.frontier().unwrap(), frontier);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

/// fgdb-k3nub: a write RETURN's property map carries a stored List field
/// unchanged beside the fields the write assigns, for SET and vertex MERGE,
/// and the List property itself returns as a List, with or without a clause.
#[test]
fn write_return_maps_carry_stored_list_fields() {
    let ((), report) = run_async_under_lab(0x6d61_000b, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, cx, txcx) = (contexts.commit(), contexts.query(), contexts.txn());
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let tags = fgdb_types::CanonicalList::try_new(vec![
            CanonicalScalar::ucs_basic_text("a").unwrap().into(),
            CanonicalScalar::Int(2).into(),
        ])
        .unwrap();
        let mut batch = WriteBatch::new(R);
        batch.create_vertex_values(
            VId(7),
            vec![FOO],
            vec![
                (SCORE, CanonicalScalar::Int(1).into()),
                (NAME, fgdb_types::CanonicalPropertyValue::List(tags)),
            ],
        );
        db.write(&commit, batch).await.unwrap();
        let listed = GraphValue::List(vec![text("a"), int(2)].into_boxed_slice());
        let params = GqlParameters::new();
        let node = map(&[("name", listed.clone()), ("score", int(5))]);
        for (statement, expected) in [
            (
                "MATCH (a:Foo) SET a.score = 5 RETURN properties(a) AS node",
                vec![node.clone()],
            ),
            (
                "MERGE (a:Foo) ON MATCH SET a.score = 5 RETURN properties(a) AS node",
                vec![node.clone()],
            ),
            ("MERGE (a:Foo) RETURN properties(a) AS node", vec![node]),
            (
                "MERGE (a:Foo) ON MATCH SET a.score = 5 RETURN a.name AS name",
                vec![listed.clone()],
            ),
            ("MERGE (a:Foo) RETURN a.name AS name", vec![listed.clone()]),
        ] {
            let result = db
                .query_write_engine(
                    &txcx,
                    &cx,
                    &commit,
                    statement,
                    &params,
                    Catalog::Full,
                    R,
                    write_policy(),
                )
                .await
                .map_err(|error| format!("{statement}: {error}"))
                .unwrap();
            assert_eq!(cells(result), vec![expected], "{statement}");
        }
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn mutation_return_maps_compose_with_prior_overlay_and_remove_fields() {
    let ((), report) = run_async_under_lab(0x6d61_0006, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, cx, txcx) = (contexts.commit(), contexts.query(), contexts.txn());
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let before = seed(&mut db, &commit, false).await;
        let mut transaction = db.begin(&txcx).unwrap();
        let mut prefix = WriteBatch::new(R);
        prefix.set_vertex_property(VId(1), SCORE, Some(CanonicalScalar::Int(20)));
        prefix.set_vertex_property(VId(1), NAME, None);
        prefix.set_vertex_property(VId(1), NOTE, Some(CanonicalScalar::Int(4)));
        transaction.write(&mut db, prefix).unwrap();
        let params = GqlParameters::new();
        let result = transaction
            .query_write_engine(
                &mut db,
                &cx,
                "MATCH (a)-[r:R]->(b) SET a.score = a.score + 1 \
                 RETURN a{.*} AS node, keys(a) AS keys",
                &params,
                Catalog::Full,
                R,
                write_policy(),
            )
            .unwrap();
        assert_eq!(
            cells(result),
            vec![vec![
                map(&[("edge-note", int(4)), ("score", int(21))]),
                list(&["edge-note", "score"]),
            ]]
        );
        let result = transaction
            .query_write_engine(
                &mut db,
                &cx,
                "MATCH (a)-[r:R]->(b) REMOVE a.`edge-note`, r.score \
                 RETURN properties(a) AS node, properties(r) AS edge, keys(a) AS keys",
                &params,
                Catalog::Full,
                R,
                write_policy(),
            )
            .unwrap();
        assert_eq!(
            cells(result),
            vec![vec![
                map(&[("score", int(21))]),
                map(&[("edge-note", GraphValue::Scalar(CanonicalScalar::Bool(true)))]),
                list(&["score"]),
            ]]
        );
        assert_eq!(db.frontier().unwrap(), before);
        transaction.abort();
        assert_eq!(
            db.vertex(VId(1)).unwrap().unwrap().props,
            vec![
                (SCORE, CanonicalScalar::Int(10).into()),
                (NAME, CanonicalScalar::ucs_basic_text("one").unwrap().into()),
            ]
        );
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn mutation_return_map_aggregation_and_limit_do_not_limit_effects() {
    let ((), report) = run_async_under_lab(0x6d61_0007, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, cx, txcx) = (contexts.commit(), contexts.query(), contexts.txn());
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit, false).await;
        let params = GqlParameters::new();
        let result = db
            .query_write_engine(
                &txcx,
                &cx,
                &commit,
                "MATCH (n:Thing) SET n.score = 7 \
                 RETURN collect(properties(n)) AS maps, count(*) AS count",
                &params,
                Catalog::Full,
                R,
                write_policy(),
            )
            .await
            .unwrap();
        assert_eq!(
            cells(result),
            vec![vec![
                GraphValue::List(
                    vec![
                        map(&[("name", text("one")), ("score", int(7))]),
                        map(&[("score", int(7))]),
                    ]
                    .into_boxed_slice()
                ),
                int(2),
            ]]
        );
        let result = db
            .query_write_engine(
                &txcx,
                &cx,
                &commit,
                "MATCH (n:Thing) SET n.score = 9 RETURN n{.*} AS props LIMIT 0",
                &params,
                Catalog::Full,
                R,
                write_policy(),
            )
            .await
            .unwrap();
        assert!(cells(result).is_empty());
        for id in [VId(1), VId(2)] {
            assert_eq!(
                db.vertex(id).unwrap().unwrap().props[0],
                (SCORE, CanonicalScalar::Int(9).into())
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn mutation_return_map_failures_preserve_prior_staging_and_durable_state() {
    let ((), report) = run_async_under_lab(0x6d61_0008, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, cx, txcx) = (contexts.commit(), contexts.query(), contexts.txn());
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let before = seed(&mut db, &commit, false).await;
        let mut transaction = db.begin(&txcx).unwrap();
        let mut prefix = WriteBatch::new(R);
        prefix.set_vertex_property(VId(1), SCORE, Some(CanonicalScalar::Int(20)));
        transaction.write(&mut db, prefix).unwrap();
        let digest = transaction.staged_effect_digest().unwrap();
        let params = GqlParameters::new();
        for (statement, catalog, budget) in [
            (
                "MATCH (n:Thing) SET n.score = 99 RETURN properties(n) AS props",
                Catalog::MissingScore,
                write_policy(),
            ),
            (
                "MATCH (n:Thing) SET n.score = 99 RETURN n{.*, failed:1 / 0} AS props",
                Catalog::Full,
                write_policy(),
            ),
            (
                "MATCH (n:Thing) SET n.score = 99 RETURN properties(n) AS props",
                Catalog::Full,
                GraphWriteProgramPolicy::new(
                    GqlQueryPolicy::new(100_000, 100_000, 1_000_000, 0),
                    100_000,
                    100_000,
                    100_000,
                ),
            ),
        ] {
            assert!(
                transaction
                    .query_write_engine(&mut db, &cx, statement, &params, catalog, R, budget,)
                    .is_err(),
                "{statement}"
            );
            assert_eq!(transaction.staged_effect_digest().unwrap(), digest);
            assert_eq!(db.frontier().unwrap(), before);
        }
        transaction.abort();
        assert_eq!(
            db.vertex(VId(1)).unwrap().unwrap().props[0].1,
            CanonicalScalar::Int(10).into()
        );
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn authorized_write_return_maps_exclude_hidden_fields_and_preserve_them_in_storage() {
    let ((), report) =
        run_async_under_lab(0x6d61_0009, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let (commit, cx, txcx) = (contexts.commit(), contexts.query(), contexts.txn());
            let issuer = Authority::new(
                AuthKey::from_seed(0x6d61_0011),
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
            grant.rights = Rights::ReadWrite;
            grant.labels = Scope::only([THING]);
            grant.relations = Scope::only([R]);
            grant.properties = Scope::only([SCORE, NOTE]);
            let token = issuer.issue_at(&grant, 100).unwrap();
            let params = GqlParameters::new();
            let prepared = PreparedGraphMutationQueryText::prepare(
                "MATCH (a:Thing)-[r:R]->(b:Thing) \
             SET a.score = a.score + 1, r.score = r.score + 2 \
             RETURN properties(a) AS node, r{.*, score:r.score * 10} AS edge, keys(a) AS keys",
                R,
                Catalog::Full,
            )
            .unwrap()
            .bind_parameters(&params)
            .unwrap();
            for hidden in [false, true] {
                let mut db = Database::open_memory(&commit, keys()).await.unwrap();
                seed(&mut db, &commit, hidden).await;
                let (_, result, _) = db
                    .execute_graph_mutation_query_authorized(
                        &txcx,
                        &cx,
                        &commit,
                        &issuer,
                        &token,
                        "main",
                        &prepared,
                        GraphMutationPolicy::new(policy(), 100_000),
                        || 100,
                    )
                    .await
                    .unwrap();
                assert_eq!(
                    result
                        .value
                        .iter()
                        .map(|row| row.values().to_vec())
                        .collect::<Vec<_>>(),
                    vec![vec![
                        map(&[("score", int(11))]),
                        map(&[
                            ("edge-note", GraphValue::Scalar(CanonicalScalar::Bool(true))),
                            ("score", int(50)),
                        ]),
                        list(&["score"]),
                    ]],
                );
                let props = db.vertex(VId(1)).unwrap().unwrap().props;
                assert_eq!(props[0], (SCORE, CanonicalScalar::Int(11).into()));
                assert_eq!(
                    props[1],
                    (NAME, CanonicalScalar::ucs_basic_text("one").unwrap().into())
                );
                assert_eq!(props.len(), if hidden { 22 } else { 2 });
                let before = (
                    db.frontier().unwrap(),
                    db.vertices().unwrap(),
                    db.edges().unwrap(),
                );
                let forbidden = PreparedGraphMutationQueryText::prepare(
                "MATCH (a:Thing) WHERE a.score = 11 SET a.name = 'changed' RETURN properties(a)",
                R, Catalog::Full,
            ).unwrap().bind_parameters(&params).unwrap();
                assert!(
                    db.execute_graph_mutation_query_authorized(
                        &txcx,
                        &cx,
                        &commit,
                        &issuer,
                        &token,
                        "main",
                        &forbidden,
                        GraphMutationPolicy::new(policy(), 100_000),
                        || 100,
                    )
                    .await
                    .is_err()
                );
                assert_eq!(
                    (
                        db.frontier().unwrap(),
                        db.vertices().unwrap(),
                        db.edges().unwrap()
                    ),
                    before
                );
            }
            assert_eq!(txcx.outstanding_obligations(), 0);
        });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn creation_return_maps_describe_the_durable_fields_before_projection_overrides() {
    let ((), report) = run_async_under_lab(0x6d61_0010, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, cx, txcx) = (contexts.commit(), contexts.query(), contexts.txn());
        for keyword in ["CREATE", "INSERT"] {
            let vfs = MemVfs::new().unwrap();
            let path = vfs.database_dir();
            let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
                .await
                .unwrap();
            let before = db.frontier().unwrap();
            let statement = format!(
                "{keyword} (a:Thing {{score:2,name:'created',`edge-note`:NULL}}),\
                 (b:Thing),(a)-[r:R {{score:3}}]->(b) \
                 RETURN a,b,r,properties(a) AS node,b{{.*}} AS empty,\
                 properties(r) AS edge,keys(a) AS names,a{{.*,score:a.score+1}} AS projected"
            );
            let returned = cells(
                db.query_write_engine(
                    &txcx,
                    &cx,
                    &commit,
                    &statement,
                    &GqlParameters::new(),
                    Catalog::Full,
                    R,
                    write_policy(),
                )
                .await
                .unwrap(),
            );
            let [row] = returned.as_slice() else {
                panic!("one creation occurrence")
            };
            let [
                GraphValue::Vertex(a),
                GraphValue::Vertex(b),
                GraphValue::Edge(r),
                ..,
            ] = row.as_slice()
            else {
                panic!("the maps retain their created element identities")
            };
            assert_eq!(
                &row[3..],
                &[
                    map(&[
                        ("edge-note", null()),
                        ("name", text("created")),
                        ("score", int(2))
                    ]),
                    map(&[]),
                    map(&[("score", int(3))]),
                    list(&["edge-note", "name", "score"]),
                    map(&[
                        ("edge-note", null()),
                        ("name", text("created")),
                        ("score", int(3))
                    ]),
                ],
            );
            let fields = vec![
                (SCORE, CanonicalScalar::Int(2).into()),
                (
                    NAME,
                    CanonicalScalar::ucs_basic_text("created").unwrap().into(),
                ),
                (NOTE, CanonicalScalar::Null.into()),
            ];
            assert_eq!(db.vertex(*a).unwrap().unwrap().props, fields);
            assert!(db.vertex(*b).unwrap().unwrap().props.is_empty());
            let edge = db.edge(*r).unwrap().unwrap();
            assert_eq!((edge.entry.src, edge.entry.dst), (*a, *b));
            assert_eq!(edge.props, vec![(SCORE, CanonicalScalar::Int(3).into())]);
            assert_eq!(db.frontier().unwrap().0, before.0 + 1);
            db.compact(&commit).await.unwrap();
            drop(db);
            let db = Database::open_with_vfs(&commit, vfs, &path, keys())
                .await
                .unwrap();
            assert_eq!(db.vertex(*a).unwrap().unwrap().props, fields);
            assert_eq!(db.edge(*r).unwrap().unwrap(), edge);
            assert_eq!(db.frontier().unwrap().0, before.0 + 1);
        }
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn insertion_return_maps_preserve_imported_rows_and_the_transaction_source() {
    let ((), report) = run_async_under_lab(0x6d61_0011, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, cx, txcx) = (contexts.commit(), contexts.query(), contexts.txn());
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let before = seed(&mut db, &commit, false).await;
        let mut transaction = db.begin(&txcx).unwrap();
        let mut prefix = WriteBatch::new(R);
        prefix.set_vertex_property(VId(1), SCORE, Some(CanonicalScalar::Int(20)));
        prefix.set_vertex_property(VId(1), NAME, None);
        prefix.set_edge_property(EId(1), SCORE, Some(CanonicalScalar::Int(4)));
        transaction.write(&mut db, prefix).unwrap();
        let result = transaction
            .query_write_engine(
                &mut db,
                &cx,
                "UNWIND [{bump:2},{bump:3}] AS input UNWIND [input.bump] AS bump \
                 MATCH (a:Thing)-[r:R]->(b:Thing) \
                 CREATE (c:Thing {score:a.score+bump}),\
                 (c)-[fresh:R {score:r.score}]->(a) \
                 RETURN properties(a) AS source,properties(r) AS old,\
                 properties(c) AS created,properties(fresh) AS fresh,\
                 properties(input) AS input ORDER BY created",
                &GqlParameters::new(),
                Catalog::Full,
                R,
                write_policy(),
            )
            .unwrap();
        assert_eq!(
            cells(result),
            [2, 3]
                .into_iter()
                .map(|bump| vec![
                    map(&[("score", int(20))]),
                    map(&[
                        ("edge-note", GraphValue::Scalar(CanonicalScalar::Bool(true))),
                        ("score", int(4))
                    ]),
                    map(&[("score", int(20 + bump))]),
                    map(&[("score", int(4))]),
                    map(&[("bump", int(bump))]),
                ])
                .collect::<Vec<_>>(),
        );
        assert_eq!(transaction.vertices(&db).unwrap().len(), 4);
        assert_eq!(transaction.edges(&db).unwrap().len(), 3);
        assert_eq!(db.vertices().unwrap().len(), 2);
        assert_eq!(db.edges().unwrap().len(), 1);
        assert_eq!(db.frontier().unwrap(), before);
        transaction.abort();
        assert_eq!(
            db.vertex(VId(1)).unwrap().unwrap().props[0].1,
            CanonicalScalar::Int(10).into()
        );
        assert_eq!(
            db.edge(EId(1)).unwrap().unwrap().props[0].1,
            CanonicalScalar::Int(3).into()
        );
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn insertion_map_catalog_and_late_return_failures_keep_prior_staging_intact() {
    let ((), report) = run_async_under_lab(0x6d61_0012, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, cx, txcx) = (contexts.commit(), contexts.query(), contexts.txn());
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let before = seed(&mut db, &commit, false).await;
        let mut transaction = db.begin(&txcx).unwrap();
        let mut prefix = WriteBatch::new(R);
        prefix.set_vertex_property(VId(1), SCORE, Some(CanonicalScalar::Int(20)));
        transaction.write(&mut db, prefix).unwrap();
        let digest = transaction.staged_effect_digest().unwrap();
        let vertices = transaction.vertices(&db).unwrap();
        let edges = transaction.edges(&db).unwrap();
        let params = GqlParameters::new();
        for catalog in [Catalog::MissingScore, Catalog::DuplicateName] {
            assert!(
                transaction
                    .query_write(
                        &mut db,
                        &cx,
                        "CREATE (n:Thing {score:1}) RETURN properties(n) AS props LIMIT 0",
                        &params,
                        catalog,
                        R,
                        write_policy(),
                        |_| -> Result<fgdb_delta_types::ElementId, ()> {
                            panic!("catalog admission must precede identity allocation")
                        },
                    )
                    .is_err()
            );
            assert_eq!(transaction.staged_effect_digest().unwrap(), digest);
        }
        for (statement, budget) in [
            (
                "UNWIND [1,0] AS value CREATE (n:Thing {score:value}) \
                 RETURN n{.*,inverse:10/n.score} AS props LIMIT 0",
                write_policy(),
            ),
            (
                "CREATE (n:Thing {score:1}) RETURN properties(n) AS props",
                GraphWriteProgramPolicy::new(
                    GqlQueryPolicy::new(100_000, 100_000, 1_000_000, 0),
                    100_000,
                    100_000,
                    100_000,
                ),
            ),
        ] {
            assert!(
                transaction
                    .query_write_engine(&mut db, &cx, statement, &params, Catalog::Full, R, budget)
                    .is_err(),
                "{statement}"
            );
            assert_eq!(transaction.staged_effect_digest().unwrap(), digest);
            assert_eq!(transaction.vertices(&db).unwrap(), vertices);
            assert_eq!(transaction.edges(&db).unwrap(), edges);
            assert_eq!(db.frontier().unwrap(), before);
        }
        transaction.abort();
        assert_eq!(
            db.vertex(VId(1)).unwrap().unwrap().props[0].1,
            CanonicalScalar::Int(10).into()
        );
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn merge_return_maps_follow_both_branches_and_the_final_set_clause() {
    let ((), report) = run_async_under_lab(0x6d61_0013, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, cx, txcx) = (contexts.commit(), contexts.query(), contexts.txn());
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let before = db.frontier().unwrap();
        let params = GqlParameters::new();
        for score in [1, 11] {
            let returned = db
                .query_write_engine(
                    &txcx,
                    &cx,
                    &commit,
                    "MERGE (n:Thing {name:'merge-key'}) \
                     ON CREATE SET n.score=1 ON MATCH SET n.score=n.score+10 \
                     SET n.`edge-note`=n.score+1 \
                     RETURN properties(n) AS props,n{.*,score:n.score+100} AS view,\
                     keys(n) AS names,count(*) AS hits",
                    &params,
                    Catalog::Full,
                    R,
                    write_policy(),
                )
                .await
                .unwrap();
            assert_eq!(
                cells(returned),
                vec![vec![
                    map(&[
                        ("edge-note", int(score + 1)),
                        ("name", text("merge-key")),
                        ("score", int(score))
                    ]),
                    map(&[
                        ("edge-note", int(score + 1)),
                        ("name", text("merge-key")),
                        ("score", int(score + 100))
                    ]),
                    list(&["edge-note", "name", "score"]),
                    int(1),
                ]],
            );
            let vertices = db.vertices().unwrap();
            let [vertex] = vertices.as_slice() else {
                panic!("both MERGE branches choose the same vertex")
            };
            assert_eq!(
                vertex.props,
                vec![
                    (SCORE, CanonicalScalar::Int(score).into()),
                    (
                        NAME,
                        CanonicalScalar::ucs_basic_text("merge-key").unwrap().into()
                    ),
                    (NOTE, CanonicalScalar::Int(score + 1).into()),
                ],
            );
        }
        let returned = db
            .query_write_engine(
                &txcx,
                &cx,
                &commit,
                "MERGE (n:Thing {name:'merge-key'}) SET n.score=22 \
                 RETURN properties(n) AS props LIMIT 0",
                &params,
                Catalog::Full,
                R,
                write_policy(),
            )
            .await
            .unwrap();
        assert!(cells(returned).is_empty());
        assert_eq!(db.frontier().unwrap().0, before.0 + 3);
        db.compact(&commit).await.unwrap();
        drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        assert_eq!(
            cells(
                db.query(
                    &cx,
                    "MATCH (n:Thing) RETURN properties(n) AS props",
                    &params,
                    Catalog::Full,
                    policy()
                )
                .unwrap()
            ),
            vec![vec![map(&[
                ("edge-note", int(12)),
                ("name", text("merge-key")),
                ("score", int(22))
            ])]],
        );
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn merge_map_return_reads_prior_overlay_and_rolls_back_every_failing_clause() {
    let ((), report) = run_async_under_lab(0x6d61_0014, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, cx, txcx) = (contexts.commit(), contexts.query(), contexts.txn());
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let before = seed(&mut db, &commit, false).await;
        let params = GqlParameters::new();
        let mut transaction = db.begin(&txcx).unwrap();
        let mut prefix = WriteBatch::new(R);
        prefix.set_vertex_property(VId(1), NAME, None);
        prefix.set_vertex_property(VId(1), NOTE, Some(CanonicalScalar::Int(5)));
        transaction.write(&mut db, prefix).unwrap();
        let result = transaction
            .query_write_engine(
                &mut db,
                &cx,
                "MERGE (n:Thing {score:10}) ON MATCH SET n.`edge-note`=n.`edge-note`+1 \
                 RETURN properties(n) AS props,n{.name,.score} AS selected",
                &params,
                Catalog::Full,
                R,
                write_policy(),
            )
            .unwrap();
        assert_eq!(
            cells(result),
            vec![vec![
                map(&[("edge-note", int(6)), ("score", int(10))]),
                map(&[("name", null()), ("score", int(10))])
            ]],
        );
        let digest = transaction.staged_effect_digest().unwrap();
        let vertices = transaction.vertices(&db).unwrap();
        for (statement, catalog, budget) in [
            (
                "MERGE (n:Thing {score:10}) SET n.`edge-note`=99 RETURN properties(n) AS props LIMIT 0",
                Catalog::MissingScore,
                write_policy(),
            ),
            (
                "MERGE (n:Thing {score:10}) SET n.`edge-note`=99 RETURN properties(n) AS props",
                Catalog::DuplicateName,
                write_policy(),
            ),
            (
                "MERGE (n:Thing {score:10}) SET n.`edge-note`=99 RETURN n{.*,failed:1/0} AS props LIMIT 0",
                Catalog::Full,
                write_policy(),
            ),
            (
                "MERGE (n:Thing {score:999}) ON CREATE SET n.`edge-note`=99 RETURN n{.*,failed:1/0} AS props",
                Catalog::Full,
                write_policy(),
            ),
            (
                "MERGE (n:Thing {score:10}) SET n.`edge-note`=99 RETURN properties(n) AS props",
                Catalog::Full,
                GraphWriteProgramPolicy::new(
                    GqlQueryPolicy::new(100_000, 100_000, 1_000_000, 0),
                    100_000,
                    100_000,
                    100_000,
                ),
            ),
        ] {
            assert!(
                transaction
                    .query_write_engine(&mut db, &cx, statement, &params, catalog, R, budget)
                    .is_err(),
                "{statement}"
            );
            assert_eq!(transaction.staged_effect_digest().unwrap(), digest);
            assert_eq!(transaction.vertices(&db).unwrap(), vertices);
            assert_eq!(db.frontier().unwrap(), before);
        }
        transaction.abort();
        assert_eq!(
            db.vertex(VId(1)).unwrap().unwrap().props,
            vec![
                (SCORE, CanonicalScalar::Int(10).into()),
                (NAME, CanonicalScalar::ucs_basic_text("one").unwrap().into())
            ]
        );
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn authorized_merge_maps_mask_final_fields_and_preserve_hidden_storage() {
    let ((), report) = run_async_under_lab(0x6d61_0015, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, cx, txcx) = (contexts.commit(), contexts.query(), contexts.txn());
        let issuer = Authority::new(
            AuthKey::from_seed(0x6d61_0021),
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
        grant.rights = Rights::ReadWrite;
        grant.labels = Scope::only([THING]);
        grant.relations = Scope::only([R]);
        grant.properties = Scope::only([SCORE, NOTE]);
        let token = issuer.issue_at(&grant, 100).unwrap();
        let params = GqlParameters::new();
        let statement = PreparedGraphVertexUpsertQueryText::prepare(
            "MERGE (n:Thing {score:10}) ON MATCH SET n.score=n.score+1 SET n.`edge-note`=NULL \
             RETURN properties(n) AS props,keys(n) AS names,n{.*,score:n.score*10} AS view",
            R,
            Catalog::Full,
        )
        .unwrap()
        .bind_parameters(&params)
        .unwrap();
        let forbidden = PreparedGraphVertexUpsertQueryText::prepare(
            "MERGE (n:Thing {score:11}) SET n.name='changed' RETURN properties(n) AS props",
            R,
            Catalog::Full,
        )
        .unwrap()
        .bind_parameters(&params)
        .unwrap();
        for hidden in [false, true] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let before = seed(&mut db, &commit, hidden).await;
            let (_, _, returned, _) = db
                .execute_graph_vertex_upsert_query_authorized(
                    &txcx,
                    &cx,
                    &commit,
                    &issuer,
                    &token,
                    "main",
                    &statement,
                    upsert_policy(),
                    || 100,
                )
                .await
                .unwrap();
            assert_eq!(
                returned
                    .value
                    .iter()
                    .map(|row| row.values().to_vec())
                    .collect::<Vec<_>>(),
                vec![vec![
                    map(&[("edge-note", null()), ("score", int(11))]),
                    list(&["edge-note", "score"]),
                    map(&[("edge-note", null()), ("score", int(110))]),
                ]]
            );
            let props = db.vertex(VId(1)).unwrap().unwrap().props;
            assert!(
                props.contains(&(NAME, CanonicalScalar::ucs_basic_text("one").unwrap().into()))
            );
            assert!(props.contains(&(NOTE, CanonicalScalar::Null.into())));
            assert_eq!(props.len(), if hidden { 23 } else { 3 });
            assert_eq!(db.frontier().unwrap().0, before.0 + 1);
            let after = (
                db.frontier().unwrap(),
                db.vertices().unwrap(),
                db.edges().unwrap(),
            );
            assert!(
                db.execute_graph_vertex_upsert_query_authorized(
                    &txcx,
                    &cx,
                    &commit,
                    &issuer,
                    &token,
                    "main",
                    &forbidden,
                    upsert_policy(),
                    || 100,
                )
                .await
                .is_err()
            );
            assert_eq!(
                (
                    db.frontier().unwrap(),
                    db.vertices().unwrap(),
                    db.edges().unwrap()
                ),
                after
            );
        }
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

// fgdb-iufb7: MERGE ... RETURN labels(n) reads the chosen vertex after every
// clause (TCK Merge1 [2], Merge2 [1], Merge3 [2]), named in identity order.
#[test]
fn merge_return_labels_follow_both_branches_and_the_prior_overlay() {
    let ((), report) = run_async_under_lab(0x6d61_0016, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, cx, txcx) = (contexts.commit(), contexts.query(), contexts.txn());
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let params = GqlParameters::new();
        let statement = "MERGE (n:Thing {name:'merge-key'}) ON CREATE SET n:Foo ON MATCH SET n:Bar \
                         RETURN labels(n) AS labels,n.name AS name";
        // ON CREATE adds Foo to the MERGE label, then ON MATCH adds Bar. The
        // list is in identity order (Thing, Foo, Bar), not name order.
        for expected in [&["Thing", "Foo"][..], &["Thing", "Foo", "Bar"][..]] {
            let returned = db
                .query_write_engine(
                    &txcx,
                    &cx,
                    &commit,
                    statement,
                    &params,
                    Catalog::Full,
                    R,
                    write_policy(),
                )
                .await
                .unwrap();
            assert_eq!(
                cells(returned),
                vec![vec![list(expected), text("merge-key")]]
            );
        }
        // The returned list equals a later read of the committed vertex.
        assert_eq!(
            cells(
                db.query(
                    &cx,
                    "MATCH (n:Thing) RETURN labels(n) AS labels",
                    &params,
                    Catalog::Full,
                    policy()
                )
                .unwrap()
            ),
            vec![vec![list(&["Thing", "Foo", "Bar"])]],
        );
        let vid = db.vertices().unwrap()[0].vid;
        // RETURN reads the transaction's earlier label edits too.
        let mut transaction = db.begin(&txcx).unwrap();
        let mut prefix = WriteBatch::new(R);
        prefix.set_vertex_label(vid, FOO, false);
        transaction.write(&mut db, prefix).unwrap();
        let returned = transaction
            .query_write_engine(
                &mut db,
                &cx,
                "MERGE (n:Thing {name:'merge-key'}) RETURN labels(n) AS labels",
                &params,
                Catalog::Full,
                R,
                write_policy(),
            )
            .unwrap();
        assert_eq!(cells(returned), vec![vec![list(&["Thing", "Bar"])]]);
        transaction.abort();

        // A label the host cannot name refuses; it is never omitted, and the
        // refused statement stages nothing.
        let mut unnamed = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(
            VId(1),
            vec![THING, LabelId(99)],
            vec![(NAME, CanonicalScalar::ucs_basic_text("unnamed").unwrap())],
        );
        let before = unnamed.write(&commit, seed).await.unwrap();
        let mut transaction = unnamed.begin(&txcx).unwrap();
        let digest = transaction.staged_effect_digest().unwrap();
        let Err(error) = transaction.query_write_engine(
            &mut unnamed,
            &cx,
            "MERGE (n:Thing {name:'unnamed'}) ON MATCH SET n:Foo RETURN labels(n) AS labels",
            &params,
            Catalog::Full,
            R,
            write_policy(),
        ) else {
            panic!("an unnamed label refuses")
        };
        assert!(
            format!("{error:?}").contains("UnmappedLabel(LabelId(99))"),
            "{error:?}"
        );
        assert_eq!(transaction.staged_effect_digest().unwrap(), digest);
        transaction.abort();
        assert_eq!(unnamed.frontier().unwrap(), before);
        assert_eq!(
            unnamed.vertex(VId(1)).unwrap().unwrap().labels,
            vec![THING, LabelId(99)]
        );
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn authorized_merge_labels_omit_hidden_labels_and_preserve_them_in_storage() {
    let ((), report) = run_async_under_lab(0x6d61_0017, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, cx, txcx) = (contexts.commit(), contexts.query(), contexts.txn());
        let issuer = Authority::new(
            AuthKey::from_seed(0x6d61_0022),
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
        grant.rights = Rights::ReadWrite;
        grant.labels = Scope::only([THING, FOO]);
        grant.relations = Scope::only([R]);
        grant.properties = Scope::only([NAME]);
        let token = issuer.issue_at(&grant, 100).unwrap();
        let params = GqlParameters::new();
        let statement = PreparedGraphVertexUpsertQueryText::prepare(
            "MERGE (n:Thing {name:'one'}) ON MATCH SET n:Foo RETURN labels(n) AS labels",
            R,
            Catalog::Full,
        )
        .unwrap()
        .bind_parameters(&params)
        .unwrap();
        let mut observed = Vec::new();
        for hidden in [false, true] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            // Bar is named but outside the grant; LabelId(99) is unnamed.
            // Neither may be named, refuse the statement, or be removed.
            let mut labels = vec![THING];
            if hidden {
                labels.extend([BAR, LabelId(99)]);
            }
            let mut seed = WriteBatch::new(R);
            seed.create_vertex(
                VId(1),
                labels,
                vec![(NAME, CanonicalScalar::ucs_basic_text("one").unwrap())],
            );
            db.write(&commit, seed).await.unwrap();
            let (_, _, returned, _) = db
                .execute_graph_vertex_upsert_query_authorized(
                    &txcx,
                    &cx,
                    &commit,
                    &issuer,
                    &token,
                    "main",
                    &statement,
                    upsert_policy(),
                    || 100,
                )
                .await
                .unwrap();
            let rows: Vec<_> = returned
                .value
                .iter()
                .map(|row| row.values().to_vec())
                .collect();
            assert_eq!(rows, vec![vec![list(&["Thing", "Foo"])]]);
            let mut stored = vec![THING, FOO];
            if hidden {
                stored.extend([BAR, LabelId(99)]);
            }
            assert_eq!(db.vertex(VId(1)).unwrap().unwrap().labels, stored);
            observed.push(rows);
        }
        assert_eq!(observed[0], observed[1]);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
