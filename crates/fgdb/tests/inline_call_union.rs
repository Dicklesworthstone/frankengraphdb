//! Returning CALL unions use every branch's ordinary snapshot, transaction,
//! capability and insertion owner. No branch may escape through a live requery.

use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb::{Database, DatabaseKeys, MemVfs, PreparedNativeRead, QueryResult, WriteBatch};
use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId, SchemaEpoch};
use fgdb_gql::insertion::{GraphInsertPolicy, GraphInsertRequest};
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind, PreparedGraphInsertQueryText,
    PreparedGraphInsertText,
};
use fgdb_types::{
    CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId,
};
use fgdb_warden::{Authority, Grant, QueryLimits, Scope};
use std::cell::Cell;

const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0xb7; 32]);
const L: LabelId = LabelId(1);
const COPY: LabelId = LabelId(2);
const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const P: PropertyKeyId = PropertyKeyId(1);
const QUERY: &str = "MATCH (n:L) WHERE n.p=10 CALL (n) { \
    MATCH (n)-[:R]->(m) RETURN m UNION ALL MATCH (n)-[:S]->(m) RETURN m \
    } RETURN m.p AS p ORDER BY p";

fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0xb6; 32], NS, [0xb8; 32])
}

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "L") => Some(GraphSymbol::Label(L)),
        (GraphSymbolKind::Label, "Copy") => Some(GraphSymbol::Label(COPY)),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Relation, "S") => Some(GraphSymbol::Relation(S)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    }
}

fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(1_000, 1_000, 1_000_000, 100_000)
}

fn values(result: QueryResult) -> Vec<i128> {
    let QueryResult::Rows { columns, rows } = result else {
        panic!("expected returning CALL rows");
    };
    assert_eq!(columns, ["p"]);
    rows.into_iter()
        .map(|row| row[0].as_integer().unwrap())
        .collect()
}

async fn database(cx: &CommitCx) -> Database<MemVfs> {
    let mut db = Database::open_memory(cx, keys()).await.unwrap();
    let mut first = WriteBatch::new(R);
    for id in 1..=4 {
        first.create_vertex(
            VId(id),
            vec![if id == 4 { LabelId(99) } else { L }],
            vec![(P, CanonicalScalar::Int(id as i64 * 10))],
        );
    }
    first.add_edge(EId(1), VId(1), VId(2), vec![]);
    first.add_edge(EId(2), VId(1), VId(2), vec![]);
    first.add_edge(EId(3), VId(1), VId(4), vec![]);
    let mut second = WriteBatch::new(S);
    second.add_edge(EId(4), VId(1), VId(3), vec![]);
    db.write_atomic(cx, vec![first, second]).await.unwrap();
    db
}

