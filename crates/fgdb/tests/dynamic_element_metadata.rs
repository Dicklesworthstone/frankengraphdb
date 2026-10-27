//! Dynamic element metadata must keep the native snapshot, staged overlay,
//! catalog, authorization and read-witness boundaries (fgdb-a1bx3).

use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb::{
    Database, DatabaseKeys, MemVfs, PreparedNativeRead, QueryError, QueryResult, QueryValue,
    WriteBatch, WriteError, WriteTxnError,
};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId, SchemaEpoch};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GraphIntegerErrorKind, GraphSetExecutionError,
    GraphSymbol, GraphSymbolKind, GraphSymbolResolver, GraphWriteProgramPolicy,
    ReverseSymbolCatalog,
};
use fgdb_types::{
    CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId,
};
use fgdb_warden::{Authority, Grant, QueryLimits, Scope};

const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0xa1; 32]);
const THING: LabelId = LabelId(1);
const EXTRA: LabelId = LabelId(2);
const HIDDEN_LABEL: LabelId = LabelId(99);
const R: RelationId = RelationId(1);
const SCORE: PropertyKeyId = PropertyKeyId(1);
const NAME: PropertyKeyId = PropertyKeyId(2);
const NOTE: PropertyKeyId = PropertyKeyId(3);
const HIDDEN_PROPERTY: PropertyKeyId = PropertyKeyId(99);

#[derive(Clone, Copy)]
struct Catalog;

impl GraphSymbolResolver for Catalog {
    fn resolve_symbol(&mut self, kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        match (kind, name) {
            (GraphSymbolKind::Label, "Thing") => Some(GraphSymbol::Label(THING)),
            (GraphSymbolKind::Label, "Extra") => Some(GraphSymbol::Label(EXTRA)),
            (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
            (GraphSymbolKind::Property, "score") => Some(GraphSymbol::Property(SCORE)),
            (GraphSymbolKind::Property, "name") => Some(GraphSymbol::Property(NAME)),
            (GraphSymbolKind::Property, "note") => Some(GraphSymbol::Property(NOTE)),
            _ => None,
        }
    }

    fn reverse_catalog(&self) -> Option<ReverseSymbolCatalog> {
        let mut catalog = ReverseSymbolCatalog::new();
        catalog.insert_label(THING, "Thing");
        catalog.insert_label(EXTRA, "Extra");
        catalog.insert_relation(R, "R");
        catalog.insert_property(SCORE, "score");
        catalog.insert_property(NAME, "name");
        catalog.insert_property(NOTE, "note");
        // Hidden identities intentionally have no names. Masking must happen
        // before any reverse-catalog lookup, including inside comprehensions.
        Some(catalog)
    }
}

fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0xa2; 32], NS, [0xa3; 32])
}

fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100_000, 1_000_000, 1_000_000)
}

fn integer(value: i64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(value))
}

fn text(value: &str) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::ucs_basic_text(value).unwrap())
}

fn list(values: Vec<GraphValue>) -> GraphValue {
    GraphValue::List(values.into_boxed_slice())
}

