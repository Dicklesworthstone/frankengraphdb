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

mod transactional_metadata {
    use super::*;
    use fgdb::{WriteError, WriteTxnError};

    const NODES: &str = "UNWIND $refs AS x RETURN x AS id,labels(x) AS labels,\
                        properties(x) AS props,keys(x) AS names ORDER BY id";
    const EDGES: &str = "UNWIND $refs AS x RETURN x AS id,type(x) AS kind,\
                        properties(x) AS props,keys(x) AS names ORDER BY id";

    #[test]
    fn point_metadata_uses_the_staged_basis_across_savepoints_commit_and_reopen() {
        let ((), report) = run_async_under_lab(0x6d71_0001, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let (commit, cx, txcx) = (contexts.commit(), contexts.query(), contexts.txn());
            let vfs = MemVfs::new().unwrap();
            let path = vfs.database_dir();
            let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
                .await
                .unwrap();
            seed(&mut db, &commit, false).await;
            let pinned = db.read_session().unwrap();
            let nodes = GqlParameters::new()
                .with_list(
                    "refs",
                    vec![
                        GraphValue::Vertex(VId(1)),
                        GraphValue::Vertex(VId(u128::MAX)),
                    ],
                )
                .unwrap();
            let edges = GqlParameters::new()
                .with_list(
                    "refs",
                    vec![GraphValue::Edge(EId(1)), GraphValue::Edge(EId(u128::MAX))],
                )
                .unwrap();
            let old_nodes = cells(
                db.query(&cx, NODES, &nodes, Catalog::Full, policy())
                    .unwrap(),
            );
            let old_edges = cells(
                db.query(&cx, EDGES, &edges, Catalog::Full, policy())
                    .unwrap(),
            );
            let mut txn = db.begin(&txcx).unwrap();
            let mut prefix = WriteBatch::new(R);
            prefix.set_vertex_property(VId(1), SCORE, Some(CanonicalScalar::Int(20)));
            prefix.set_vertex_property(VId(1), NAME, None);
            prefix.set_vertex_property(
                VId(1),
                NOTE,
                Some(CanonicalScalar::ucs_basic_text("staged").unwrap()),
            );
            prefix.set_vertex_label(VId(1), THING, false);
            prefix.set_vertex_label(VId(1), FOO, true);
            prefix.create_vertex(
                VId(u128::MAX),
                vec![THING, BAR],
                vec![(SCORE, CanonicalScalar::Int(30))],
            );
            prefix.set_edge_property(EId(1), NOTE, None);
            prefix.set_edge_property(
                EId(1),
                NAME,
                Some(CanonicalScalar::ucs_basic_text("edge-stage").unwrap()),
            );
            prefix.add_edge(
                EId(u128::MAX),
                VId(u128::MAX),
                VId(2),
                vec![(SCORE, CanonicalScalar::Int(50))],
            );
            txn.write(&mut db, prefix).unwrap();
            let expected_nodes = vec![
                vec![
                    GraphValue::Vertex(VId(1)),
                    list(&["Foo"]),
                    map(&[("edge-note", text("staged")), ("score", int(20))]),
                    list(&["edge-note", "score"]),
                ],
                vec![
                    GraphValue::Vertex(VId(u128::MAX)),
                    list(&["Thing", "Bar"]),
                    map(&[("score", int(30))]),
                    list(&["score"]),
                ],
            ];
            let expected_edges = vec![
                vec![
                    GraphValue::Edge(EId(1)),
                    text("R"),
                    map(&[("name", text("edge-stage")), ("score", int(3))]),
                    list(&["name", "score"]),
                ],
                vec![
                    GraphValue::Edge(EId(u128::MAX)),
                    text("R"),
                    map(&[("score", int(50))]),
                    list(&["score"]),
                ],
            ];
            assert_eq!(
                cells(
                    txn.query(&db, &cx, NODES, &nodes, Catalog::Full, policy())
                        .unwrap()
                ),
                expected_nodes
            );
            assert_eq!(
                cells(
                    txn.query(&db, &cx, EDGES, &edges, Catalog::Full, policy())
                        .unwrap()
                ),
                expected_edges
            );
            assert_eq!(
                cells(
                    db.query(&cx, NODES, &nodes, Catalog::Full, policy())
                        .unwrap()
                ),
                old_nodes
            );
            assert_eq!(
                cells(
                    db.query(&cx, EDGES, &edges, Catalog::Full, policy())
                        .unwrap()
                ),
                old_edges
            );

            txn.savepoint(&db, "staged").unwrap();
            let digest = txn.staged_effect_digest().unwrap();
            let mut deletion = WriteBatch::new(R);
            deletion.delete_edge(EId(1));
            txn.write(&mut db, deletion).unwrap();
            assert_eq!(
                cells(
                    txn.query(&db, &cx, EDGES, &edges, Catalog::Full, policy())
                        .unwrap()
                )[0],
                vec![GraphValue::Edge(EId(1)), null(), null(), null()]
            );
            txn.rollback_to_savepoint(&db, "staged").unwrap();
            assert_eq!(txn.staged_effect_digest().unwrap(), digest);
            assert_eq!(
                cells(
                    txn.query(&db, &cx, EDGES, &edges, Catalog::Full, policy())
                        .unwrap()
                ),
                expected_edges
            );

            // A disjoint newer commit must not replace this transaction's basis.
            let mut unrelated = WriteBatch::new(R);
            unrelated.create_vertex(VId(9), vec![THING], vec![]);
            db.write(&commit, unrelated).await.unwrap();
            assert_eq!(
                cells(
                    txn.query(&db, &cx, NODES, &nodes, Catalog::Full, policy())
                        .unwrap()
                ),
                expected_nodes
            );
            txn.commit(&mut db, &commit).await.unwrap();
            db.compact(&commit).await.unwrap();
            drop(db);
            let db = Database::open_with_vfs(&commit, vfs, &path, keys())
                .await
                .unwrap();
            assert_eq!(
                cells(
                    db.query(&cx, NODES, &nodes, Catalog::Full, policy())
                        .unwrap()
                ),
                expected_nodes
            );
            assert_eq!(
                cells(
                    db.query(&cx, EDGES, &edges, Catalog::Full, policy())
                        .unwrap()
                ),
                expected_edges
            );
            assert_eq!(
                cells(
                    pinned
                        .query(&cx, NODES, &nodes, Catalog::Full, policy())
                        .unwrap()
                ),
                old_nodes
            );
            assert_eq!(
                cells(
                    pinned
                        .query(&cx, EDGES, &edges, Catalog::Full, policy())
                        .unwrap()
                ),
                old_edges
            );
            assert_eq!(txcx.outstanding_obligations(), 0);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    fn property_change(
        edge: bool,
        key: PropertyKeyId,
        value: Option<CanonicalScalar>,
    ) -> WriteBatch {
        let mut batch = WriteBatch::new(R);
        if edge {
            batch.set_edge_property(EId(1), key, value);
        } else {
            batch.set_vertex_property(VId(1), key, value);
        }
        batch
    }

    #[test]
    fn complete_metadata_witnesses_detect_insertions_and_keep_unobserved_values_independent() {
        let ((), report) = run_async_under_lab(0x6d71_0002, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let (commit, cx, txcx) = (contexts.commit(), contexts.query(), contexts.txn());
            let mut label_change = WriteBatch::new(R);
            label_change.set_vertex_label(VId(1), FOO, true);
            for (edge, function, change, conflict) in [
                (
                    false,
                    "keys",
                    property_change(false, NOTE, Some(CanonicalScalar::Int(4))),
                    true,
                ),
                (
                    false,
                    "properties",
                    property_change(false, SCORE, Some(CanonicalScalar::Int(11))),
                    true,
                ),
                (
                    true,
                    "properties",
                    property_change(true, NAME, Some(CanonicalScalar::Int(4))),
                    true,
                ),
                (false, "labels", label_change, true),
                // New fields have no per-key witness and no catalog entry yet.
                (
                    false,
                    "properties",
                    property_change(false, PropertyKeyId(99), Some(CanonicalScalar::Int(4))),
                    true,
                ),
                (
                    false,
                    "keys",
                    property_change(false, PropertyKeyId(99), Some(CanonicalScalar::Int(4))),
                    true,
                ),
                (true, "keys", property_change(true, NOTE, None), true),
                (
                    false,
                    "keys",
                    property_change(false, SCORE, Some(CanonicalScalar::Int(11))),
                    false,
                ),
                (
                    true,
                    "keys",
                    property_change(true, SCORE, Some(CanonicalScalar::Int(11))),
                    false,
                ),
                (
                    true,
                    "type",
                    property_change(true, SCORE, Some(CanonicalScalar::Int(11))),
                    false,
                ),
                (
                    false,
                    "labels",
                    property_change(false, SCORE, Some(CanonicalScalar::Int(11))),
                    false,
                ),
            ] {
                let mut db = Database::open_memory(&commit, keys()).await.unwrap();
                seed(&mut db, &commit, false).await;
                let mut txn = db.begin(&txcx).unwrap();
                txn.savepoint(&db, "before-read").unwrap();
                let operand = if edge {
                    GraphValue::Edge(EId(1))
                } else {
                    GraphValue::Vertex(VId(1))
                };
                let params = GqlParameters::new()
                    .with_list("refs", vec![operand])
                    .unwrap();
                let query = format!("UNWIND $refs AS x RETURN {function}(x) AS value");
                let rows = cells(
                    txn.query(&db, &cx, &query, &params, Catalog::Full, policy())
                        .unwrap(),
                );
                assert_eq!(rows.len(), 1);
                assert_ne!(rows[0][0], null());
                // Rolling effects back cannot erase already observed dependencies.
                txn.rollback_to_savepoint(&db, "before-read").unwrap();
                let mut own = WriteBatch::new(R);
                own.create_vertex(VId(40), vec![THING], vec![]);
                txn.write(&mut db, own).unwrap();
                db.write(&commit, change).await.unwrap();
                let result = txn.commit(&mut db, &commit).await;
                if conflict {
                    assert!(
                        matches!(
                            result,
                            Err(WriteTxnError::Write(WriteError::FirstCommitterWins {
                                law: "FG-LAW-FCW-READ-01",
                                ..
                            }))
                        ),
                        "{query}, edge={edge}: {result:?}"
                    );
                    assert!(db.vertex(VId(40)).unwrap().is_none());
                } else {
                    result.unwrap();
                    assert!(db.vertex(VId(40)).unwrap().is_some());
                }
            }
            assert_eq!(txcx.outstanding_obligations(), 0);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn dynamic_write_results_follow_staged_fields_and_simultaneous_rhs_reads() {
        let ((), report) = run_async_under_lab(0x6d71_0003, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let (commit, cx, txcx) = (contexts.commit(), contexts.query(), contexts.txn());
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let before = seed(&mut db, &commit, false).await;
            let mut txn = db.begin(&txcx).unwrap();
            let mut prefix = WriteBatch::new(R);
            prefix.set_vertex_property(VId(1), SCORE, Some(CanonicalScalar::Int(20)));
            txn.write(&mut db, prefix).unwrap();
            let params = GqlParameters::new();
            let returned = txn
                .query_write_engine(
                    &mut db,
                    &cx,
                    "MATCH (a)-[r:R]->(b) \
                 SET a.score=properties([a][0]).score+1,\
                     a.name=properties([a][0]).score,\
                     r.score=properties([r][0]).score+2,a:Foo \
                 RETURN properties([a][0]) AS node,\
                        [x IN [a] | labels(x)] AS labels,\
                        type([r][0]) AS kind,properties([r][0]) AS edge",
                    &params,
                    Catalog::Full,
                    R,
                    write_policy(),
                )
                .unwrap();
            assert_eq!(
                cells(returned),
                vec![vec![
                    map(&[("name", int(20)), ("score", int(21))]),
                    GraphValue::List(vec![list(&["Thing", "Foo"])].into_boxed_slice()),
                    text("R"),
                    map(&[
                        ("edge-note", GraphValue::Scalar(CanonicalScalar::Bool(true))),
                        ("score", int(5))
                    ]),
                ]]
            );
            let returned = txn
                .query_write_engine(
                    &mut db,
                    &cx,
                    "MATCH (a)-[r:R]->(b) REMOVE a.name,a:Thing,r.`edge-note` \
                 WITH a,r RETURN keys([a][0]) AS names,labels([a][0]) AS labels,\
                                  properties([r][0]) AS edge",
                    &params,
                    Catalog::Full,
                    R,
                    write_policy(),
                )
                .unwrap();
            assert_eq!(
                cells(returned),
                vec![vec![
                    list(&["score"]),
                    list(&["Foo"]),
                    map(&[("score", int(5))])
                ]]
            );
            let returned = txn
                .query_write_engine(
                    &mut db,
                    &cx,
                    "MATCH (a)-[r:R]->(b) SET a={name:'replacement'},r={score:7} \
                 RETURN properties([a][0]),keys([a][0]),properties([r][0])",
                    &params,
                    Catalog::Full,
                    R,
                    write_policy(),
                )
                .unwrap();
            assert_eq!(
                cells(returned),
                vec![vec![
                    map(&[("name", text("replacement"))]),
                    list(&["name"]),
                    map(&[("score", int(7))])
                ]]
            );
            let digest = txn.staged_effect_digest().unwrap();
            // A failed late projection must not install its preceding SET.
            assert!(
                txn.query_write_engine(
                    &mut db,
                    &cx,
                    "MATCH (a)-[r:R]->(b) SET r.score=99 \
                 RETURN properties([r][0]) AS props,1/(properties([r][0]).score-99) AS fail",
                    &params,
                    Catalog::Full,
                    R,
                    write_policy()
                )
                .is_err()
            );
            assert_eq!(txn.staged_effect_digest().unwrap(), digest);
            assert_eq!(db.frontier().unwrap(), before);
            txn.commit(&mut db, &commit).await.unwrap();
            assert_eq!(
                db.vertex(VId(1)).unwrap().unwrap().props,
                vec![(
                    NAME,
                    CanonicalScalar::ucs_basic_text("replacement")
                        .unwrap()
                        .into()
                )]
            );
            assert_eq!(
                db.edge(EId(1)).unwrap().unwrap().props,
                vec![(SCORE, CanonicalScalar::Int(7).into())]
            );
            assert_eq!(txcx.outstanding_obligations(), 0);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn dynamic_creation_and_merge_outputs_use_the_selected_new_elements() {
        let ((), report) = run_async_under_lab(0x6d71_0004, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let (commit, cx, txcx) = (contexts.commit(), contexts.query(), contexts.txn());
            let params = GqlParameters::new();
            for keyword in ["CREATE", "INSERT"] {
                let mut db = Database::open_memory(&commit, keys()).await.unwrap();
                let query = format!(
                    "{keyword} (a:Thing {{score:2,name:'created'}}),\
                                     (b:Thing),(a)-[r:R {{score:3}}]->(b) \
                                     RETURN properties([a][0]),labels([a][0]),\
                                     properties([b][0]),keys([r][0]),type([r][0])"
                );
                let result = db
                    .query_write_engine(
                        &txcx,
                        &cx,
                        &commit,
                        &query,
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
                        map(&[("name", text("created")), ("score", int(2))]),
                        list(&["Thing"]),
                        map(&[]),
                        list(&["score"]),
                        text("R")
                    ]]
                );
            }
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            for (score, labels) in [
                (1, list(&["Thing", "Foo"])),
                (11, list(&["Thing", "Foo", "Bar"])),
            ] {
                let result = db
                    .query_write_engine(
                        &txcx,
                        &cx,
                        &commit,
                        "MERGE (n:Thing {name:'merge-key'}) \
                     ON CREATE SET n.score=1,n:Foo ON MATCH SET n.score=n.score+10,n:Bar \
                     RETURN properties([n][0]),labels([n][0]),keys([n][0])",
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
                        map(&[("name", text("merge-key")), ("score", int(score))]),
                        labels,
                        list(&["name", "score"])
                    ]]
                );
            }
            assert_eq!(db.vertices().unwrap().len(), 1);
            assert_eq!(txcx.outstanding_obligations(), 0);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn transactional_aggregate_hosts_share_the_metadata_overlay() {
        let ((), report) = run_async_under_lab(0x6d71_0006, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let (commit, cx, txcx) = (contexts.commit(), contexts.query(), contexts.txn());
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            seed(&mut db, &commit, false).await;
            let mut txn = db.begin(&txcx).unwrap();
            let mut prefix = WriteBatch::new(R);
            prefix.set_vertex_property(VId(1), SCORE, Some(CanonicalScalar::Int(20)));
            prefix.set_vertex_property(VId(1), NAME, None);
            prefix.set_vertex_label(VId(1), FOO, true);
            prefix.set_edge_property(EId(1), NOTE, None);
            txn.write(&mut db, prefix).unwrap();
            let params = GqlParameters::new();
            assert_eq!(
                cells(
                    txn.query(
                        &db,
                        &cx,
                        "MATCH (a)-[r:R]->(b) RETURN collect(properties([a][0])) AS nodes,\
                                           collect(properties([r][0])) AS edges",
                        &params,
                        Catalog::Full,
                        policy()
                    )
                    .unwrap()
                ),
                vec![vec![
                    GraphValue::List(vec![map(&[("score", int(20))])].into_boxed_slice()),
                    GraphValue::List(vec![map(&[("score", int(3))])].into_boxed_slice()),
                ]]
            );
            assert_eq!(
                cells(
                    txn.query(
                        &db,
                        &cx,
                        "MATCH (a)-[r:R]->(b) WITH collect(a) AS nodes,collect(r) AS edges \
                 RETURN labels(nodes[0]),type(edges[0]),properties(nodes[0]),keys(edges[0])",
                        &params,
                        Catalog::Full,
                        policy()
                    )
                    .unwrap()
                ),
                vec![vec![
                    list(&["Thing", "Foo"]),
                    text("R"),
                    map(&[("score", int(20))]),
                    list(&["score"]),
                ]]
            );
            txn.abort();
            assert_eq!(txcx.outstanding_obligations(), 0);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn authorized_dynamic_write_metadata_names_only_visible_fields() {
        let ((), report) = run_async_under_lab(0x6d71_0005, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let (commit, cx, txcx) = (contexts.commit(), contexts.query(), contexts.txn());
            let issuer = Authority::new(
                AuthKey::from_seed(0x6d71_0005),
                NS,
                "graph",
                SchemaEpoch(0),
                1,
            )
            .unwrap();
            let mut grant = Grant::read_only(
                "main",
                1_000,
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
                 SET a.score=properties([a][0]).score+1,r.score=properties([r][0]).score+2 \
                 RETURN properties([a][0]),keys([a][0]),labels([a][0]),\
                        properties([r][0]),type([r][0])",
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
                        list(&["score"]),
                        list(&["Thing"]),
                        map(&[
                            ("edge-note", GraphValue::Scalar(CanonicalScalar::Bool(true))),
                            ("score", int(5))
                        ]),
                        text("R"),
                    ]]
                );
                let props = db.vertex(VId(1)).unwrap().unwrap().props;
                assert!(
                    props.contains(&(NAME, CanonicalScalar::ucs_basic_text("one").unwrap().into()))
                );
                assert_eq!(props.len(), if hidden { 22 } else { 2 });
            }
            assert_eq!(txcx.outstanding_obligations(), 0);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }
}

mod dynamic_metadata {
    use super::*;
    use fgdb::{PreparedNativeRead, QueryError};
    use fgdb_gql::{GlaLimitDimension, GraphIntegerErrorKind, GraphSetExecutionError};
    use fgdb_types::context::SimulationCheckpointProbe;
    use fgdb_warden::{Error as AuthorizationError, LimitDimension};
    use std::sync::Arc;

    fn values(values: Vec<GraphValue>) -> GraphValue {
        GraphValue::List(values.into_boxed_slice())
    }

    fn node() -> GraphValue {
        map(&[("name", text("one")), ("score", int(10))])
    }

    fn edge() -> GraphValue {
        map(&[
            ("edge-note", GraphValue::Scalar(CanonicalScalar::Bool(true))),
            ("score", int(3)),
        ])
    }

    fn sorted(mut rows: Vec<Vec<GraphValue>>) -> Vec<Vec<GraphValue>> {
        rows.sort();
        rows
    }

    #[test]
    fn carried_nodes_edges_subscripts_locals_and_aggregate_outputs_read_native_metadata() {
        let ((), report) = run_async_under_lab(0x6d61_0101, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let cx = contexts.query();
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            seed(&mut db, &commit, false).await;
            let params = GqlParameters::new();
            let node_rows = vec![
                vec![list(&["Thing"]), node(), list(&["name", "score"])],
                vec![list(&["Thing"]), map(&[]), list(&[])],
            ];
            let edge_rows = vec![vec![text("R"), edge(), list(&["edge-note", "score"])]];
            for (statement, expected) in [
                (
                    "MATCH p=(a:Thing)-[:R]->(b:Thing) WITH nodes(p) AS ns \
                     UNWIND ns AS x RETURN labels(x) AS labels,properties(x) AS props,keys(x) AS keys",
                    node_rows.clone(),
                ),
                (
                    "MATCH p=(a)-[:R]->(b) WITH relationships(p) AS rs \
                     UNWIND rs AS x RETURN type(x) AS kind,properties(x) AS props,keys(x) AS keys",
                    edge_rows.clone(),
                ),
                (
                    "MATCH p=(a)-[:R]->(b) RETURN labels(nodes(p)[-1]) AS labels,\
                     properties(nodes(p)[0]) AS props,type(relationships(p)[0]) AS kind,\
                     keys(relationships(p)[0]) AS keys",
                    vec![vec![
                        list(&["Thing"]),
                        node(),
                        text("R"),
                        list(&["edge-note", "score"]),
                    ]],
                ),
                (
                    "MATCH (n:Thing) WITH collect(n) AS ns UNWIND ns AS x \
                     RETURN labels(x) AS labels,properties(x) AS props,keys(x) AS keys",
                    node_rows,
                ),
                (
                    "MATCH (a)-[r:R]->(b) WITH collect(r) AS rs UNWIND rs AS x \
                     RETURN type(x) AS kind,properties(x) AS props,keys(x) AS keys",
                    edge_rows.clone(),
                ),
                (
                    "MATCH p=(a)-[:R]->(b) RETURN [x IN nodes(p) | labels(x)] AS labels,\
                     [x IN nodes(p) | properties(x)] AS props,\
                     [x IN relationships(p) | type(x)] AS kinds,\
                     [x IN relationships(p) | keys(x)] AS keys",
                    vec![vec![
                        values(vec![list(&["Thing"]), list(&["Thing"])]),
                        values(vec![node(), map(&[])]),
                        list(&["R"]),
                        values(vec![list(&["edge-note", "score"])]),
                    ]],
                ),
                (
                    "MATCH (n:Thing {score:10}) RETURN labels(collect(n)[0]) AS labels,\
                     properties(collect(n)[0]) AS props,keys(collect(n)[0]) AS keys",
                    vec![vec![list(&["Thing"]), node(), list(&["name", "score"])]],
                ),
                (
                    "MATCH (a)-[r:R]->(b) RETURN type(collect(r)[0]) AS kind,\
                     properties(collect(r)[0]) AS props,keys(collect(r)[0]) AS keys",
                    edge_rows,
                ),
                (
                    "MATCH p=(a)-[:R]->(b) WITH nodes(p) AS ns UNWIND ns AS x \
                     RETURN labels(x) AS labels,count(*) AS count",
                    vec![vec![list(&["Thing"]), int(2)]],
                ),
            ] {
                let actual = db
                    .query(&cx, statement, &params, Catalog::Full, policy())
                    .unwrap_or_else(|error| panic!("{statement}: {error:?}"));
                assert_eq!(sorted(cells(actual)), sorted(expected), "{statement}");
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn metadata_nulls_type_errors_catalog_failures_and_precharge_keep_their_native_causes() {
        let ((), report) = run_async_under_lab(0x6d61_0102, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let cx = contexts.query();
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            seed(&mut db, &commit, false).await;
            for (statement, expected) in [
                (
                    "UNWIND [NULL] AS x RETURN labels(x),type(x),properties(x),keys(x)",
                    vec![vec![null(), null(), null(), null()]],
                ),
                (
                    "UNWIND [{score:7}] AS x RETURN properties(x),keys(x)",
                    vec![vec![map(&[("score", int(7))]), list(&["score"])]],
                ),
            ] {
                assert_eq!(
                    cells(
                        db.query(
                            &cx,
                            statement,
                            &GqlParameters::new(),
                            Catalog::Full,
                            policy()
                        )
                        .unwrap()
                    ),
                    expected
                );
            }
            for (function, operand) in [
                ("labels", GraphValue::Edge(EId(1))),
                ("type", GraphValue::Vertex(VId(1))),
                ("labels", int(1)),
                ("type", text("R")),
                ("properties", int(1)),
                ("keys", int(1)),
            ] {
                let parameters = GqlParameters::new()
                    .with_list("refs", vec![operand])
                    .unwrap();
                let statement = format!("UNWIND $refs AS x RETURN {function}(x) AS value");
                let error = db
                    .query(&cx, &statement, &parameters, Catalog::Full, policy())
                    .unwrap_err();
                assert!(
                    matches!(error,
                        QueryError::Set(GqlQueryError::Source(GraphSetExecutionError::Projection { error, .. }))
                            if error.kind == GraphIntegerErrorKind::IncompatibleOperands
                    ),
                    "{statement}: {error:?}"
                );
            }
            let parameters = GqlParameters::new()
                .with_list("refs", vec![GraphValue::Vertex(VId(1))])
                .unwrap();
            let statement = "UNWIND $refs AS x RETURN properties(x) AS props";
            for suffix in ["", " LIMIT 0"] {
                let statement = format!("{statement}{suffix}");
                assert!(
                    matches!(
                        db.query(
                            &cx,
                            &statement,
                            &parameters,
                            Catalog::MissingScore,
                            policy()
                        ),
                        Err(QueryError::Set(GqlQueryError::Source(
                            GraphSetExecutionError::Source(GqlError::Read(
                                ReadError::UnmappedProperty(SCORE)
                            ))
                        )))
                    ),
                    "{statement}"
                );
            }
            let error = db
                .query(
                    &cx,
                    statement,
                    &parameters,
                    Catalog::MissingScore,
                    GqlQueryPolicy::new(100_000, 100_000, 0, 1_000_000),
                )
                .unwrap_err();
            assert!(
                matches!(error, QueryError::Set(GqlQueryError::Evaluator(limit))
                if limit.dimension == GlaLimitDimension::WorkUnits),
                "{error:?}"
            );
            let cancelled =
                cx.with_checkpoint_probe(Arc::new(SimulationCheckpointProbe::new(Some(1))));
            let error = db
                .query(
                    &cancelled,
                    statement,
                    &parameters,
                    Catalog::MissingScore,
                    policy(),
                )
                .unwrap_err();
            assert!(
                matches!(error, QueryError::Set(GqlQueryError::Interrupted(_))),
                "{error:?}"
            );

            // Every reached native checkpoint is a refusal boundary; none may
            // return a successful metadata prefix or spoil a later execution.
            let probe = Arc::new(SimulationCheckpointProbe::new(None));
            let expected = db
                .query(
                    &cx.with_checkpoint_probe(Arc::clone(&probe)),
                    statement,
                    &parameters,
                    Catalog::Full,
                    policy(),
                )
                .unwrap();
            assert!(probe.calls() > 0);
            for stop in 1..=probe.calls() {
                let stopped = Arc::new(SimulationCheckpointProbe::new(Some(stop)));
                let error = db
                    .query(
                        &cx.with_checkpoint_probe(Arc::clone(&stopped)),
                        statement,
                        &parameters,
                        Catalog::Full,
                        policy(),
                    )
                    .unwrap_err();
                assert!(
                    matches!(error, QueryError::Set(GqlQueryError::Interrupted(_))),
                    "checkpoint {stop}: {error:?}"
                );
                assert_eq!(stopped.calls(), stop);
            }
            assert_eq!(
                db.query(&cx, statement, &parameters, Catalog::Full, policy())
                    .unwrap(),
                expected
            );
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn dynamic_metadata_keeps_pinned_data_and_catalog_through_edits_compaction_and_reopen() {
        let ((), report) = run_async_under_lab(0x6d61_0103, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let cx = contexts.query();
            let vfs = MemVfs::new().unwrap();
            let path = vfs.database_dir();
            let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
                .await
                .unwrap();
            let basis = seed(&mut db, &commit, false).await;
            let pinned = db.read_session().unwrap();
            let parameters = GqlParameters::new();
            let statement = "MATCH p=(a)-[:R]->(b) WITH nodes(p) AS ns,relationships(p) AS rs \
                RETURN labels(ns[0]) AS labels,properties(ns[0]) AS node,\
                type(rs[0]) AS kind,properties(rs[0]) AS edge,keys(rs[0]) AS keys";
            let prepared =
                PreparedNativeRead::prepare(statement, &parameters, Catalog::Full).unwrap();
            let renamed =
                PreparedNativeRead::prepare(statement, &parameters, Catalog::RenamedScore).unwrap();
            let before = vec![vec![
                list(&["Thing"]),
                node(),
                text("R"),
                edge(),
                list(&["edge-note", "score"]),
            ]];
            assert_eq!(
                cells(
                    prepared
                        .execute_in_view(&pinned, &cx, &parameters, policy())
                        .unwrap()
                ),
                before
            );
            let mut edit = WriteBatch::new(R);
            edit.set_vertex_label(VId(1), FOO, true);
            edit.set_vertex_property(VId(1), SCORE, Some(CanonicalScalar::Int(20)));
            edit.set_vertex_property(
                VId(1),
                NAME,
                Some(CanonicalScalar::ucs_basic_text("two").unwrap()),
            );
            edit.set_edge_property(EId(1), SCORE, None);
            edit.set_edge_property(
                EId(1),
                NAME,
                Some(CanonicalScalar::ucs_basic_text("edge").unwrap()),
            );
            db.write(&commit, edit).await.unwrap();
            db.compact(&commit).await.unwrap();
            drop(db);
            let db = Database::open_with_vfs(&commit, vfs, &path, keys())
                .await
                .unwrap();
            let expected = vec![vec![
                list(&["Thing", "Foo"]),
                map(&[("name", text("two")), ("score", int(20))]),
                text("R"),
                map(&[
                    ("edge-note", GraphValue::Scalar(CanonicalScalar::Bool(true))),
                    ("name", text("edge")),
                ]),
                list(&["edge-note", "name"]),
            ]];
            assert_eq!(
                cells(prepared.execute(&db, &cx, &parameters, policy()).unwrap()),
                expected
            );
            let (result, selected) = pinned
                .query_with_snapshot(&cx, statement, &parameters, Catalog::Full, policy())
                .unwrap();
            assert_eq!(selected, basis);
            assert_eq!(cells(result), before);
            let temporal = statement.replacen(
                " WITH ",
                &format!(" FOR SYSTEM_TIME AS OF SEQ {} WITH ", basis.0),
                1,
            );
            let current = db.read_session().unwrap();
            let (result, selected) = current
                .query_with_snapshot(&cx, &temporal, &parameters, Catalog::Full, policy())
                .unwrap();
            assert_eq!(selected, basis);
            assert_eq!(cells(result), before);
            let renamed_before = vec![vec![
                list(&["Thing"]),
                map(&[("name", text("one")), ("renamed-score", int(10))]),
                text("R"),
                map(&[
                    ("edge-note", GraphValue::Scalar(CanonicalScalar::Bool(true))),
                    ("renamed-score", int(3)),
                ]),
                list(&["edge-note", "renamed-score"]),
            ]];
            assert_eq!(
                cells(
                    renamed
                        .execute_in_view(&pinned, &cx, &parameters, policy())
                        .unwrap()
                ),
                renamed_before
            );
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn hidden_metadata_and_forged_identities_match_the_removed_graph_and_signed_work_boundary() {
        let ((), report) = run_async_under_lab(0x6d61_0104, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let cx = contexts.query();
            let mut hidden = Database::open_memory(&commit, keys()).await.unwrap();
            let mut removed = Database::open_memory(&commit, keys()).await.unwrap();
            seed(&mut hidden, &commit, true).await;
            seed(&mut removed, &commit, false).await;
            let mut extra = WriteBatch::new(R);
            extra.set_vertex_label(VId(1), LabelId(99), true);
            extra.add_edge(
                EId(3),
                VId(3),
                VId(1),
                vec![(PropertyKeyId(99), CanonicalScalar::Int(999))],
            );
            hidden.write(&commit, extra).await.unwrap();
            let mut extra = WriteBatch::new(RelationId(99));
            extra.add_edge(
                EId(4),
                VId(1),
                VId(2),
                vec![(PropertyKeyId(99), CanonicalScalar::Int(999))],
            );
            hidden.write(&commit, extra).await.unwrap();
            let issuer = Authority::new(
                AuthKey::from_seed(0x6d61_0114),
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
                    max_work: 100_000,
                    max_rows: 100_000,
                },
            );
            grant.labels = Scope::only([THING]);
            grant.relations = Scope::only([R]);
            grant.properties = Scope::only([SCORE, NAME, NOTE]);
            let read =
                |db: &Database<MemVfs>, statement: &str, parameters: &GqlParameters, maximum| {
                    let mut grant = grant.clone();
                    grant.limits.max_work = maximum;
                    let token = issuer.issue_at(&grant, 100).unwrap();
                    db.query_authorized(
                        &cx,
                        &issuer,
                        &token,
                        "main",
                        statement,
                        parameters,
                        Catalog::Full,
                        policy(),
                        || 100,
                    )
                };
            let node_rows = vec![
                vec![list(&["Thing"]), node(), list(&["name", "score"])],
                vec![list(&["Thing"]), map(&[]), list(&[])],
            ];
            let mut cases = vec![
                (
                    "MATCH p=(a)-[r]->(b) WITH nodes(p) AS ns UNWIND ns AS x RETURN labels(x),properties(x),keys(x)",
                    GqlParameters::new(),
                    node_rows.clone(),
                ),
                (
                    "MATCH p=(a)-[r]->(b) WITH relationships(p) AS rs UNWIND rs AS x RETURN type(x),properties(x),keys(x)",
                    GqlParameters::new(),
                    vec![vec![text("R"), edge(), list(&["edge-note", "score"])]],
                ),
                (
                    "MATCH (n:Thing) WITH collect(n) AS ns UNWIND ns AS x RETURN labels(x),properties(x),keys(x)",
                    GqlParameters::new(),
                    node_rows,
                ),
                (
                    "MATCH p=(a)-[r]->(b) RETURN [x IN nodes(p) | labels(x)],\
                    [x IN relationships(p) | properties(x)]",
                    GqlParameters::new(),
                    vec![vec![
                        values(vec![list(&["Thing"]), list(&["Thing"])]),
                        values(vec![edge()]),
                    ]],
                ),
            ];
            for (identity, expected) in [
                (
                    VId(1),
                    vec![list(&["Thing"]), node(), list(&["name", "score"])],
                ),
                (VId(2), vec![list(&["Thing"]), map(&[]), list(&[])]),
                (VId(3), vec![null(), null(), null()]),
                (VId(999), vec![null(), null(), null()]),
            ] {
                cases.push((
                    "UNWIND $refs AS x RETURN labels(x),properties(x),keys(x)",
                    GqlParameters::new()
                        .with_list("refs", vec![GraphValue::Vertex(identity)])
                        .unwrap(),
                    vec![expected],
                ));
            }
            for identity in [EId(1), EId(2), EId(3), EId(4), EId(999)] {
                let expected = if identity == EId(1) {
                    vec![text("R"), edge(), list(&["edge-note", "score"])]
                } else {
                    vec![null(), null(), null()]
                };
                cases.push((
                    "UNWIND $refs AS x RETURN type(x),properties(x),keys(x)",
                    GqlParameters::new()
                        .with_list("refs", vec![GraphValue::Edge(identity)])
                        .unwrap(),
                    vec![expected],
                ));
            }
            let mut hidden_vertex_work = None;
            let mut hidden_edge_work = None;
            for (statement, parameters, expected) in cases {
                let expected = sorted(expected);
                assert_eq!(
                    sorted(cells(
                        removed
                            .query(&cx, statement, &parameters, Catalog::Full, policy())
                            .unwrap()
                    )),
                    expected
                );
                for db in [&hidden, &removed] {
                    assert_eq!(
                        sorted(cells(
                            read(db, statement, &parameters, 100_000)
                                .unwrap_or_else(|error| panic!("{statement}: {error:?}"))
                        )),
                        expected
                    );
                }
                let minimum = |db: &Database<MemVfs>| {
                    let (mut low, mut high) = (0, 100_000);
                    while low < high {
                        let middle = low + (high - low) / 2;
                        match read(db, statement, &parameters, middle) {
                            Ok(_) => high = middle,
                            Err(error) => {
                                assert!(
                                    matches!(
                                        error,
                                        QueryError::Authorization(
                                            AuthorizationError::LimitExceeded(LimitDimension::Work)
                                        )
                                    ),
                                    "{statement}: {error:?}"
                                );
                                low = middle + 1;
                            }
                        }
                    }
                    low
                };
                let work = minimum(&removed);
                assert!(work > 0 && work < 100_000);
                assert_eq!(minimum(&hidden), work, "hidden metadata work: {statement}");
                assert!(matches!(
                    read(&hidden, statement, &parameters, work - 1),
                    Err(QueryError::Authorization(
                        AuthorizationError::LimitExceeded(LimitDimension::Work)
                    ))
                ));
                assert_eq!(
                    sorted(cells(read(&hidden, statement, &parameters, work).unwrap())),
                    expected
                );
                if expected == vec![vec![null(), null(), null()]] {
                    let same_kind = if statement.contains("labels(x)") {
                        &mut hidden_vertex_work
                    } else {
                        &mut hidden_edge_work
                    };
                    if let Some(prior) = *same_kind {
                        assert_eq!(work, prior, "hidden and absent identity work: {statement}");
                    } else {
                        *same_kind = Some(work);
                    }
                }
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }
}

mod transaction_dynamic_metadata {
    use super::*;
    use fgdb::{WriteError, WriteTxnError};

    fn sorted(mut rows: Vec<Vec<GraphValue>>) -> Vec<Vec<GraphValue>> {
        rows.sort();
        rows
    }

    #[test]
    fn carried_metadata_reads_canonical_staging_in_rows_and_aggregate_outputs() {
        let ((), report) = run_async_under_lab(0x6d61_0040, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let (commit, cx, txcx) = (contexts.commit(), contexts.query(), contexts.txn());
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let basis = seed(&mut db, &commit, false).await;
            let pinned = db.read_session().unwrap();
            let params = GqlParameters::new()
                .with_list(
                    "nodes",
                    vec![
                        GraphValue::Vertex(VId(1)),
                        GraphValue::Vertex(VId(u128::MAX)),
                    ],
                )
                .unwrap()
                .with_list(
                    "edges",
                    vec![GraphValue::Edge(EId(1)), GraphValue::Edge(EId(u128::MAX))],
                )
                .unwrap();
            let nodes = "UNWIND $nodes AS x RETURN labels(x) AS labels, properties(x) AS props, keys(x) AS names";
            let edges = "UNWIND $edges AS x RETURN type(x) AS relation, properties(x) AS props, keys(x) AS names";
            let before = cells(
                db.query(&cx, nodes, &params, Catalog::Full, policy())
                    .unwrap(),
            );
            let mut txn = db.begin(&txcx).unwrap();
            let mut edits = WriteBatch::new(R);
            edits.set_vertex_label(VId(1), THING, false);
            edits.set_vertex_label(VId(1), FOO, true);
            edits.set_vertex_property(VId(1), SCORE, Some(CanonicalScalar::Int(40)));
            edits.set_vertex_property(VId(1), NAME, None);
            edits.set_vertex_property(VId(1), NOTE, Some(CanonicalScalar::Int(5)));
            edits.set_edge_property(EId(1), SCORE, None);
            edits.set_edge_property(EId(1), NAME, Some(CanonicalScalar::Int(9)));
            edits.create_vertex(
                VId(u128::MAX),
                vec![BAR],
                vec![(SCORE, CanonicalScalar::Int(70))],
            );
            edits.add_edge(
                EId(u128::MAX),
                VId(u128::MAX),
                VId(2),
                vec![(NAME, CanonicalScalar::Int(8))],
            );
            txn.write(&mut db, edits).unwrap();
            let digest = txn.staged_effect_digest().unwrap();
            let node_props = map(&[("edge-note", int(5)), ("score", int(40))]);
            let expected_nodes = sorted(vec![
                vec![
                    list(&["Foo"]),
                    node_props.clone(),
                    list(&["edge-note", "score"]),
                ],
                vec![list(&["Bar"]), map(&[("score", int(70))]), list(&["score"])],
            ]);
            let expected_edges = sorted(vec![
                vec![
                    text("R"),
                    map(&[
                        ("edge-note", GraphValue::Scalar(CanonicalScalar::Bool(true))),
                        ("name", int(9)),
                    ]),
                    list(&["edge-note", "name"]),
                ],
                vec![text("R"), map(&[("name", int(8))]), list(&["name"])],
            ]);
            for (statement, expected) in [(nodes, &expected_nodes), (edges, &expected_edges)] {
                assert_eq!(
                    sorted(cells(
                        txn.query(&db, &cx, statement, &params, Catalog::Full, policy())
                            .unwrap()
                    )),
                    *expected
                );
            }
            // Exercise the direct pattern aggregate and the relational aggregate
            // independently; both evaluate carried identities after grouping.
            for statement in [
                "MATCH (n:Foo) RETURN labels(collect(n)[0]) AS labels, properties(collect(n)[0]) AS props, keys(collect(n)[0]) AS names",
                "UNWIND $nodes AS x RETURN labels(collect(x)[0]) AS labels, properties(collect(x)[0]) AS props, keys(collect(x)[0]) AS names",
            ] {
                assert_eq!(
                    cells(
                        txn.query(&db, &cx, statement, &params, Catalog::Full, policy())
                            .unwrap()
                    ),
                    vec![vec![
                        list(&["Foo"]),
                        node_props.clone(),
                        list(&["edge-note", "score"])
                    ]]
                );
            }
            assert_eq!(txn.staged_effect_digest().unwrap(), digest);
            assert_eq!(db.frontier().unwrap(), basis);
            assert_eq!(
                cells(
                    db.query(&cx, nodes, &params, Catalog::Full, policy())
                        .unwrap()
                ),
                before
            );
            txn.commit(&mut db, &commit).await.unwrap();
            assert_eq!(
                sorted(cells(
                    db.query(&cx, nodes, &params, Catalog::Full, policy())
                        .unwrap()
                )),
                expected_nodes
            );
            assert_eq!(
                cells(
                    pinned
                        .query(&cx, nodes, &params, Catalog::Full, policy())
                        .unwrap()
                ),
                before
            );
            assert_eq!(txcx.outstanding_obligations(), 0);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn metadata_absence_and_edge_cascades_follow_staging_and_savepoint_rollback() {
        let ((), report) = run_async_under_lab(0x6d61_0041, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let (commit, cx, txcx) = (contexts.commit(), contexts.query(), contexts.txn());
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            seed(&mut db, &commit, false).await;
            let params = GqlParameters::new()
                .with_list(
                    "nodes",
                    vec![
                        GraphValue::Vertex(VId(2)),
                        GraphValue::Vertex(VId(99)),
                        null(),
                    ],
                )
                .unwrap()
                .with_list(
                    "edges",
                    vec![GraphValue::Edge(EId(1)), GraphValue::Edge(EId(99)), null()],
                )
                .unwrap();
            let nodes = "UNWIND $nodes AS x RETURN labels(x), properties(x), keys(x)";
            let edges = "UNWIND $edges AS x RETURN type(x), properties(x), keys(x)";
            let mut txn = db.begin(&txcx).unwrap();
            let before_nodes = cells(
                txn.query(&db, &cx, nodes, &params, Catalog::Full, policy())
                    .unwrap(),
            );
            let before_edges = cells(
                txn.query(&db, &cx, edges, &params, Catalog::Full, policy())
                    .unwrap(),
            );
            txn.savepoint(&db, "before_cascade").unwrap();
            let mut deletion = WriteBatch::new(R);
            deletion.delete_vertex(VId(2));
            txn.write(&mut db, deletion).unwrap();
            for statement in [nodes, edges] {
                assert_eq!(
                    cells(
                        txn.query(&db, &cx, statement, &params, Catalog::Full, policy())
                            .unwrap()
                    ),
                    vec![vec![null(), null(), null()]; 3]
                );
            }
            txn.rollback_to_savepoint(&db, "before_cascade").unwrap();
            assert_eq!(
                cells(
                    txn.query(&db, &cx, nodes, &params, Catalog::Full, policy())
                        .unwrap()
                ),
                before_nodes
            );
            assert_eq!(
                cells(
                    txn.query(&db, &cx, edges, &params, Catalog::Full, policy())
                        .unwrap()
                ),
                before_edges
            );
            txn.abort();
            assert!(db.edge(EId(1)).unwrap().is_some());
            assert_eq!(txcx.outstanding_obligations(), 0);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn metadata_observes_field_insertions_absent_identities_and_cascade_retirement() {
        let ((), report) = run_async_under_lab(0x6d61_0042, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let (commit, cx, txcx) = (contexts.commit(), contexts.query(), contexts.txn());
            for case in 0..7 {
                let mut db = Database::open_memory(&commit, keys()).await.unwrap();
                seed(&mut db, &commit, false).await;
                let mut txn = db.begin(&txcx).unwrap();
                txn.savepoint(&db, "before_read").unwrap();
                let (element, expression) = match case {
                    0 => (GraphValue::Vertex(VId(1)), "labels(x)"),
                    1 => (GraphValue::Vertex(VId(2)), "properties(x)"),
                    2 => (GraphValue::Edge(EId(1)), "keys(x)"),
                    3 => (GraphValue::Vertex(VId(99)), "labels(x)"),
                    4 => (GraphValue::Edge(EId(99)), "type(x)"),
                    5 => (GraphValue::Edge(EId(1)), "type(x)"),
                    _ => (GraphValue::Vertex(VId(1)), "properties(x)"),
                };
                let params = GqlParameters::new()
                    .with_list("elements", vec![element])
                    .unwrap();
                txn.query(
                    &db,
                    &cx,
                    &format!("UNWIND $elements AS x RETURN {expression}"),
                    &params,
                    Catalog::Full,
                    policy(),
                )
                .unwrap();
                txn.rollback_to_savepoint(&db, "before_read").unwrap();
                let mut winner = WriteBatch::new(R);
                match case {
                    0 => {
                        winner.set_vertex_label(VId(1), FOO, true);
                    }
                    1 => {
                        winner.set_vertex_property(VId(2), NOTE, Some(CanonicalScalar::Int(7)));
                    }
                    2 => {
                        winner.set_edge_property(EId(1), NAME, Some(CanonicalScalar::Int(7)));
                    }
                    3 => {
                        winner.create_vertex(VId(99), vec![], vec![]);
                    }
                    4 => {
                        winner.add_edge(EId(99), VId(1), VId(2), vec![]);
                    }
                    5 => {
                        winner.delete_vertex(VId(2));
                    }
                    _ => {
                        winner.set_vertex_property(VId(1), SCORE, Some(CanonicalScalar::Int(99)));
                    }
                }
                db.write(&commit, winner).await.unwrap();
                if case == 6 {
                    // Restoring the same answer cannot erase the observation
                    // of an intervening write to an enumerated property.
                    let mut restore = WriteBatch::new(R);
                    restore.set_vertex_property(VId(1), SCORE, Some(CanonicalScalar::Int(10)));
                    db.write(&commit, restore).await.unwrap();
                }
                let frontier = db.frontier().unwrap();
                assert!(
                    matches!(
                        txn.finish(&mut db, &commit).await,
                        Err(WriteTxnError::Write(WriteError::FirstCommitterWins {
                            law: "FG-LAW-FCW-READ-01",
                            ..
                        }))
                    ),
                    "metadata observation was lost in case {case}"
                );
                assert_eq!(db.frontier().unwrap(), frontier);
                assert_eq!(txcx.outstanding_obligations(), 0);
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn metadata_refusals_keep_staging_and_the_original_control_or_source_error() {
        use fgdb::QueryError;
        use fgdb_gql::{GlaLimitDimension, GraphSetExecutionError};
        use fgdb_types::context::SimulationCheckpointProbe;
        use std::sync::Arc;

        let ((), report) = run_async_under_lab(0x6d61_0043, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let (commit, cx, txcx) = (contexts.commit(), contexts.query(), contexts.txn());
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let basis = seed(&mut db, &commit, false).await;
            let mut txn = db.begin(&txcx).unwrap();
            let mut prefix = WriteBatch::new(R);
            prefix.create_vertex(VId(90), vec![BAR], vec![]);
            txn.write(&mut db, prefix).unwrap();
            let digest = txn.staged_effect_digest().unwrap();
            let params = GqlParameters::new()
                .with_list("elements", vec![GraphValue::Vertex(VId(1))])
                .unwrap();
            let statement = "UNWIND $elements AS x RETURN properties(x) AS props";
            for suffix in ["", " LIMIT 0"] {
                let failure = txn
                    .query(
                        &db,
                        &cx,
                        &format!("{statement}{suffix}"),
                        &params,
                        Catalog::MissingScore,
                        policy(),
                    )
                    .unwrap_err();
                assert!(
                    matches!(&failure, QueryError::TransactionSet(error)
                    if matches!(error.as_ref(), GqlQueryError::Source(GraphSetExecutionError::Source(WriteTxnError::Read(ReadError::UnmappedProperty(SCORE)))))),
                    "{failure:?}"
                );
                assert_eq!(txn.staged_effect_digest().unwrap(), digest);
            }
            for (budget, dimension) in [
                (
                    GqlQueryPolicy::new(100_000, 100_000, 0, 1_000_000),
                    GlaLimitDimension::WorkUnits,
                ),
                (
                    GqlQueryPolicy::new(100_000, 100_000, 1_000_000, 0),
                    GlaLimitDimension::ScratchEntries,
                ),
            ] {
                let failure = txn
                    .query(&db, &cx, statement, &params, Catalog::Full, budget)
                    .unwrap_err();
                assert!(
                    matches!(&failure, QueryError::TransactionSet(error)
                    if matches!(error.as_ref(), GqlQueryError::Evaluator(limit) if limit.dimension == dimension)),
                    "{failure:?}"
                );
                assert_eq!(txn.staged_effect_digest().unwrap(), digest);
            }
            let probe = Arc::new(SimulationCheckpointProbe::new(None));
            let expected = txn
                .query(
                    &db,
                    &cx.with_checkpoint_probe(Arc::clone(&probe)),
                    statement,
                    &params,
                    Catalog::Full,
                    policy(),
                )
                .unwrap();
            assert!(probe.calls() > 1);
            for stop in [1, probe.calls() / 2, probe.calls()] {
                let stopped = Arc::new(SimulationCheckpointProbe::new(Some(stop)));
                let failure = txn
                    .query(
                        &db,
                        &cx.with_checkpoint_probe(Arc::clone(&stopped)),
                        statement,
                        &params,
                        Catalog::Full,
                        policy(),
                    )
                    .unwrap_err();
                assert!(
                    matches!(&failure, QueryError::TransactionSet(error)
                    if matches!(error.as_ref(), GqlQueryError::Interrupted(_))),
                    "checkpoint {stop}: {failure:?}"
                );
                assert_eq!(stopped.calls(), stop);
                assert_eq!(txn.staged_effect_digest().unwrap(), digest);
            }
            assert_eq!(
                txn.query(&db, &cx, statement, &params, Catalog::Full, policy())
                    .unwrap(),
                expected
            );
            assert_eq!(db.frontier().unwrap(), basis);
            txn.abort();
            assert!(db.vertex(VId(90)).unwrap().is_none());
            assert_eq!(txcx.outstanding_obligations(), 0);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }
}

mod write_tail_dynamic_metadata {
    use super::*;

    #[test]
    fn create_and_merge_returns_read_final_metadata_in_rows_and_aggregate_tails() {
        let ((), report) = run_async_under_lab(0x6d61_0044, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let (commit, cx, txcx) = (contexts.commit(), contexts.query(), contexts.txn());
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            seed(&mut db, &commit, false).await;
            let mut endpoint = WriteBatch::new(R);
            endpoint.set_vertex_property(VId(2), SCORE, Some(CanonicalScalar::Int(20)));
            let basis = db.write(&commit, endpoint).await.unwrap();
            let params = GqlParameters::new();
            let mut txn = db.begin(&txcx).unwrap();
            for (statement, expected) in [
                (
                    "CREATE (n:Foo {score:30}) RETURN labels([n][0]) AS labels, properties([n][0]) AS props, keys([n][0]) AS names",
                    vec![vec![
                        list(&["Foo"]),
                        map(&[("score", int(30))]),
                        list(&["score"]),
                    ]],
                ),
                (
                    "MERGE (n:Foo {score:40}) ON CREATE SET n.name='created' RETURN labels([n][0]) AS labels, properties([n][0]) AS props, keys([n][0]) AS names",
                    vec![vec![
                        list(&["Foo"]),
                        map(&[("name", text("created")), ("score", int(40))]),
                        list(&["name", "score"]),
                    ]],
                ),
                (
                    "MERGE (n:Foo {score:40}) ON MATCH SET n.name='matched' RETURN labels([n][0]) AS labels, properties([n][0]) AS props, keys([n][0]) AS names",
                    vec![vec![
                        list(&["Foo"]),
                        map(&[("name", text("matched")), ("score", int(40))]),
                        list(&["name", "score"]),
                    ]],
                ),
                (
                    "MERGE (n:Foo {score:40}) RETURN labels(collect(n)[0]) AS labels, properties(collect(n)[0]) AS props, keys(collect(n)[0]) AS names",
                    vec![vec![
                        list(&["Foo"]),
                        map(&[("name", text("matched")), ("score", int(40))]),
                        list(&["name", "score"]),
                    ]],
                ),
                (
                    "CREATE (a:Foo)-[r:R {score:50}]->(b:Bar) RETURN type([r][0]) AS kind, properties([r][0]) AS props, keys([r][0]) AS names",
                    vec![vec![
                        text("R"),
                        map(&[("score", int(50))]),
                        list(&["score"]),
                    ]],
                ),
                (
                    "MATCH (a:Thing {score:10}), (b:Thing {score:20}) MERGE (a)-[r:R]->(b) ON MATCH SET r.score=60 RETURN type([r][0]) AS kind, properties([r][0]) AS props, keys([r][0]) AS names",
                    vec![vec![
                        text("R"),
                        map(&[
                            ("edge-note", GraphValue::Scalar(CanonicalScalar::Bool(true))),
                            ("score", int(60)),
                        ]),
                        list(&["edge-note", "score"]),
                    ]],
                ),
                (
                    "MATCH (a:Thing {score:20}), (b:Thing {score:10}) MERGE (a)-[r:R]->(b) ON CREATE SET r.score=70 RETURN type([r][0]) AS kind, properties([r][0]) AS props, keys([r][0]) AS names",
                    vec![vec![
                        text("R"),
                        map(&[("score", int(70))]),
                        list(&["score"]),
                    ]],
                ),
                (
                    "UNWIND [80,81] AS score CREATE (n:Foo {score:score}) RETURN labels([n][0]) AS labels, properties([n][0]) AS props, keys([n][0]) AS names",
                    vec![
                        vec![list(&["Foo"]), map(&[("score", int(80))]), list(&["score"])],
                        vec![list(&["Foo"]), map(&[("score", int(81))]), list(&["score"])],
                    ],
                ),
            ] {
                let returned = txn
                    .query_write_engine(
                        &mut db,
                        &cx,
                        statement,
                        &params,
                        Catalog::Full,
                        R,
                        write_policy(),
                    )
                    .unwrap_or_else(|error| panic!("{statement}: {error}"));
                assert_eq!(cells(returned), expected, "{statement}");
                assert_eq!(db.frontier().unwrap(), basis);
            }
            let vertices = txn.vertices(&db).unwrap();
            let edges = txn.edges(&db).unwrap();
            txn.commit(&mut db, &commit).await.unwrap();
            // Publication assigns the durable creation sequence; compare the
            // identities and fields whose staged metadata was returned.
            assert_eq!(
                db.vertices()
                    .unwrap()
                    .iter()
                    .map(|vertex| (
                        vertex.vid,
                        vertex.birth_ordinal,
                        &vertex.labels,
                        &vertex.props
                    ))
                    .collect::<Vec<_>>(),
                vertices
                    .iter()
                    .map(|vertex| (
                        vertex.vid,
                        vertex.birth_ordinal,
                        &vertex.labels,
                        &vertex.props
                    ))
                    .collect::<Vec<_>>()
            );
            assert_eq!(
                db.edges()
                    .unwrap()
                    .iter()
                    .map(|edge| (&edge.entry.eid, &edge.props))
                    .collect::<Vec<_>>(),
                edges
                    .iter()
                    .map(|edge| (&edge.entry.eid, &edge.props))
                    .collect::<Vec<_>>()
            );
            assert_eq!(txcx.outstanding_obligations(), 0);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn failing_metadata_return_restores_the_whole_statement_and_preserves_prior_staging() {
        let ((), report) = run_async_under_lab(0x6d61_0045, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let (commit, cx, txcx) = (contexts.commit(), contexts.query(), contexts.txn());
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let basis = seed(&mut db, &commit, false).await;
            let params = GqlParameters::new();
            let mut txn = db.begin(&txcx).unwrap();
            let mut prefix = WriteBatch::new(R);
            prefix.set_vertex_property(VId(1), NOTE, Some(CanonicalScalar::Int(9)));
            txn.write(&mut db, prefix).unwrap();
            let digest = txn.staged_effect_digest().unwrap();
            let vertices = txn.vertices(&db).unwrap();
            let edges = txn.edges(&db).unwrap();
            for (case, (statement, catalog)) in [
                (
                    "CREATE (n:Foo {score:99}) RETURN properties([n][0]) AS props",
                    Catalog::MissingScore,
                ),
                (
                    "CREATE (a:Foo)-[r:R {score:99}]->(b:Bar) RETURN properties([r][0]) AS props LIMIT 0",
                    Catalog::MissingScore,
                ),
                (
                    "MERGE (n:Thing {score:10}) ON MATCH SET n.score=99 RETURN properties([n][0]) AS props LIMIT 0",
                    Catalog::MissingScore,
                ),
                (
                    "MERGE (n:Foo {score:99}) ON CREATE SET n.name='refused' RETURN properties([n][0]) AS props",
                    Catalog::MissingScore,
                ),
                (
                    "CREATE (n:Foo {score:99}) RETURN properties([n][0]) AS props, 1/0 AS refused",
                    Catalog::Full,
                ),
            ].into_iter().enumerate() {
                let failure = txn.query_write_engine(
                        &mut db,
                        &cx,
                        statement,
                        &params,
                        catalog,
                        R,
                        write_policy()
                    )
                    .unwrap_err();
                if case == 0 {
                    assert!(matches!(failure,
                        fgdb::QueryWriteError::Insert(GqlQueryError::Source(
                            fgdb_gql::GraphInsertQueryError::Returning(
                                fgdb_gql::GraphSetExecutionError::Source(
                                    fgdb::WriteTxnError::Read(ReadError::UnmappedProperty(SCORE))
                                )
                            )
                        ))
                    ), "{failure:?}");
                }
                assert_eq!(txn.staged_effect_digest().unwrap(), digest, "{statement}");
                assert_eq!(txn.vertices(&db).unwrap(), vertices, "{statement}");
                assert_eq!(txn.edges(&db).unwrap(), edges, "{statement}");
                assert_eq!(db.frontier().unwrap(), basis, "{statement}");
            }
            assert_eq!(
                cells(
                    txn.query_write_engine(
                        &mut db,
                        &cx,
                        "CREATE (n:Foo {score:77}) RETURN properties([n][0]) AS props",
                        &params,
                        Catalog::Full,
                        R,
                        write_policy()
                    )
                    .unwrap()
                ),
                vec![vec![map(&[("score", int(77))])]]
            );
            txn.commit(&mut db, &commit).await.unwrap();
            assert_eq!(
                cells(
                    db.query(
                        &cx,
                        "MATCH (n:Foo) RETURN n.score AS score",
                        &params,
                        Catalog::Full,
                        policy()
                    )
                    .unwrap()
                ),
                vec![vec![int(77)]]
            );
            assert_eq!(
                db.vertex(VId(1))
                    .unwrap()
                    .unwrap()
                    .props
                    .iter()
                    .find(|(key, _)| *key == NOTE)
                    .unwrap()
                    .1,
                CanonicalScalar::Int(9).into()
            );
            assert_eq!(txcx.outstanding_obligations(), 0);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }
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
