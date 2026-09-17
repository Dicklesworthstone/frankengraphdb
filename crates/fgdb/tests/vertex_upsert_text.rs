//! Native MERGE branch clauses through the real autocommit upsert path.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, WriteBatch};
use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId};
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind, GraphVertexMergeOutcome,
    GraphVertexMergePolicy, GraphVertexUpsertBranch, GraphVertexUpsertPolicy,
    PreparedGraphVertexUpsertText,
};
use fgdb_types::{
    CanonicalScalar, DatabaseSecurityNamespaceId, EmbeddedTxnCompletion, PurposeContexts, VId,
};

const R: RelationId = RelationId(1);
const PERSON: LabelId = LabelId(1);
const SEEN: LabelId = LabelId(2);
const CREATED: LabelId = LabelId(3);
const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);
fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x21; 32],
        DatabaseSecurityNamespaceId([0x22; 32]),
        [0x23; 32],
    )
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "Person") => Some(GraphSymbol::Label(PERSON)),
        (GraphSymbolKind::Label, "Seen") => Some(GraphSymbol::Label(SEEN)),
        (GraphSymbolKind::Label, "Created") => Some(GraphSymbol::Label(CREATED)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
        _ => None,
    }
}
fn policy() -> GraphVertexUpsertPolicy {
    GraphVertexUpsertPolicy::new(
        GraphVertexMergePolicy::new(GqlQueryPolicy::new(10_000, 10_000, 2_000_000, 2_000_000)),
        8,
    )
}
fn full_template() -> PreparedGraphVertexUpsertText {
    PreparedGraphVertexUpsertText::prepare(
        "MERGE (n:Person {p:$p}) ON MATCH SET n.q=$matched,n:Seen ON CREATE SET n.q=$created,n:Created",
        R, symbols,
    ).unwrap()
}
fn args(p: i64, matched: i64, created: i64) -> GqlParameters {
    GqlParameters::new()
        .with_int64("p", p)
        .unwrap()
        .with_int64("matched", matched)
        .unwrap()
        .with_int64("created", created)
        .unwrap()
}

#[test]
fn native_upsert_executes_the_selected_branch_and_publishes_once() {
    let ((), report) = run_async_under_lab(0x6e29_4001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(1), vec![PERSON], vec![(P, CanonicalScalar::Int(7))]);
        db.write(&commit, seed).await.unwrap();
        let template = full_template();

        let matched = template.bind_parameters(&args(7, 100, 200)).unwrap();
        let (stats, outcome, completion) = db
            .execute_graph_vertex_upsert_autocommit_governed(
                &txcx,
                &query,
                &commit,
                &matched,
                policy(),
                |_| -> Result<ElementId, ()> { panic!("matched upsert must not allocate") },
            )
            .await
            .unwrap();
        assert_eq!(stats.branch, GraphVertexUpsertBranch::Match);
        assert_eq!(outcome, GraphVertexMergeOutcome::Matched(VId(1)));
        assert!(matches!(
            completion,
            EmbeddedTxnCompletion::WriteCommitted { .. }
        ));
        let row = db.vertex(VId(1)).unwrap().unwrap();
        assert_eq!(row.labels, vec![PERSON, SEEN]);
        assert_eq!(
            row.props,
            vec![(P, CanonicalScalar::Int(7)), (Q, CanonicalScalar::Int(100))]
        );

        let created = template.bind_parameters(&args(9, 100, 200)).unwrap();
        let (stats, outcome, completion) = db
            .execute_graph_vertex_upsert_autocommit_governed(
                &txcx,
                &query,
                &commit,
                &created,
                policy(),
                |_| Ok::<_, ()>(ElementId::Vertex(VId(9))),
            )
            .await
            .unwrap();
        assert_eq!(stats.branch, GraphVertexUpsertBranch::Create);
        assert_eq!(outcome, GraphVertexMergeOutcome::Created(VId(9)));
        assert!(matches!(
            completion,
            EmbeddedTxnCompletion::WriteCommitted { .. }
        ));
        let row = db.vertex(VId(9)).unwrap().unwrap();
        assert_eq!(row.labels, vec![PERSON, CREATED]);
        assert_eq!(
            row.props,
            vec![(P, CanonicalScalar::Int(9)), (Q, CanonicalScalar::Int(200))]
        );
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn unconfigured_match_branch_stays_read_only() {
    let ((), report) = run_async_under_lab(0x6e29_4002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(1), vec![PERSON], vec![(P, CanonicalScalar::Int(7))]);
        db.write(&commit, seed).await.unwrap();
        let before = db.frontier().unwrap();
        let template = PreparedGraphVertexUpsertText::prepare(
            "MERGE (n:Person {p:$p}) ON CREATE SET n.q=200,n:Created",
            R,
            symbols,
        )
        .unwrap();
        let bound = template
            .bind_parameters(&GqlParameters::new().with_int64("p", 7).unwrap())
            .unwrap();
        let (stats, outcome, completion) = db
            .execute_graph_vertex_upsert_autocommit_governed(
                &txcx,
                &query,
                &commit,
                &bound,
                policy(),
                |_| -> Result<ElementId, ()> { panic!("existing match must not allocate") },
            )
            .await
            .unwrap();
        assert_eq!(stats.branch, GraphVertexUpsertBranch::Match);
        assert_eq!(stats.action_effects, 0);
        assert_eq!(outcome, GraphVertexMergeOutcome::Matched(VId(1)));
        assert!(matches!(
            completion,
            EmbeddedTxnCompletion::ReadClosed { .. }
        ));
        assert_eq!(db.frontier().unwrap(), before);
        assert_eq!(
            db.vertex(VId(1)).unwrap().unwrap().props,
            vec![(P, CanonicalScalar::Int(7))]
        );
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