fn names(values: &[&str]) -> GraphValue {
    list(values.iter().map(|value| text(value)).collect())
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

fn cells(result: QueryResult) -> Vec<Vec<GraphValue>> {
    let QueryResult::Rows { rows, .. } = result else {
        panic!("expected native result rows")
    };
    rows.into_iter()
        .map(|row| {
            row.into_iter()
                .map(|value| match value {
                    QueryValue::Value(value) => value,
                    QueryValue::Count(value) => integer(i64::try_from(value).unwrap()),
                    QueryValue::Integer(value) => integer(i64::try_from(value).unwrap()),
                    value => panic!("unexpected metadata result {value:?}"),
                })
                .collect()
        })
        .collect()
}

async fn seed(db: &mut Database<MemVfs>, cx: &CommitCx, hidden: bool) {
    let mut batch = WriteBatch::new(R);
    for (id, name, score) in [(1, "one", 10), (2, "two", 20)] {
        let mut labels = vec![THING];
        let mut properties = vec![
            (SCORE, CanonicalScalar::Int(score)),
            (NAME, CanonicalScalar::ucs_basic_text(name).unwrap()),
        ];
        if id == 1 {
            labels.push(EXTRA);
        }
        if hidden {
            labels.push(HIDDEN_LABEL);
            properties.push((HIDDEN_PROPERTY, CanonicalScalar::Int(999)));
        }
        batch.create_vertex(VId(id), labels, properties);
    }
    let mut edge_properties = vec![(SCORE, CanonicalScalar::Int(3))];
    if hidden {
        edge_properties.push((HIDDEN_PROPERTY, CanonicalScalar::Int(999)));
        batch.create_vertex(VId(3), vec![HIDDEN_LABEL], vec![]);
        // Both source-only and destination-only visibility are insufficient.
        batch.add_edge(EId(2), VId(1), VId(3), vec![]);
        batch.add_edge(EId(3), VId(3), VId(2), vec![]);
    }
    batch.add_edge(EId(1), VId(1), VId(2), edge_properties);
    db.write(cx, batch).await.unwrap();
}

#[test]
fn native_metadata_matches_bound_forms_for_unwind_subscripts_and_path_locals() {
    let ((), report) = run_async_under_lab(0xa1b3_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, cx) = (contexts.commit(), contexts.query());
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit, false).await;
        let parameters = GqlParameters::new();
        let node_bound = "MATCH (n:Thing) RETURN n.score AS score, labels(n) AS labels, \
                          keys(n) AS keys, properties(n) AS props ORDER BY score";
        let edge_bound = "MATCH ()-[r:R]->() RETURN type(r) AS kind, \
                          keys(r) AS keys, properties(r) AS props";
        for (bound, dynamic) in [
            (
                node_bound,
                "MATCH (n:Thing) WITH collect(n) AS ns UNWIND ns AS x \
                 RETURN x.score AS score, labels(x) AS labels, keys(x) AS keys, \
                 properties(x) AS props ORDER BY score",
            ),
            (
                node_bound,
                "MATCH p = (a)-[:R]->(b) UNWIND nodes(p) AS x \
                 RETURN x.score AS score, labels(x) AS labels, keys(x) AS keys, \
                 properties(x) AS props ORDER BY score",
            ),
            (
                edge_bound,
                "MATCH p = (a)-[r:R]->(b) UNWIND relationships(p) AS x \
                 RETURN type(x) AS kind, keys(x) AS keys, properties(x) AS props",
            ),
            (
                edge_bound,
                "MATCH ()-[r:R]->() WITH collect(r) AS rs \
                 RETURN type(rs[0]) AS kind, keys(rs[0]) AS keys, properties(rs[0]) AS props",
            ),
            (
                "MATCH (a)-[r:R]->(b) RETURN [labels(a), labels(b)] AS labels, \
                 [type(r)] AS kinds, [properties(a), properties(b)] AS nodes, \
                 [keys(r)] AS edge_keys",
                "MATCH p = (a)-[r:R]->(b) RETURN [n IN nodes(p) | labels(n)] AS labels, \
                 [e IN relationships(p) | type(e)] AS kinds, \
                 [n IN nodes(p) | properties(n)] AS nodes, \
                 [e IN relationships(p) | keys(e)] AS edge_keys",
            ),
            (
                "MATCH (n:Thing {score:10}) RETURN labels(n) AS labels, \
                 properties(n) AS props, keys(n) AS keys",
                "MATCH (n:Thing {score:10}) WITH collect(n) AS ns \
                 RETURN labels(ns[0]) AS labels, properties(ns[0]) AS props, keys(ns[0]) AS keys",
            ),
        ] {
            let expected = cells(
                db.query(&cx, bound, &parameters, Catalog, policy())
                    .unwrap(),
            );
            let prepared = PreparedNativeRead::prepare(dynamic, &parameters, Catalog)
                .unwrap_or_else(|error| panic!("{dynamic}: {error:?}"));
            assert_eq!(
                cells(prepared.execute(&db, &cx, &parameters, policy()).unwrap()),
                expected,
                "{dynamic}"
            );
        }
        // Names follow stable label identity order, while property keys follow
        // the canonical map order. Pin values as well as the differential.
        assert_eq!(
            cells(
                db.query(&cx, node_bound, &parameters, Catalog, policy())
                    .unwrap()
            ),
            vec![
                vec![
                    integer(10),
                    names(&["Thing", "Extra"]),
                    names(&["name", "score"]),
                    map(&[("name", text("one")), ("score", integer(10))]),
                ],
                vec![
                    integer(20),
                    names(&["Thing"]),
                    names(&["name", "score"]),
                    map(&[("name", text("two")), ("score", integer(20))]),
                ],
            ]
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn native_metadata_preserves_null_typed_failures_and_lazy_branch_selection() {
    let ((), report) = run_async_under_lab(0xa1b3_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, cx) = (contexts.commit(), contexts.query());
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit, false).await;
        let parameters = GqlParameters::new();
        let null = GraphValue::Scalar(CanonicalScalar::Null);
        assert_eq!(
            cells(
                db.query(
                    &cx,
                    "UNWIND [NULL] AS x RETURN labels(x) AS l, type(x) AS t, \
                 keys(x) AS k, properties(x) AS p",
                    &parameters,
                    Catalog,
                    policy(),
                )
                .unwrap()
            ),
            vec![vec![null; 4]],
        );
        for statement in [
            "UNWIND [42] AS x RETURN labels(x) AS bad",
            "UNWIND [42] AS x RETURN properties(x) AS bad",
            "MATCH (n:Thing) WITH collect(n) AS ns RETURN type(ns[0]) AS bad",
            "MATCH ()-[r:R]->() WITH collect(r) AS rs RETURN labels(rs[0]) AS bad",
        ] {
            let error = db
                .query(&cx, statement, &parameters, Catalog, policy())
                .unwrap_err();
            assert!(
                matches!(&error, QueryError::Set(GqlQueryError::Source(
                    GraphSetExecutionError::Projection { error, .. }
                )) if error.kind == GraphIntegerErrorKind::IncompatibleOperands),
                "{statement}: {error:?}"
            );
        }
        assert_eq!(
            cells(
                db.query(
                    &cx,
                    "MATCH (n:Thing {score:10}) WITH collect(n) AS ns RETURN \
                 CASE WHEN false THEN type(ns[0]) ELSE labels(ns[0]) END AS labels, \
                 CASE WHEN true THEN properties(ns[0]) ELSE labels(42) END AS props, \
                 coalesce('kept', type(ns[0])) AS kept",
                    &parameters,
                    Catalog,
                    policy(),
                )
                .unwrap()
            ),
            vec![vec![
                names(&["Thing", "Extra"]),
                map(&[("name", text("one")), ("score", integer(10))]),
                text("kept"),
            ]],
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

const PATH_METADATA: &str = "MATCH p = (a)-[r:R]->(b) RETURN \
    [n IN nodes(p) | labels(n)] AS labels, \
    [n IN nodes(p) | properties(n)] AS nodes, \
    [e IN relationships(p) | properties(e)] AS edges";

#[test]
fn native_metadata_reads_staging_and_final_write_rows_without_moving_old_snapshots() {
    let ((), report) = run_async_under_lab(0xa1b3_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, cx, txcx) = (contexts.commit(), contexts.query(), contexts.txn());
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        seed(&mut db, &commit, false).await;
        let parameters = GqlParameters::new();
        let pinned = db.read_session().unwrap();
        let before = cells(
            db.query(&cx, PATH_METADATA, &parameters, Catalog, policy())
                .unwrap(),
        );
        let prepared = PreparedNativeRead::prepare(PATH_METADATA, &parameters, Catalog).unwrap();
        let mut transaction = db.begin(&txcx).unwrap();
        let mut edit = WriteBatch::new(R);
        edit.set_vertex_label(VId(1), EXTRA, false);
        edit.set_vertex_label(VId(2), EXTRA, true);
        edit.set_vertex_property(VId(1), NOTE, Some(CanonicalScalar::Int(7)));
        edit.set_edge_property(EId(1), SCORE, Some(CanonicalScalar::Int(9)));
        transaction.write(&mut db, edit).unwrap();
        let staged = vec![vec![
            list(vec![names(&["Thing"]), names(&["Thing", "Extra"])]),
            list(vec![
                map(&[
                    ("name", text("one")),
                    ("note", integer(7)),
                    ("score", integer(10)),
                ]),
                map(&[("name", text("two")), ("score", integer(20))]),
            ]),
            list(vec![map(&[("score", integer(9))])]),
        ]];
        assert_eq!(
            cells(
                prepared
                    .execute_in_transaction(&transaction, &db, &cx, &parameters, policy())
                    .unwrap()
            ),
            staged,
        );
        assert_eq!(
            cells(prepared.execute(&db, &cx, &parameters, policy()).unwrap()),
            before
        );
        let returned = transaction
            .query_write_engine(
                &mut db,
                &cx,
                "MATCH (a)-[r:R]->(b) SET a.score = 11, r.note = 8 \
             RETURN [n IN [a,b] | labels(n)] AS labels, \
             [n IN [a,b] | properties(n)] AS nodes, \
             [e IN [r] | properties(e)] AS edges",
                &parameters,
                Catalog,
                R,
                GraphWriteProgramPolicy::new(policy(), 100_000, 100_000, 100_000),
            )
            .unwrap();
        let after = vec![vec![
            list(vec![names(&["Thing"]), names(&["Thing", "Extra"])]),
            list(vec![
                map(&[
                    ("name", text("one")),
                    ("note", integer(7)),
                    ("score", integer(11)),
                ]),
                map(&[("name", text("two")), ("score", integer(20))]),
            ]),
            list(vec![map(&[("note", integer(8)), ("score", integer(9))])]),
        ]];
        assert_eq!(cells(returned), after);
        transaction.commit(&mut db, &commit).await.unwrap();
        db.compact(&commit).await.unwrap();
        drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        assert_eq!(
            cells(prepared.execute(&db, &cx, &parameters, policy()).unwrap()),
            after
        );
        assert_eq!(
            cells(
                pinned
                    .query(&cx, PATH_METADATA, &parameters, Catalog, policy())
                    .unwrap()
            ),
            before,
        );
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn authorized_dynamic_metadata_masks_unnamed_fields_and_both_edge_endpoints() {
    let ((), report) =
        run_async_under_lab(0xa1b3_0004, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let (commit, cx) = (contexts.commit(), contexts.query());
            let mut hidden = Database::open_memory(&commit, keys()).await.unwrap();
            let mut removed = Database::open_memory(&commit, keys()).await.unwrap();
            seed(&mut hidden, &commit, true).await;
            seed(&mut removed, &commit, false).await;
            let authority =
                Authority::new(AuthKey::from_seed(0xa1b3), NS, "graph", SchemaEpoch(0), 1).unwrap();
            let mut grant = Grant::read_only(
                "main",
                1_000,
                QueryLimits {
                    max_nodes: 100_000,
                    max_work: 10_000_000,
                    max_rows: 100_000,
                },
            );
            grant.labels = Scope::only([THING, EXTRA]);
            grant.relations = Scope::only([R]);
            grant.properties = Scope::only([SCORE, NAME, NOTE]);
            let token = authority.issue_at(&grant, 100).unwrap();
            let parameters = GqlParameters::new();
            for statement in [
                PATH_METADATA,
                "MATCH (n:Thing) WITH collect(n) AS ns UNWIND ns AS x \
             RETURN x.score AS score, labels(x) AS labels, properties(x) AS props, \
             keys(x) AS keys ORDER BY score",
                "MATCH ()-[r:R]->() WITH collect(r) AS rs UNWIND rs AS x \
             RETURN type(x) AS kind, properties(x) AS props, keys(x) AS keys",
            ] {
                let expected = cells(
                    removed
                        .query(&cx, statement, &parameters, Catalog, policy())
                        .unwrap(),
                );
                let authorized = hidden
                    .query_authorized(
                        &cx,
                        &authority,
                        &token,
                        "main",
                        statement,
                        &parameters,
                        Catalog,
                        policy(),
                        || 100,
                    )
                    .unwrap();
                assert_eq!(cells(authorized), expected, "{statement}");
            }
            // Caller-known identities also enter as values without a MATCH.
            // These requests exercise the metadata host's own visibility
            // check, including both endpoints of an otherwise visible edge.
            let null_row = vec![GraphValue::Scalar(CanonicalScalar::Null); 3];
            for (identities, statement, mut expected) in [
                (
                    vec![
                        GraphValue::Vertex(VId(1)),
                        GraphValue::Vertex(VId(3)),
                        GraphValue::Vertex(VId(888)),
                    ],
                    "UNWIND $ids AS x RETURN labels(x) AS labels, keys(x) AS keys, \
                     properties(x) AS props",
                    vec![
                        vec![
                            names(&["Thing", "Extra"]),
                            names(&["name", "score"]),
                            map(&[("name", text("one")), ("score", integer(10))]),
                        ],
                        null_row.clone(),
                        null_row.clone(),
                    ],
                ),
                (
                    vec![
                        GraphValue::Edge(EId(1)),
                        GraphValue::Edge(EId(2)),
                        GraphValue::Edge(EId(3)),
                        GraphValue::Edge(EId(888)),
                    ],
                    "UNWIND $ids AS x RETURN type(x) AS kind, keys(x) AS keys, \
                     properties(x) AS props",
                    vec![
                        vec![text("R"), names(&["score"]), map(&[("score", integer(3))])],
                        null_row.clone(),
                        null_row.clone(),
                        null_row.clone(),
                    ],
                ),
            ] {
                let parameters = GqlParameters::new().with_list("ids", identities).unwrap();
                let mut actual = cells(
                    hidden
                        .query_authorized(
                            &cx,
                            &authority,
                            &token,
                            "main",
                            statement,
                            &parameters,
                            Catalog,
                            policy(),
                            || 100,
                        )
                        .unwrap(),
                );
                actual.sort();
                expected.sort();
                assert_eq!(actual, expected, "{statement}");
            }
            // The same unmasked request cannot silently discard unmapped visible
            // metadata; successful authorized execution above depends on masking.
            assert!(hidden.query(
            &cx,
            "MATCH (n:Thing) WITH collect(n) AS ns RETURN [x IN ns | labels(x)] AS labels",
            &parameters, Catalog, policy(),
        ).is_err());
        });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn dynamic_metadata_read_witnesses_cover_new_properties_and_label_membership() {
    let ((), report) = run_async_under_lab(0xa1b3_0005, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, cx, txcx) = (contexts.commit(), contexts.query(), contexts.txn());
        for labels in [false, true] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            seed(&mut db, &commit, false).await;
            let mut transaction = db.begin(&txcx).unwrap();
            let mut staged = WriteBatch::new(R);
            staged.set_vertex_property(VId(2), SCORE, Some(CanonicalScalar::Int(200)));
            transaction.write(&mut db, staged).unwrap();
            let statement = if labels {
                "MATCH (n:Thing {score:10}) WITH collect(n) AS ns RETURN labels(ns[0]) AS metadata"
            } else {
                "MATCH (n:Thing {score:10}) WITH collect(n) AS ns RETURN properties(ns[0]) AS metadata"
            };
            let rows = cells(
                transaction
                    .query(
                        &db,
                        &cx,
                        statement,
                        &GqlParameters::new(),
                        Catalog,
                        policy(),
                    )
                    .unwrap(),
            );
            assert_eq!(rows.len(), 1);
            // No write/write overlap: only VId(2) is staged. VId(1)'s new
            // property was absent from the observed map, so per-existing-key
            // witnesses alone would miss this change.
            let mut competing = WriteBatch::new(R);
            if labels {
                competing.set_vertex_label(VId(1), EXTRA, false);
            } else {
                competing.set_vertex_property(VId(1), NOTE, Some(CanonicalScalar::Int(44)));
            }
            db.write(&commit, competing).await.unwrap();
            assert!(matches!(
                transaction.commit(&mut db, &commit).await,
                Err(WriteTxnError::Write(WriteError::FirstCommitterWins { .. }))
            ));
            assert_eq!(txcx.outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
