//! Final metadata of WITH-fed mutations uses the frozen row and the mutation
//! owner. Expected maps/lists come from the fixture, never another query engine.

use asupersync::Budget;
use asupersync::security::key::AuthKey;
use fgdb::{
    Database, DatabaseKeys, MemVfs, QueryResult, QueryValue, QueryWriteError, ReadError,
    WriteBatch, WriteTxnError,
};
use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GraphMutationError, GraphMutationPolicy,
    GraphMutationQueryError, GraphSetExecutionError, GraphSymbol, GraphSymbolKind,
    GraphSymbolResolver, GraphWriteIdentityRequest, GraphWriteProgramPolicy,
    PreparedGraphMutationQueryText, ReverseSymbolCatalog,
};
use fgdb_types::{
    CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, EId, PurposeContexts, QueryCx, TxnCx,
    VId,
};
use fgdb_warden::{Authority, Grant, QueryLimits, Rights, Scope};

const R: RelationId = RelationId(1);
const ITEM: LabelId = LabelId(1);
const TAGGED: LabelId = LabelId(2);
const MISSING: LabelId = LabelId(3);
const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);
const BASE: u128 = 1_u128 << 100;

struct Catalog;

impl GraphSymbolResolver for Catalog {
    fn resolve_symbol(&mut self, kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        match (kind, name) {
            (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
            (GraphSymbolKind::Label, "Item") => Some(GraphSymbol::Label(ITEM)),
            (GraphSymbolKind::Label, "Tagged") => Some(GraphSymbol::Label(TAGGED)),
            (GraphSymbolKind::Label, "Missing") => Some(GraphSymbol::Label(MISSING)),
            (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
            (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
            _ => None,
        }
    }

    fn reverse_catalog(&self) -> Option<ReverseSymbolCatalog> {
        // Enumeration requires the complete host catalog, including fields
        // not mentioned by the statement. It must never silently omit them.
        let mut catalog = ReverseSymbolCatalog::new();
        catalog.insert_label(ITEM, "Item");
        catalog.insert_label(TAGGED, "Tagged");
        catalog.insert_label(MISSING, "Missing");
        catalog.insert_relation(R, "R");
        catalog.insert_property(P, "p");
        catalog.insert_property(Q, "q");
        Some(catalog)
    }
}

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x61; 32],
        DatabaseSecurityNamespaceId([0x62; 32]),
        [0x63; 32],
    )
}

fn query_policy(rows: u64) -> GqlQueryPolicy {
    GqlQueryPolicy::new(1_000, rows, 1_000_000, 1_000_000)
}

fn policy(rows: u64) -> GraphWriteProgramPolicy {
    GraphWriteProgramPolicy::new(query_policy(rows), 100, 100, 100)
}

fn no_allocation(_: GraphWriteIdentityRequest) -> Result<ElementId, ()> {
    panic!("SET/REMOVE/DELETE must not allocate identities")
}

fn fixture() -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    for id in 1..=2_i64 {
        batch.create_vertex(
            VId(BASE + id as u128),
            vec![ITEM],
            vec![
                (P, CanonicalScalar::Int(id)),
                (Q, CanonicalScalar::Int(id * 10)),
            ],
        );
    }
    batch.add_edge(
        EId(BASE + 10),
        VId(BASE + 1),
        VId(BASE + 2),
        vec![(P, CanonicalScalar::Int(7))],
    );
    batch
}

fn int(value: i64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(value))
}

fn null() -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Null)
}

fn text(value: &str) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::ucs_basic_text(value).unwrap())
}

fn names(values: &[&str]) -> GraphValue {
    GraphValue::List(values.iter().map(|name| text(name)).collect())
}

fn map(values: &[(&str, i64)]) -> GraphValue {
    GraphValue::map(
        values
            .iter()
            .map(|&(name, value)| (name.into(), int(value)))
            .collect(),
    )
    .unwrap()
}

fn rows(result: QueryResult, names: &[&str]) -> Vec<Vec<GraphValue>> {
    let QueryResult::Rows { columns, rows } = result else {
        panic!("a returning mutation must produce rows");
    };
    assert_eq!(
        columns,
        names
            .iter()
            .map(|name| (*name).to_owned())
            .collect::<Vec<_>>()
    );
    rows.into_iter()
        .map(|row| {
            row.into_iter()
                .map(|cell| match cell {
                    QueryValue::Value(value) => value,
                    _ => panic!("expected an ordinary native value"),
                })
                .collect()
        })
        .collect()
}

