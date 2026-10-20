//! Public returning writes must dispatch every property-target shape already
//! accepted by the native mutation parser, without falling back to a script.

use asupersync::Budget;
use fgdb::{Database, DatabaseKeys, MemVfs, QueryResult, QueryValue, QueryWriteError, WriteBatch};
use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GraphSymbol, GraphSymbolKind,
    GraphWriteIdentityRequest, GraphWriteProgramPolicy,
};
use fgdb_types::{
    CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, EId, PurposeContexts, QueryCx,
    TxnCx, VId,
};

const R: RelationId = RelationId(1);
const ITEM: LabelId = LabelId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const W: PropertyKeyId = PropertyKeyId(2);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x61; 32],
        DatabaseSecurityNamespaceId([0x62; 32]),
        [0x63; 32],
    )
}

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Label, "Item") => Some(GraphSymbol::Label(ITEM)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "w") => Some(GraphSymbol::Property(W)),
        _ => None,
    }
}

fn policy(rows: u64) -> GraphWriteProgramPolicy {
    GraphWriteProgramPolicy::new(
        GqlQueryPolicy::new(1_000, rows, 1_000_000, 1_000_000),
        100,
        100,
        100,
    )
}

fn no_allocation(_: GraphWriteIdentityRequest) -> Result<ElementId, ()> {
    panic!("SET and REMOVE cannot allocate identities")
}

fn int(value: i64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(value))
}

