//! Staged write receipts expose identities only after ordinary workspace staging
//! succeeds. They are not commit acknowledgements: abort and failed preparation
//! leave durable state untouched, while successful commit publishes the same IDs.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, WriteBatch};
use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId};
use fgdb_gql::insertion::{GraphInsertPolicy, GraphInsertRequest};
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphMutationPolicy, GraphSymbol, GraphSymbolKind,
    GraphWriteIdentityRequest, GraphWriteProgramPolicy, GraphWriteStatement, GraphWriteStepReceipt,
    PreparedGraphInsertText, PreparedGraphMutationText, PreparedGraphWriteProgram,
};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};
use std::cell::RefCell;

const R: RelationId = RelationId(1);
const SOURCE: LabelId = LabelId(1);
const COPY: LabelId = LabelId(2);
const MARKED: LabelId = LabelId(3);
const P: PropertyKeyId = PropertyKeyId(1);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x41; 32],
        DatabaseSecurityNamespaceId([0x42; 32]),
        [0x43; 32],
    )
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Label, "Source") => Some(GraphSymbol::Label(SOURCE)),
        (GraphSymbolKind::Label, "Copy") => Some(GraphSymbol::Label(COPY)),
        (GraphSymbolKind::Label, "Marked") => Some(GraphSymbol::Label(MARKED)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    }
}
fn query_policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(1_000, 1_000, 1_000_000, 1_000_000)
}
fn insert_policy() -> GraphInsertPolicy {
    GraphInsertPolicy::new(query_policy(), 1_000, 1_000)
}
fn mutation_policy() -> GraphMutationPolicy {
    GraphMutationPolicy::new(query_policy(), 1_000)
}
fn program_policy() -> GraphWriteProgramPolicy {
    GraphWriteProgramPolicy::new(query_policy(), 1_000, 1_000, 1_000)
}
async fn seed(db: &mut Database<MemVfs>, cx: &fgdb_types::CommitCx) {
    let mut batch = WriteBatch::new(R);
    batch.create_vertex(VId(1), vec![SOURCE], vec![(P, CanonicalScalar::Int(10))]);
    batch.create_vertex(VId(2), vec![], vec![(P, CanonicalScalar::Int(20))]);
    batch.add_edge(EId(11), VId(1), VId(2), vec![]);
    batch.add_edge(EId(12), VId(1), VId(2), vec![]);
    db.write(cx, batch).await.unwrap();
}