#[allow(clippy::result_large_err)]
async fn apply(
    db: &mut Database<MemVfs>,
    commit: &CommitCx,
    cx: &QueryCx,
    txcx: &TxnCx,
    text: &str,
    limit: u64,
) -> Result<QueryResult, QueryWriteError<()>> {
    db.query_write(
        txcx,
        cx,
        commit,
        text,
        &GqlParameters::new(),
        Catalog,
        R,
        policy(limit),
        no_allocation,
    )
    .await
}

fn read_p(db: &Database<MemVfs>, cx: &QueryCx) -> Vec<Vec<GraphValue>> {
    rows(
        db.query(
            cx,
            "MATCH (n:Item) RETURN n.p AS p ORDER BY p",
            &GqlParameters::new(),
            Catalog,
            query_policy(10),
        )
        .unwrap(),
        &["p"],
    )
}

fn run<T>(test: impl AsyncFnOnce(&CommitCx, &QueryCx, &TxnCx) -> T) -> T {
    let runtime = fgdb::runtime_builder().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    let commit = contexts.commit();
    let query = contexts.query();
    let txn = contexts.txn();
    runtime.block_on(test(&commit, &query, &txn))
}

#[test]
fn row_metadata_returns_simultaneous_final_values_and_survives_reopen() {
    run(async |commit, cx, txcx| {
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        db.write(commit, fixture()).await.unwrap();
        let result = apply(
            &mut db,
            commit,
            cx,
            txcx,
            "MATCH (n:Item) WITH 7 AS pad,n AS source \
             SET source.p=source.p+10,source.q=source.p*10 \
             RETURN source.p AS p,properties(source) AS props,keys(source) AS names ORDER BY p",
            2,
        )
        .await
        .unwrap();
        assert_eq!(
            rows(result, &["p", "props", "names"]),
            vec![
                vec![int(11), map(&[("p", 11), ("q", 10)]), names(&["p", "q"])],
                vec![int(12), map(&[("p", 12), ("q", 20)]), names(&["p", "q"])],
            ]
        );
        db.compact(commit).await.unwrap();
        drop(db);
        let db = Database::open_with_vfs(commit, vfs, &path, keys())
            .await
            .unwrap();
        let result = db
            .query(
                cx,
                "MATCH (n:Item) RETURN n.p AS p,n.q AS q ORDER BY p",
                &GqlParameters::new(),
                Catalog,
                query_policy(2),
            )
            .unwrap();
        assert_eq!(
            rows(result, &["p", "q"]),
            vec![vec![int(11), int(10)], vec![int(12), int(20)]]
        );
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
}

#[test]
fn removed_fields_and_relationship_metadata_are_final_not_preimages() {
    run(async |commit, cx, txcx| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        db.write(commit, fixture()).await.unwrap();
        let result = apply(
            &mut db,
            commit,
            cx,
            txcx,
            "MATCH (n:Item) WITH n AS source REMOVE source.q \
             RETURN source.p AS p,properties(source) AS props,keys(source) AS names ORDER BY p",
            2,
        )
        .await
        .unwrap();
        assert_eq!(
            rows(result, &["p", "props", "names"]),
            vec![
                vec![int(1), map(&[("p", 1)]), names(&["p"])],
                vec![int(2), map(&[("p", 2)]), names(&["p"])],
            ]
        );
        let result = apply(
            &mut db,
            commit,
            cx,
            txcx,
            "MATCH (a)-[r:R]->(b) WITH r AS edge SET edge.p=edge.p+10 \
             RETURN properties(edge) AS props,keys(edge) AS names",
            1,
        )
        .await
        .unwrap();
        assert_eq!(
            rows(result, &["props", "names"]),
            vec![vec![map(&[("p", 17)]), names(&["p"])]]
        );
        let result = apply(
            &mut db,
            commit,
            cx,
            txcx,
            "MATCH (a)-[r:R]->(b) WITH r AS edge REMOVE edge.p \
             RETURN properties(edge) AS props,keys(edge) AS names",
            1,
        )
        .await
        .unwrap();
        assert_eq!(
            rows(result, &["props", "names"]),
            vec![vec![map(&[]), names(&[])]]
        );
    });
}

#[test]
fn label_changes_and_nullable_metadata_keep_the_native_domain() {
    run(async |commit, cx, txcx| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        db.write(commit, fixture()).await.unwrap();
        let result = apply(
            &mut db,
            commit,
            cx,
            txcx,
            "MATCH (n:Item) WITH n AS source SET source:Tagged \
             RETURN source.p AS p,labels(source) AS tags ORDER BY p",
            2,
        )
        .await
        .unwrap();
        assert_eq!(
            rows(result, &["p", "tags"]),
            vec![
                vec![int(1), names(&["Item", "Tagged"])],
                vec![int(2), names(&["Item", "Tagged"])],
            ]
        );
        let result = apply(
            &mut db,
            commit,
            cx,
            txcx,
            "MATCH (n:Tagged) WITH n AS source REMOVE source:Item \
             RETURN source.p AS p,labels(source) AS tags ORDER BY p",
            2,
        )
        .await
        .unwrap();
        assert_eq!(
            rows(result, &["p", "tags"]),
            vec![
                vec![int(1), names(&["Tagged"])],
                vec![int(2), names(&["Tagged"])],
            ]
        );
        let result = apply(
            &mut db,
            commit,
            cx,
            txcx,
            "MATCH (a:Tagged) OPTIONAL MATCH (b:Missing) WITH a,b SET b.p=9 \
             RETURN a.p AS p,properties(b) AS props,keys(b) AS names,labels(b) AS tags ORDER BY p",
            2,
        )
        .await
        .unwrap();
        assert_eq!(
            rows(result, &["p", "props", "names", "tags"]),
            vec![
                vec![int(1), null(), null(), null()],
                vec![int(2), null(), null(), null()],
            ]
        );
    });
}