fn rows(result: QueryResult, names: &[&str]) -> Vec<Vec<GraphValue>> {
    let QueryResult::Rows { columns, rows } = result else {
        panic!("RETURN must produce rows, not a script receipt");
    };
    assert_eq!(
        columns,
        names.iter().map(|name| (*name).to_owned()).collect::<Vec<_>>()
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

fn read(db: &Database<MemVfs>, cx: &QueryCx) -> Vec<Vec<GraphValue>> {
    rows(
        db.query(
            cx,
            "MATCH (n:Item) RETURN n.p AS p ORDER BY p",
            &GqlParameters::new(),
            symbols,
            GqlQueryPolicy::new(1_000, 100, 1_000_000, 1_000_000),
        )
        .unwrap(),
        &["p"],
    )
}

fn graph() -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    for id in 1..=3_i64 {
        batch.create_vertex(
            VId(id as u128),
            vec![ITEM],
            vec![(P, CanonicalScalar::Int(id))],
        );
    }
    for (edge, source, destination, weight) in [(11, 1, 2, 5), (12, 2, 3, 7)] {
        batch.add_edge(
            EId(edge),
            VId(source),
            VId(destination),
            vec![(W, CanonicalScalar::Int(weight))],
        );
    }
    batch
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
fn parenthesized_vertex_and_edge_updates_return_final_values_and_survive_reopen() {
    run(async |commit, cx, txcx| {
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        db.write(commit, graph()).await.unwrap();
        let result = db
            .query_write(
                txcx,
                cx,
                commit,
                "MATCH (n:Item) SET /* target */ (n).p = n.p+10 \
                 RETURN n.p AS p ORDER BY p LIMIT 1",
                &GqlParameters::new(),
                symbols,
                R,
                policy(1),
                no_allocation,
            )
            .await
            .unwrap();
        assert_eq!(rows(result, &["p"]), vec![vec![int(11)]]);
        // Paging limits the answer, never the three effects.
        let expected = vec![vec![int(11)], vec![int(12)], vec![int(13)]];
        assert_eq!(read(&db, cx), expected);
        let result = db
            .query_write(
                txcx,
                cx,
                commit,
                "MATCH (a)-[r:R]->(b) SET (r).w = r.w+10 RETURN r.w AS w ORDER BY w",
                &GqlParameters::new(),
                symbols,
                R,
                policy(2),
                no_allocation,
            )
            .await
            .unwrap();
        assert_eq!(rows(result, &["w"]), vec![vec![int(15)], vec![int(17)]]);
        db.compact(commit).await.unwrap();
        drop(db);
        let db = Database::open_with_vfs(commit, vfs, &path, keys())
            .await
            .unwrap();
        assert_eq!(read(&db, cx), expected);
        let result = db
            .query(
                cx,
                "MATCH (a)-[r:R]->(b) RETURN r.w AS w ORDER BY w",
                &GqlParameters::new(),
                symbols,
                GqlQueryPolicy::new(1_000, 2, 1_000_000, 1_000_000),
            )
            .unwrap();
        assert_eq!(rows(result, &["w"]), vec![vec![int(15)], vec![int(17)]]);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
}

#[test]
fn parenthesized_late_failure_keeps_the_prior_prefix_and_remove_returns_null() {
    run(async |commit, cx, txcx| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        db.write(commit, graph()).await.unwrap();
        let before = db.frontier().unwrap();
        let mut txn = db.begin(txcx).unwrap();
        let mut prefix = WriteBatch::new(R);
        prefix.create_vertex(VId(99), vec![], vec![(P, CanonicalScalar::Int(99))]);
        txn.write(&mut db, prefix).unwrap();
        let digest = txn.staged_effect_digest().unwrap();
        let failure = txn.query_write(
            &mut db,
            cx,
            "MATCH (n:Item) SET (n).p = n.p+10 \
             RETURN 12/(n.p-12) AS bad LIMIT 0",
            &GqlParameters::new(),
            symbols,
            R,
            policy(0),
            no_allocation,
        );
        assert!(matches!(
            failure,
            Err(QueryWriteError::Mutation(GqlQueryError::Source(_)))
        ));
        assert_eq!(txn.staged_effect_digest().unwrap(), digest);
        assert_eq!(db.frontier().unwrap(), before);
        assert_eq!(
            read(&db, cx),
            vec![vec![int(1)], vec![int(2)], vec![int(3)]]
        );
        let removed = txn
            .query_write(
                &mut db,
                cx,
                "MATCH (n:Item) REMOVE (n).p RETURN n.p AS p",
                &GqlParameters::new(),
                symbols,
                R,
                policy(3),
                no_allocation,
            )
            .unwrap();
        let empty = GraphValue::Scalar(CanonicalScalar::Null);
        assert_eq!(rows(removed, &["p"]), vec![vec![empty.clone()]; 3]);
        let removed_edges = txn
            .query_write(
                &mut db,
                cx,
                "MATCH (a)-[r:R]->(b) REMOVE (r).w RETURN r.w AS w",
                &GqlParameters::new(),
                symbols,
                R,
                policy(2),
                no_allocation,
            )
            .unwrap();
        assert_eq!(rows(removed_edges, &["w"]), vec![vec![empty.clone()]; 2]);
        txn.finish(&mut db, commit).await.unwrap();
        assert!(db.vertex(VId(99)).unwrap().is_some());
        assert_eq!(read(&db, cx), vec![vec![empty]; 3]);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
}

#[test]
fn parenthesized_dispatch_keeps_preparation_and_budget_refusals_atomic() {
    run(async |commit, cx, txcx| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        db.write(commit, graph()).await.unwrap();
        let before = db.frontier().unwrap();
        for text in [
            "MATCH (n:Item) SET (n).p = 1 RETURN n; CREATE (m)",
            "MATCH (n:Item) SET (missing).p = 1 RETURN n",
        ] {
            let mut calls = 0;
            let failure = db
                .query_write(
                    txcx,
                    cx,
                    commit,
                    text,
                    &GqlParameters::new(),
                    |kind, name: &str| {
                        calls += 1;
                        symbols(kind, name)
                    },
                    R,
                    policy(3),
                    no_allocation,
                )
                .await;
            assert!(
                matches!(failure, Err(QueryWriteError::MutationText(_))),
                "{text}"
            );
            assert_eq!(
                calls, 0,
                "framing and target errors must precede catalog access"
            );
            assert_eq!(db.frontier().unwrap(), before);
        }
        let text = "MATCH (n:Item) SET (n).p = n.p+10 RETURN n.p AS p";
        for allowance in [
            GraphWriteProgramPolicy::new(
                GqlQueryPolicy::new(1_000, 3, 0, 1_000_000),
                100,
                100,
                100,
            ),
            policy(0),
        ] {
            let failure = db
                .query_write(
                    txcx,
                    cx,
                    commit,
                    text,
                    &GqlParameters::new(),
                    symbols,
                    R,
                    allowance,
                    no_allocation,
                )
                .await;
            assert!(matches!(failure, Err(QueryWriteError::Mutation(_))));
            assert_eq!(db.frontier().unwrap(), before);
            assert_eq!(
                read(&db, cx),
                vec![vec![int(1)], vec![int(2)], vec![int(3)]]
            );
        }
        // A successful zero-output control proves the native LIMIT 0 route,
        // and all updates still happen once after the two refused attempts.
        let result = db
            .query_write(
                txcx,
                cx,
                commit,
                &format!("{text} LIMIT 0"),
                &GqlParameters::new(),
                symbols,
                R,
                policy(0),
                no_allocation,
            )
            .await
            .unwrap();
        assert!(rows(result, &["p"]).is_empty());
        assert_eq!(
            read(&db, cx),
            vec![vec![int(11)], vec![int(12)], vec![int(13)]]
        );
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
}