#[test]
fn create_receipt_maps_occurrences_and_is_only_durable_after_commit() {
    let ((), report) = run_async_under_lab(0x7e71_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        let insertion = PreparedGraphInsertText::prepare(
            "MATCH (a:Source)-[:R]->(b) CREATE (x:Copy {p:a.p}),(a)-[:R]->(x)",
            R,
            symbols,
        )
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
        assert_eq!(
            (insertion.vertices_per_row(), insertion.edges_per_row()),
            (1, 1)
        );
        let mut txn = db.begin(&txcx).unwrap();
        let (stats, vertices, edges) = txn
            .execute_graph_insert_returning_governed(
                &mut db,
                &query,
                &insertion,
                insert_policy(),
                |request| {
                    Ok::<_, ()>(match request {
                        GraphInsertRequest::Vertex { row, vertex } => {
                            ElementId::Vertex(VId(100 + row as u128 * 10 + vertex as u128))
                        }
                        GraphInsertRequest::Edge { row, edge } => {
                            ElementId::Edge(EId(1_000 + row as u128 * 10 + edge as u128))
                        }
                    })
                },
            )
            .unwrap();
        assert_eq!(
            stats.selection.result_rows, 2,
            "parallel source edges retain occurrence multiplicity"
        );
        assert_eq!(vertices, vec![VId(100), VId(110)]);
        assert_eq!(edges, vec![EId(1_000), EId(1_010)]);
        for (vertex, edge) in vertices.iter().zip(&edges) {
            assert!(
                db.vertex(*vertex).unwrap().is_none(),
                "receipt is not a durability claim"
            );
            let staged = txn.vertex(&db, *vertex).unwrap().unwrap();
            assert_eq!(staged.labels, vec![COPY]);
            assert_eq!(staged.props, vec![(P, CanonicalScalar::Int(10))]);
            let staged_edge = txn.edge(&db, *edge).unwrap().unwrap();
            assert_eq!(
                (staged_edge.entry.src, staged_edge.entry.dst),
                (VId(1), *vertex)
            );
        }
        txn.commit(&mut db, &commit).await.unwrap();
        for (vertex, edge) in vertices.iter().zip(&edges) {
            assert!(db.vertex(*vertex).unwrap().is_some());
            assert!(db.edge(*edge).unwrap().is_some());
        }
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn mutation_receipt_is_distinct_sorted_targets_not_match_occurrences_or_fields() {
    let ((), report) = run_async_under_lab(0x7e71_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        let mutation = PreparedGraphMutationText::prepare(
            "MATCH (a:Source)-[:R]->(b) SET b.p=b.p,a.p=a.p,a:Marked",
            R,
            symbols,
        )
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
        let mut txn = db.begin(&txcx).unwrap();
        let (stats, targets) = txn
            .execute_graph_mutation_returning_governed(
                &mut db,
                &query,
                &mutation,
                mutation_policy(),
            )
            .unwrap();
        assert_eq!(stats.selection.result_rows, 2);
        assert_eq!(stats.target_vertices, 2);
        assert_eq!(
            stats.effects, 3,
            "duplicate matches collapse per target/field"
        );
        assert_eq!(targets, vec![VId(1), VId(2)]);
        assert_eq!(db.vertex(VId(1)).unwrap().unwrap().labels, vec![SOURCE]);
        assert_eq!(
            txn.vertex(&db, VId(1)).unwrap().unwrap().labels,
            vec![SOURCE, MARKED]
        );
        txn.commit(&mut db, &commit).await.unwrap();
        assert_eq!(
            db.vertex(VId(1)).unwrap().unwrap().labels,
            vec![SOURCE, MARKED]
        );
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn aborted_success_and_late_failure_never_turn_receipts_into_publication() {
    let ((), report) = run_async_under_lab(0x7e71_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        let standalone = PreparedGraphInsertText::prepare("CREATE (x:Copy {p:7})", R, symbols)
            .unwrap()
            .bind_parameters(&GqlParameters::new())
            .unwrap();
        let mut txn = db.begin(&txcx).unwrap();
        let (_, vertices, edges) = txn
            .execute_graph_insert_returning_governed(
                &mut db,
                &query,
                &standalone,
                insert_policy(),
                |_| Ok::<_, ()>(ElementId::Vertex(VId(500))),
            )
            .unwrap();
        assert_eq!(vertices, vec![VId(500)]);
        assert!(edges.is_empty());
        assert!(txn.vertex(&db, VId(500)).unwrap().is_some());
        txn.abort();
        assert!(db.vertex(VId(500)).unwrap().is_none());

        let insertion = PreparedGraphInsertText::prepare(
            "MATCH (a:Source)-[:R]->(b) CREATE (x:Copy {p:a.p}),(a)-[:R]->(x)",
            R,
            symbols,
        )
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
        let mut txn = db.begin(&txcx).unwrap();
        let mut prefix = WriteBatch::new(R);
        prefix.create_vertex(VId(777), vec![], vec![]);
        txn.write(&mut db, prefix).unwrap();
        let before = txn.staged_effect_digest().unwrap();
        let issued = RefCell::new(Vec::new());
        let failed = txn.execute_graph_insert_returning_governed(
            &mut db,
            &query,
            &insertion,
            insert_policy(),
            |request| {
                let id = match request {
                    GraphInsertRequest::Vertex { row: 0, .. } => ElementId::Vertex(VId(600)),
                    GraphInsertRequest::Edge { row: 0, .. } => ElementId::Edge(EId(1_600)),
                    GraphInsertRequest::Vertex { row: 1, .. } => ElementId::Vertex(VId(1)),
                    GraphInsertRequest::Edge { row: 1, .. } => ElementId::Edge(EId(1_610)),
                    _ => unreachable!(),
                };
                issued.borrow_mut().push(id);
                Ok::<_, ()>(id)
            },
        );
        assert!(
            failed.is_err(),
            "live identity collision must refuse during ordinary storage preparation"
        );
        assert_eq!(
            issued.borrow().len(),
            4,
            "external identities may already have been issued"
        );
        assert_eq!(
            txn.staged_effect_digest().unwrap(),
            before,
            "failed returning write preserves prior workspace"
        );
        assert!(txn.vertex(&db, VId(600)).unwrap().is_none());
        txn.commit(&mut db, &commit).await.unwrap();
        assert!(db.vertex(VId(777)).unwrap().is_some());
        assert!(db.vertex(VId(600)).unwrap().is_none());
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn mixed_program_receipt_escapes_only_after_the_whole_program_is_accepted() {
    let ((), report) = run_async_under_lab(0x7e71_0004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;

        let insertion = PreparedGraphInsertText::prepare("CREATE (x:Copy {p:7})", R, symbols)
            .unwrap()
            .bind_parameters(&GqlParameters::new())
            .unwrap();
        let mutation =
            PreparedGraphMutationText::prepare("MATCH (x:Copy) SET x.p=x.p+1,x:Marked", R, symbols)
                .unwrap()
                .bind_parameters(&GqlParameters::new())
                .unwrap();
        let program = PreparedGraphWriteProgram::prepare(vec![
            GraphWriteStatement::Insert(insertion.clone()),
            GraphWriteStatement::Mutation(mutation),
        ])
        .unwrap();
        let mut txn = db.begin(&txcx).unwrap();
        let receipt = txn
            .execute_graph_write_program_returning_governed(
                &mut db,
                &query,
                &program,
                program_policy(),
                |request| {
                    Ok::<_, ()>(match request {
                        GraphWriteIdentityRequest {
                            statement: 0,
                            request: GraphInsertRequest::Vertex { .. },
                        } => ElementId::Vertex(VId(900)),
                        _ => unreachable!(),
                    })
                },
            )
            .unwrap();
        assert_eq!(receipt.stats().completed_statements, 2);
        assert_eq!(receipt.steps().len(), 2);
        assert!(matches!(
            &receipt.steps()[0],
            GraphWriteStepReceipt::Insert { vertices, edges }
                if vertices == &[VId(900)] && edges.is_empty()
        ));
        assert!(matches!(
            &receipt.steps()[1],
            GraphWriteStepReceipt::Mutation { targets } if targets == &[VId(900)]
        ));
        assert!(!format!("{receipt:?}").contains("900"));
        assert!(db.vertex(VId(900)).unwrap().is_none());
        assert_eq!(
            txn.vertex(&db, VId(900)).unwrap().unwrap().props,
            vec![(P, CanonicalScalar::Int(8))]
        );
        txn.commit(&mut db, &commit).await.unwrap();
        assert_eq!(
            db.vertex(VId(900)).unwrap().unwrap().props,
            vec![(P, CanonicalScalar::Int(8))]
        );

        let bad_mutation =
            PreparedGraphMutationText::prepare("MATCH (x:Copy) SET x.p=x.p/0", R, symbols)
                .unwrap()
                .bind_parameters(&GqlParameters::new())
                .unwrap();
        let failed_program = PreparedGraphWriteProgram::prepare(vec![
            GraphWriteStatement::Insert(insertion),
            GraphWriteStatement::Mutation(bad_mutation),
        ])
        .unwrap();
        let mut txn = db.begin(&txcx).unwrap();
        let issued = RefCell::new(Vec::new());
        let failed = txn.execute_graph_write_program_returning_governed(
            &mut db,
            &query,
            &failed_program,
            program_policy(),
            |request| {
                let id = match request {
                    GraphWriteIdentityRequest {
                        statement: 0,
                        request: GraphInsertRequest::Vertex { .. },
                    } => ElementId::Vertex(VId(901)),
                    _ => unreachable!(),
                };
                issued.borrow_mut().push(id);
                Ok::<_, ()>(id)
            },
        );
        assert!(failed.is_err());
        assert_eq!(&*issued.borrow(), &[ElementId::Vertex(VId(901))]);
        assert!(
            txn.vertex(&db, VId(901)).unwrap().is_none(),
            "late program failure rolls back staged creation"
        );
        txn.abort();
        assert!(db.vertex(VId(901)).unwrap().is_none());
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
