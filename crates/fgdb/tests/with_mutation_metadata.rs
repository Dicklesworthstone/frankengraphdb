//! Final metadata of WITH-fed mutations uses the frozen row and the mutation
//! owner. Expected maps/lists come from the fixture, never another query engine.

use asupersync::Budget;
use fgdb::{Database, DatabaseKeys, MemVfs, QueryResult, QueryValue, QueryWriteError, WriteBatch};
use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GraphSymbol, GraphSymbolKind,
    GraphSymbolResolver, GraphWriteIdentityRequest, GraphWriteProgramPolicy,
    PreparedGraphMutationQueryText, ReverseSymbolCatalog,
};
use fgdb_types::{
    CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, EId, PurposeContexts, QueryCx,
    TxnCx, VId,
};

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
            vec![(P, CanonicalScalar::Int(id)), (Q, CanonicalScalar::Int(id * 10))],
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
    GraphValue::map(values.iter().map(|&(name, value)| (name.into(), int(value))).collect())
        .unwrap()
}

fn rows(result: QueryResult, names: &[&str]) -> Vec<Vec<GraphValue>> {
    let QueryResult::Rows { columns, rows } = result else {
        panic!("a returning mutation must produce rows");
    };
    assert_eq!(columns, names.iter().map(|name| (*name).to_owned()).collect::<Vec<_>>());
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
        txcx, cx, commit, text, &GqlParameters::new(), Catalog, R, policy(limit), no_allocation,
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
        let mut db = Database::create_with_vfs(commit, vfs.clone(), &path, keys()).await.unwrap();
        db.write(commit, fixture()).await.unwrap();
        let result = apply(
            &mut db, commit, cx, txcx,
            "MATCH (n:Item) WITH 7 AS pad,n AS source \
             SET source.p=source.p+10,source.q=source.p*10 \
             RETURN source.p AS p,properties(source) AS props,keys(source) AS names ORDER BY p",
            2,
        ).await.unwrap();
        assert_eq!(rows(result, &["p", "props", "names"]), vec![
            vec![int(11), map(&[("p", 11), ("q", 10)]), names(&["p", "q"])],
            vec![int(12), map(&[("p", 12), ("q", 20)]), names(&["p", "q"])],
        ]);
        db.compact(commit).await.unwrap();
        drop(db);
        let db = Database::open_with_vfs(commit, vfs, &path, keys()).await.unwrap();
        let result = db.query(
            cx, "MATCH (n:Item) RETURN n.p AS p,n.q AS q ORDER BY p",
            &GqlParameters::new(), Catalog, query_policy(2),
        ).unwrap();
        assert_eq!(rows(result, &["p", "q"]), vec![vec![int(11), int(10)], vec![int(12), int(20)]]);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
}

#[test]
fn removed_fields_and_relationship_metadata_are_final_not_preimages() {
    run(async |commit, cx, txcx| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        db.write(commit, fixture()).await.unwrap();
        let result = apply(&mut db, commit, cx, txcx,
            "MATCH (n:Item) WITH n AS source REMOVE source.q \
             RETURN source.p AS p,properties(source) AS props,keys(source) AS names ORDER BY p", 2,
        ).await.unwrap();
        assert_eq!(rows(result, &["p", "props", "names"]), vec![
            vec![int(1), map(&[("p", 1)]), names(&["p"])],
            vec![int(2), map(&[("p", 2)]), names(&["p"])],
        ]);
        let result = apply(&mut db, commit, cx, txcx,
            "MATCH (a)-[r:R]->(b) WITH r AS edge SET edge.p=edge.p+10 \
             RETURN properties(edge) AS props,keys(edge) AS names", 1,
        ).await.unwrap();
        assert_eq!(rows(result, &["props", "names"]), vec![vec![map(&[("p", 17)]), names(&["p"])]]);
        let result = apply(&mut db, commit, cx, txcx,
            "MATCH (a)-[r:R]->(b) WITH r AS edge REMOVE edge.p \
             RETURN properties(edge) AS props,keys(edge) AS names", 1,
        ).await.unwrap();
        assert_eq!(rows(result, &["props", "names"]), vec![vec![map(&[]), names(&[])]]);
    });
}