#[test]
fn returning_union_preserves_pinned_and_staged_reads_and_filters_every_authorized_branch() {
    let ((), report) = run_async_under_lab(0xca11_0011, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, cx, txcx) = (contexts.commit(), contexts.query(), contexts.txn());
        let mut db = database(&commit).await;
        let basis = db.frontier().unwrap();
        let pinned = db.read_session().unwrap();
        let args = GqlParameters::new();
        let prepared = PreparedNativeRead::prepare(QUERY, &args, symbols).unwrap();
        let before = vec![20, 20, 30, 40];
        assert_eq!(
            values(prepared.execute(&db, &cx, &args, policy()).unwrap()),
            before
        );
        assert_eq!(
            values(db.query(&cx, QUERY, &args, symbols, policy()).unwrap()),
            before
        );

        let issuer =
            Authority::new(AuthKey::from_seed(0xca11), NS, "graph", SchemaEpoch(0), 1).unwrap();
        for (relations, expected) in [
            (vec![R, S], vec![20, 20, 30]),
            (vec![R], vec![20, 20]),
            (vec![S], vec![30]),
        ] {
            let mut grant = Grant::read_only(
                "main",
                1_000,
                QueryLimits {
                    max_nodes: 1_000,
                    max_work: 1_000_000,
                    max_rows: 1_000,
                },
            );
            grant.labels = Scope::only([L]);
            grant.relations = Scope::only(relations);
            grant.properties = Scope::only([P]);
            let token = issuer.issue_at(&grant, 100).unwrap();
            assert_eq!(
                values(
                    prepared
                        .execute_authorized(
                            &db,
                            &cx,
                            &issuer,
                            &token,
                            "main",
                            &args,
                            policy(),
                            || 100,
                        )
                        .unwrap()
                ),
                expected
            );
            assert_eq!(
                values(
                    db.query_authorized(
                        &cx,
                        &issuer,
                        &token,
                        "main",
                        QUERY,
                        &args,
                        symbols,
                        policy(),
                        || 100,
                    )
                    .unwrap()
                ),
                expected
            );
        }

        let mut txn = db.begin(&txcx).unwrap();
        let mut change = WriteBatch::new(R);
        change.delete_edge(EId(1));
        change.set_vertex_property(VId(3), P, Some(CanonicalScalar::Int(35)));
        txn.write(&mut db, change).unwrap();
        let after = vec![20, 35, 40];
        assert_eq!(
            values(
                prepared
                    .execute_in_transaction(&txn, &db, &cx, &args, policy())
                    .unwrap()
            ),
            after
        );
        assert_eq!(
            values(prepared.execute(&db, &cx, &args, policy()).unwrap()),
            before
        );
        assert_eq!(db.frontier().unwrap(), basis);
        txn.commit(&mut db, &commit).await.unwrap();
        assert_eq!(
            values(prepared.execute(&db, &cx, &args, policy()).unwrap()),
            after
        );
        assert_eq!(
            values(pinned.query(&cx, QUERY, &args, symbols, policy()).unwrap()),
            before
        );
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

fn allocator(request: GraphInsertRequest) -> Result<ElementId, &'static str> {
    Ok(match request {
        GraphInsertRequest::Vertex { row, vertex } => {
            ElementId::Vertex(VId(1_000 + row as u128 * 4 + vertex as u128))
        }
        GraphInsertRequest::Edge { row, edge } => {
            ElementId::Edge(EId(10_000 + row as u128 * 4 + edge as u128))
        }
    })
}

#[test]
fn returning_union_feeds_creation_and_a_late_branch_failure_allocates_nothing() {
    let ((), report) = run_async_under_lab(0xca11_0012, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, cx, txcx) = (contexts.commit(), contexts.query(), contexts.txn());
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let insertion_policy = GraphInsertPolicy::new(policy(), 100, 100);
        let insertion = PreparedGraphInsertText::prepare(
            "CALL { UNWIND [2,2] AS v RETURN v UNION ALL RETURN 3 AS v } CREATE (copy:Copy {p:v})",
            R,
            symbols,
        )
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
        let mut txn = db.begin(&txcx).unwrap();
        let stats = txn
            .execute_graph_insert_governed(&mut db, &cx, &insertion, insertion_policy, allocator)
            .unwrap();
        assert_eq!(stats.created_vertices, 3);
        txn.commit(&mut db, &commit).await.unwrap();
        assert_eq!(
            values(
                db.query(
                    &cx,
                    "MATCH (n:Copy) RETURN n.p AS p ORDER BY p",
                    &GqlParameters::new(),
                    symbols,
                    policy()
                )
                .unwrap()
            ),
            vec![2, 2, 3]
        );

        let failing = PreparedGraphInsertQueryText::prepare(
            "CALL { RETURN 1 AS v UNION ALL RETURN 1/0 AS v } CREATE (copy:Copy {p:v}) RETURN copy.p LIMIT 0",
            R, symbols,
        ).unwrap().bind_parameters(&GqlParameters::new()).unwrap();
        let mut txn = db.begin(&txcx).unwrap();
        let mut earlier = WriteBatch::new(R);
        earlier.create_vertex(VId(777), vec![], vec![]);
        txn.write(&mut db, earlier).unwrap();
        let digest = txn.staged_effect_digest().unwrap();
        let allocations = Cell::new(0);
        assert!(
            txn.execute_graph_insert_query_governed(
                &mut db,
                &cx,
                &failing,
                insertion_policy,
                |request| {
                    allocations.set(allocations.get() + 1);
                    allocator(request)
                }
            )
            .is_err()
        );
        assert_eq!(allocations.get(), 0);
        assert_eq!(txn.staged_effect_digest().unwrap(), digest);
        assert!(txn.vertex(&db, VId(777)).unwrap().is_some());
        txn.abort();
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
