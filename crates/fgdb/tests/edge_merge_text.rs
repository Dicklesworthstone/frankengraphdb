//! Native MATCH ... MERGE relationship text through the real autocommit engine.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, WriteBatch};
use fgdb_delta_types::{ElementId, PropertyKeyId, RelationId};
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphEdgeMergeOutcome, GraphEdgeMergePolicy,
    GraphSymbol, GraphSymbolKind, PreparedGraphEdgeMergeText,
};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, EId, EmbeddedTxnCompletion,
    PurposeContexts, VId};

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0x51; 32], DatabaseSecurityNamespaceId([0x52; 32]), [0x53; 32])
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    }
}
fn policy() -> GraphEdgeMergePolicy {
    GraphEdgeMergePolicy::new(GqlQueryPolicy::new(5_000, 5_000, 2_000_000, 2_000_000))
}
fn template() -> PreparedGraphEdgeMergeText {
    PreparedGraphEdgeMergeText::prepare(
        "MATCH (a),(b) WHERE a.p=$left AND b.p=$right MERGE (a)-[:R]->(b)", R, symbols,
    ).unwrap()
}
fn args(left: i64, right: i64) -> GqlParameters {
    GqlParameters::new().with_int64("left", left).unwrap().with_int64("right", right).unwrap()
}

#[test]
fn native_relationship_merge_creates_then_matches_without_second_commit() {
    let ((), report) = run_async_under_lab(0xed9e_2001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(R);
        for id in 1..=3_u128 {
            seed.create_vertex(VId(id), vec![], vec![(P, CanonicalScalar::Int(id as i64))]);
        }
        db.write(&commit, seed).await.unwrap();

        let template = template();
        let merge = template.bind_parameters(&args(1, 2)).unwrap();
        let (_, outcome, completion) = db.execute_graph_edge_merge_autocommit_governed(
            &txcx, &query, &commit, &merge, policy(),
            |_| Ok::<_, ()>(ElementId::Edge(EId(10))),
        ).await.unwrap();
        assert_eq!(outcome, GraphEdgeMergeOutcome::Created(EId(10)));
        assert!(matches!(completion, EmbeddedTxnCompletion::WriteCommitted { .. }));
        let edge = db.edge(EId(10)).unwrap().unwrap();
        assert_eq!((edge.entry.src, edge.entry.dst), (VId(1), VId(2)));

        let before = db.frontier().unwrap();
        let rebound = template.bind_parameters(&args(1, 2)).unwrap();
        let (_, outcome, completion) = db.execute_graph_edge_merge_autocommit_governed(
            &txcx, &query, &commit, &rebound, policy(),
            |_| -> Result<ElementId, ()> { panic!("existing relationship must not allocate") },
        ).await.unwrap();
        assert_eq!(outcome, GraphEdgeMergeOutcome::Matched(EId(10)));
        assert!(matches!(completion, EmbeddedTxnCompletion::ReadClosed { .. }));
        assert_eq!(db.frontier().unwrap(), before);

        let reverse = PreparedGraphEdgeMergeText::prepare(
            "MATCH (a),(b) WHERE a.p=$left AND b.p=$right MERGE (b)<-[:R]-(a)", R, symbols,
        ).unwrap().bind_parameters(&args(1, 2)).unwrap();
        let (_, outcome, completion) = db.execute_graph_edge_merge_autocommit_governed(
            &txcx, &query, &commit, &reverse, policy(),
            |_| -> Result<ElementId, ()> { panic!("reverse spelling must find same relationship") },
        ).await.unwrap();
        assert_eq!(outcome, GraphEdgeMergeOutcome::Matched(EId(10)));
        assert!(matches!(completion, EmbeddedTxnCompletion::ReadClosed { .. }));
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