#[test]
fn label_changes_and_nullable_metadata_keep_the_native_domain() {
    run(async |commit, cx, txcx| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        db.write(commit, fixture()).await.unwrap();
        let result = apply(&mut db, commit, cx, txcx,
            "MATCH (n:Item) WITH n AS source SET source:Tagged \
             RETURN source.p AS p,labels(source) AS tags ORDER BY p", 2,
        ).await.unwrap();
        assert_eq!(rows(result, &["p", "tags"]), vec![
            vec![int(1), names(&["Item", "Tagged"])],
            vec![int(2), names(&["Item", "Tagged"])],
        ]);
        let result = apply(&mut db, commit, cx, txcx,
            "MATCH (n:Tagged) WITH n AS source REMOVE source:Item \
             RETURN source.p AS p,labels(source) AS tags ORDER BY p", 2,
        ).await.unwrap();
        assert_eq!(rows(result, &["p", "tags"]), vec![
            vec![int(1), names(&["Tagged"])], vec![int(2), names(&["Tagged"])],
        ]);
        let result = apply(&mut db, commit, cx, txcx,
            "MATCH (a:Tagged) OPTIONAL MATCH (b:Missing) WITH a,b SET b.p=9 \
             RETURN a.p AS p,properties(b) AS props,keys(b) AS names,labels(b) AS tags ORDER BY p", 2,
        ).await.unwrap();
        assert_eq!(rows(result, &["p", "props", "names", "tags"]), vec![
            vec![int(1), null(), null(), null()], vec![int(2), null(), null(), null()],
        ]);
    });
}

#[test]
fn metadata_aggregates_count_occurrences_without_reapplying_updates() {
    run(async |commit, cx, txcx| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        db.write(commit, fixture()).await.unwrap();
        let result = apply(&mut db, commit, cx, txcx,
            "MATCH (a:Item),(b:Item) WITH 7 AS pad,a AS source SET source.p=source.p+10 \
             RETURN count(*) AS n,size(collect(properties(source))) AS copies, \
             sum(size(keys(source))) AS fields", 1,
        ).await.unwrap();
        assert_eq!(rows(result, &["n", "copies", "fields"]), vec![vec![int(4), int(4), int(8)]]);
        assert_eq!(read_p(&db, cx), vec![vec![int(11)], vec![int(12)]]);
    });
}

#[test]
fn metadata_does_not_hide_late_errors_or_limit_the_effect_set() {
    run(async |commit, cx, txcx| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        db.write(commit, fixture()).await.unwrap();
        let before = db.frontier().unwrap();
        let result = apply(&mut db, commit, cx, txcx,
            "MATCH (n:Item) WITH n AS source SET source.p=source.p+10 \
             RETURN properties(source) AS props,12/(source.p-12) AS bad LIMIT 0", 0,
        ).await;
        assert!(matches!(result, Err(QueryWriteError::Mutation(GqlQueryError::Source(_)))));
        assert_eq!(db.frontier().unwrap(), before);
        assert_eq!(read_p(&db, cx), vec![vec![int(1)], vec![int(2)]]);
        let result = apply(&mut db, commit, cx, txcx,
            "MATCH (n:Item) WITH n AS source SET source.p=source.p+10 \
             RETURN properties(source) AS props,labels(source) AS tags LIMIT 0", 0,
        ).await.unwrap();
        assert!(rows(result, &["props", "tags"]).is_empty());
        assert_eq!(read_p(&db, cx), vec![vec![int(11)], vec![int(12)]]);
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
            assert!(matches!(result, Err(QueryWriteError::Mutation(GqlQueryError::Source(_)))));
            assert_eq!(db.frontier().unwrap(), before);
            assert_eq!(read_p(&db, cx), vec![vec![int(1)], vec![int(2)]]);
        }
        let result = apply(&mut db, commit, cx, txcx,
            "MATCH (n:Item) WITH n AS source DETACH DELETE source RETURN 1 AS value LIMIT 0", 0,
        ).await.unwrap();
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
        let result = apply(&mut db, commit, cx, txcx,
            "MATCH (n:Item) WITH n AS source SET source.p=source.p+10 \
             RETURN source.p AS p,[source IN [{z:1}] | keys(source)] AS local, \
             keys({z:source.p}) AS literal ORDER BY p", 2,
        ).await.unwrap();
        let local = GraphValue::List(vec![names(&["z"])].into_boxed_slice());
        assert_eq!(rows(result, &["p", "local", "literal"]), vec![
            vec![int(11), local.clone(), names(&["z"])],
            vec![int(12), local, names(&["z"])],
        ]);
    });
}

#[path = "with_mutation_metadata/map_projections.rs"]
mod map_projections;