#[test]
fn metadata_aggregates_count_occurrences_without_reapplying_updates() {
    run(async |commit, cx, txcx| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        db.write(commit, fixture()).await.unwrap();
        let result = apply(
            &mut db,
            commit,
            cx,
            txcx,
            "MATCH (a:Item),(b:Item) WITH 7 AS pad,a AS source SET source.p=source.p+10 \
             RETURN count(*) AS n,size(collect(properties(source))) AS copies, \
             sum(size(keys(source))) AS fields",
            1,
        )
        .await
        .unwrap();
        assert_eq!(
            rows(result, &["n", "copies", "fields"]),
            vec![vec![int(4), int(4), int(8)]]
        );
        assert_eq!(read_p(&db, cx), vec![vec![int(11)], vec![int(12)]]);
    });
}

#[test]
fn metadata_does_not_hide_late_errors_or_limit_the_effect_set() {
    run(async |commit, cx, txcx| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        db.write(commit, fixture()).await.unwrap();
        let before = db.frontier().unwrap();
        let result = apply(
            &mut db,
            commit,
            cx,
            txcx,
            "MATCH (n:Item) WITH n AS source SET source.p=source.p+10 \
             RETURN properties(source) AS props,12/(source.p-12) AS bad LIMIT 0",
            0,
        )
        .await;
        assert!(matches!(
            result,
            Err(QueryWriteError::Mutation(GqlQueryError::Source(_)))
        ));
        assert_eq!(db.frontier().unwrap(), before);
        assert_eq!(read_p(&db, cx), vec![vec![int(1)], vec![int(2)]]);
        let result = apply(
            &mut db,
            commit,
            cx,
            txcx,
            "MATCH (n:Item) WITH n AS source SET source.p=source.p+10 \
             RETURN properties(source) AS props,labels(source) AS tags LIMIT 0",
            0,
        )
        .await
        .unwrap();
        assert!(rows(result, &["props", "tags"]).is_empty());
        assert_eq!(read_p(&db, cx), vec![vec![int(11)], vec![int(12)]]);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
}

#[test]
fn multipart_metadata_refuses_an_unlisted_property_without_publishing_changes() {
    struct MissingQ;
    impl GraphSymbolResolver for MissingQ {
        fn resolve_symbol(&mut self, kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
            Catalog.resolve_symbol(kind, name)
        }

        fn reverse_catalog(&self) -> Option<ReverseSymbolCatalog> {
            let mut catalog = Catalog.reverse_catalog()?;
            catalog.properties.remove(&Q);
            Some(catalog)
        }
    }
    run(async |commit, cx, txcx| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        db.write(commit, fixture()).await.unwrap();
        let before = db.frontier().unwrap();
        let statement = "WITH 1 AS unit MATCH (n:Item) WITH n \
                         SET n.p=n.p+10,n:Tagged RETURN properties(n) AS props,labels(n) AS tags";
        // The catalog names every explicit field but omits the unmentioned,
        // visible q. Enumeration must report that exact missing identity,
        // never infer an absent field from the statement's own names.
        let query = PreparedGraphMutationQueryText::prepare(statement, R, MissingQ)
            .unwrap()
            .bind_parameters(&GqlParameters::new())
            .unwrap();
        let result = db
            .execute_graph_mutation_query_autocommit_governed(
                txcx,
                cx,
                commit,
                &query,
                GraphMutationPolicy::new(query_policy(2), 100),
            )
            .await;
        assert!(matches!(
            result,
            Err(GqlQueryError::Source(GraphMutationQueryError::Mutation(
                GraphMutationError::InputRelation(GraphSetExecutionError::Source(
                    WriteTxnError::Read(ReadError::UnmappedProperty(Q))
                ))
            )))
        ));
        assert_eq!(db.frontier().unwrap(), before);
        assert_eq!(read_p(&db, cx), vec![vec![int(1)], vec![int(2)]]);
        assert!(
            db.vertices()
                .unwrap()
                .iter()
                .all(|vertex| vertex.labels == [ITEM])
        );
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
}

#[test]
fn deleted_metadata_reads_fail_without_publishing_a_delete_prefix() {
    run(async |commit, cx, txcx| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        db.write(commit, fixture()).await.unwrap();
        let before = db.frontier().unwrap();
        for function in ["properties", "keys", "labels"] {
            let statement = format!(
                "MATCH (n:Item) WITH n AS source DETACH DELETE source \
                 RETURN {function}(source) AS value LIMIT 0"
            );
            let result = apply(&mut db, commit, cx, txcx, &statement, 0).await;
            assert!(matches!(
                result,
                Err(QueryWriteError::Mutation(GqlQueryError::Source(_)))
            ));
            assert_eq!(db.frontier().unwrap(), before);
            assert_eq!(read_p(&db, cx), vec![vec![int(1)], vec![int(2)]]);
        }
        let result = apply(
            &mut db,
            commit,
            cx,
            txcx,
            "MATCH (n:Item) WITH n AS source DETACH DELETE source RETURN 1 AS value LIMIT 0",
            0,
        )
        .await
        .unwrap();
        assert!(rows(result, &["value"]).is_empty());
        assert!(read_p(&db, cx).is_empty());
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
}

#[test]
fn metadata_respects_scope_and_map_locals_instead_of_reviving_graph_names() {
    for statement in [
        "MATCH (n) WITH n AS kept SET kept.p=1 RETURN properties(n)",
        "MATCH (n) WITH n AS kept SET kept.p=1 RETURN labels(n)",
        "MATCH (n) WITH n SET n.p=1 RETURN properties(__fg_boundary_0)",
        "MATCH (a)-[r:R]->(b) WITH a,r DETACH DELETE a RETURN properties(r)",
        "MATCH (n) WITH n SET n.p=1 WITH 0 AS n RETURN labels(n)",
        "MATCH (n) WITH n SET n.p=1 WITH [0] AS n RETURN labels(n)",
        "MATCH (a)-[r:R]->(b) WITH a,r SET a.p=1 WITH 0 AS r RETURN type(r)",
    ] {
        let mut calls = 0;
        let result = PreparedGraphMutationQueryText::prepare(statement, R, |kind, name: &str| {
            calls += 1;
            Catalog.resolve_symbol(kind, name)
        });
        assert!(result.is_err(), "{statement}");
        assert_eq!(calls, 0, "{statement}");
    }
    run(async |commit, cx, txcx| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        db.write(commit, fixture()).await.unwrap();
        let result = apply(
            &mut db,
            commit,
            cx,
            txcx,
            "MATCH (n:Item) WITH n AS source SET source.p=source.p+10 \
             RETURN source.p AS p,[source IN [{z:1}] | keys(source)] AS local, \
             keys({z:source.p}) AS literal ORDER BY p",
            2,
        )
        .await
        .unwrap();
        let local = GraphValue::List(vec![names(&["z"])].into_boxed_slice());
        assert_eq!(
            rows(result, &["p", "local", "literal"]),
            vec![
                vec![int(11), local.clone(), names(&["z"])],
                vec![int(12), local, names(&["z"])],
            ]
        );
        let result = apply(
            &mut db,
            commit,
            cx,
            txcx,
            "MATCH (n:Item) WITH n SET n.p=n.p+1 \
             WITH NULL AS n RETURN labels(n) AS l,type(n) AS t",
            2,
        )
        .await
        .unwrap();
        let null = GraphValue::Scalar(CanonicalScalar::Null);
        assert_eq!(rows(result, &["l", "t"]), vec![vec![null.clone(), null]; 2]);
    });
}

async fn apply_continuation(
    db: &mut Database<MemVfs>,
    contexts: (&CommitCx, &QueryCx, &TxnCx),
    statement: &str,
    columns: &[&str],
    authorized: bool,
    limit: u64,
) -> Result<Vec<Vec<GraphValue>>, String> {
    let (commit, cx, txcx) = contexts;
    if !authorized {
        return apply(db, commit, cx, txcx, statement, limit)
            .await
            .map(|result| rows(result, columns))
            .map_err(|error| format!("{error:?}"));
    }
    let issuer = Authority::new(
        AuthKey::from_seed(0x7c1b),
        DatabaseSecurityNamespaceId([0x62; 32]),
        "graph",
        fgdb_delta_types::SchemaEpoch(1),
        1,
    )
    .unwrap();
    let grant = Grant {
        branch: "main".into(),
        labels: Scope::only([ITEM, TAGGED]),
        relations: Scope::only([R]),
        // The fixture's q is deliberately hidden, including inside maps.
        properties: Scope::only([P]),
        rights: Rights::ReadWrite,
        limits: QueryLimits {
            max_nodes: 10_000,
            max_work: 1_000_000,
            max_rows: 100,
        },
        expires_at_ms: 10_000,
    };
    let token = issuer.issue_at(&grant, 100).unwrap();
    let query = PreparedGraphMutationQueryText::prepare(statement, R, Catalog)
        .map_err(|error| format!("{error:?}"))?
        .bind_parameters(&GqlParameters::new())
        .map_err(|error| format!("{error:?}"))?;
    db.execute_graph_mutation_query_authorized(
        txcx,
        cx,
        commit,
        &issuer,
        &token,
        "main",
        &query,
        GraphMutationPolicy::new(query_policy(limit), 100),
        || 100,
    )
    .await
    .map(|(_, result, _)| {
        result
            .value
            .into_iter()
            .map(|row| row.values().to_vec())
            .collect()
    })
    .map_err(|error| format!("{error:?}"))
}

#[test]
fn optional_and_unwind_continuations_return_final_scoped_maps_and_rollback() {
    run(async |commit, cx, txcx| {
        let prefixes = [
            (
                "MATCH (a:Item) WITH a,a AS b MATCH (a) UNWIND [0,0] AS occurrence",
                false,
            ),
            (
                "MATCH (a:Item) WITH a,[2] AS allowed \
                 OPTIONAL MATCH (a)-[:R]->(b) WHERE b.p IN allowed \
                 UNWIND [0,0] AS occurrence",
                true,
            ),
            (
                "WITH 1 AS unit MATCH (a:Item) WITH a,a AS b \
                 MATCH (a) UNWIND [0,0] AS occurrence",
                false,
            ),
        ];
        for authorized in [false, true] {
            for (prefix, optional) in prefixes {
                let mut db = Database::open_memory(commit, keys()).await.unwrap();
                db.write(commit, fixture()).await.unwrap();
                let before = db.frontier().unwrap();
                let statement = format!(
                    "{prefix} SET a.p=a.p+10,a:Tagged \
                     RETURN a.p AS p,a.q AS q,a{{.*}} AS props,keys(a) AS names, \
                     labels(a) AS tags,b.p AS peer,b{{.*}} AS peer_props ORDER BY p"
                );
                let columns = ["p", "q", "props", "names", "tags", "peer", "peer_props"];
                // A rejected delivery cannot publish a prefix of the changes.
                assert!(
                    apply_continuation(
                        &mut db,
                        (commit, cx, txcx),
                        &statement,
                        &columns,
                        authorized,
                        3,
                    )
                    .await
                    .is_err(),
                    "{statement}; authorized={authorized}"
                );
                assert_eq!(db.frontier().unwrap(), before);
                assert_eq!(read_p(&db, cx), vec![vec![int(1)], vec![int(2)]]);
                let failing = format!(
                    "{prefix} SET a.p=a.p+10 \
                     RETURN a{{.*}} AS props,12/(a.p-12) AS bad LIMIT 0"
                );
                assert!(
                    apply_continuation(
                        &mut db,
                        (commit, cx, txcx),
                        &failing,
                        &["props", "bad"],
                        authorized,
                        0,
                    )
                    .await
                    .is_err(),
                    "{failing}; authorized={authorized}"
                );
                assert_eq!(db.frontier().unwrap(), before);
                assert_eq!(read_p(&db, cx), vec![vec![int(1)], vec![int(2)]]);
                let actual = apply_continuation(
                    &mut db,
                    (commit, cx, txcx),
                    &statement,
                    &columns,
                    authorized,
                    4,
                )
                .await
                .unwrap_or_else(|error| panic!("{statement}; authorized={authorized}: {error}"));
                let properties = |id| {
                    if authorized {
                        map(&[("p", id + 10)])
                    } else {
                        map(&[("p", id + 10), ("q", id * 10)])
                    }
                };
                let expected = (1..=2)
                    .flat_map(|id| {
                        let peer = if optional {
                            (id == 1).then_some(2)
                        } else {
                            Some(id)
                        };
                        vec![
                            vec![
                                int(id + 10),
                                if authorized { null() } else { int(id * 10) },
                                properties(id),
                                if authorized {
                                    names(&["p"])
                                } else {
                                    names(&["p", "q"])
                                },
                                names(&["Item", "Tagged"]),
                                peer.map_or_else(null, |id| int(id + 10)),
                                peer.map_or_else(null, properties),
                            ];
                            2
                        ]
                    })
                    .collect::<Vec<_>>();
                assert_eq!(actual, expected, "{statement}; authorized={authorized}");
                assert_eq!(read_p(&db, cx), vec![vec![int(11)], vec![int(12)]]);
                assert_eq!(txcx.outstanding_obligations(), 0);
            }
        }
    });
}

#[test]
fn optional_apply_carries_relationship_type_and_final_maps_through_unwind() {
    run(async |commit, cx, txcx| {
        for authorized in [false, true] {
            let mut db = Database::open_memory(commit, keys()).await.unwrap();
            db.write(commit, fixture()).await.unwrap();
            let statement = "MATCH (a:Item) WITH a,[2] AS allowed \
                 OPTIONAL MATCH (a)-[r:R]->(b) WHERE b.p IN allowed \
                 UNWIND [0,0] AS occurrence SET r.p=r.p+10 \
                 RETURN a.p AS p,r.p AS edge_p,type(r) AS relation, \
                 r{.*} AS props,keys(r) AS names ORDER BY p";
            let actual = apply_continuation(
                &mut db,
                (commit, cx, txcx),
                statement,
                &["p", "edge_p", "relation", "props", "names"],
                authorized,
                4,
            )
            .await
            .unwrap_or_else(|error| panic!("{statement}; authorized={authorized}: {error}"));
            assert_eq!(
                actual,
                [
                    vec![int(1), int(17), text("R"), map(&[("p", 17)]), names(&["p"])],
                    vec![int(1), int(17), text("R"), map(&[("p", 17)]), names(&["p"])],
                    vec![int(2), null(), null(), null(), null()],
                    vec![int(2), null(), null(), null(), null()],
                ],
                "authorized={authorized}"
            );
            let result = db
                .query(
                    cx,
                    "MATCH (a)-[r:R]->(b) RETURN r.p AS p",
                    &GqlParameters::new(),
                    Catalog,
                    query_policy(1),
                )
                .unwrap();
            assert_eq!(rows(result, &["p"]), [vec![int(17)]]);
            assert_eq!(txcx.outstanding_obligations(), 0);
        }
    });
}

#[path = "with_mutation_metadata/map_projections.rs"]
mod map_projections;
