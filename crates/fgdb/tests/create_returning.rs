//! CREATE RETURN is one ordinary write operation: admit the complete result,
//! stage once, and expose autocommit rows only after durable completion.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, IdentityPermutation, MemVfs, WriteBatch, WriteTxnError};
use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GraphValue, GraphValueRow};
use fgdb_gql::insertion::{GraphInsertError, GraphInsertPolicy, GraphInsertRequest};
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GraphInsertQueryError, GraphSymbol,
    GraphSymbolKind, PreparedGraphInsertQuery, PreparedGraphInsertQueryText,
};
use fgdb_types::context::SimulationCheckpointProbe;
use fgdb_types::{
    CanonicalScalar, DatabaseSecurityNamespaceId, EId, EmbeddedTxnCompletion, PurposeContexts, VId,
};
use std::sync::Arc;

const R: RelationId = RelationId(1);
const COPY: LabelId = LabelId(1);
const P: PropertyKeyId = PropertyKeyId(1);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x71; 32],
        DatabaseSecurityNamespaceId([0x72; 32]),
        [0x73; 32],
    )
}

/// The vertex identity the engine issues for `counter` under [`keys`].
fn engine_vertex(counter: u64) -> VId {
    VId(u128::from(
        IdentityPermutation::vertices(&keys())
            .permute(counter)
            .unwrap(),
    ))
}

/// The edge identity the engine issues for `counter` under [`keys`].
fn engine_edge(counter: u64) -> EId {
    EId(u128::from(
        IdentityPermutation::edges(&keys())
            .permute(counter)
            .unwrap(),
    ))
}

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Label, "Copy") => Some(GraphSymbol::Label(COPY)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    }
}

fn query(text: &str) -> PreparedGraphInsertQuery {
    PreparedGraphInsertQueryText::prepare(text, R, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}

fn policy(rows: u64, vertices: u64, edges: u64) -> GraphInsertPolicy {
    GraphInsertPolicy::new(
        GqlQueryPolicy::new(0, rows, 5_000_000, 2_000_000),
        vertices,
        edges,
    )
}

fn int(value: i64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(value))
}

fn allocate(request: GraphInsertRequest) -> Result<ElementId, ()> {
    Ok(match request {
        GraphInsertRequest::Vertex { row, vertex } => {
            ElementId::Vertex(VId(100 + row as u128 * 10 + vertex as u128))
        }
        GraphInsertRequest::Edge { row, edge } => {
            ElementId::Edge(EId(1_000 + row as u128 * 10 + edge as u128))
        }
    })
}

#[test]
fn returned_occurrence_bindings_commit_once_and_survive_compaction_and_reopen() {
    let ((), report) = run_async_under_lab(0xc8e7_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let before = db.frontier().unwrap();
        let definition = query(
            "UNWIND [3,1,3] AS x
             CREATE (a:Copy {p:x})-[e:R {p:x+10}]->(b:Copy {p:x*2})
             RETURN a AS source,e AS edge,b AS destination,a.p AS p,e.p AS ep,b.p AS bp
             ORDER BY p",
        );
        let (stats, rows, completion) = db
            .execute_graph_insert_query_autocommit_engine_governed(
                &txcx,
                &cx,
                &commit,
                &definition,
                policy(3, 6, 3),
            )
            .await
            .unwrap();
        assert!(
            matches!(completion, EmbeddedTxnCompletion::WriteCommitted { commit_seq }
            if commit_seq.0 == before.0 + 1)
        );
        assert_eq!((stats.created_vertices, stats.created_edges), (6, 3));
        assert_eq!(
            rows.rows.snapshot_records, 0,
            "RETURN must not rescan creations"
        );
        assert_eq!(rows.rows.result_rows, 3);
        // Each UNWIND row takes its source then its destination vertex
        // counter, then one edge counter, in UNWIND order.
        let created = [
            (engine_vertex(1), engine_edge(1), engine_vertex(2), 3),
            (engine_vertex(3), engine_edge(2), engine_vertex(4), 1),
            (engine_vertex(5), engine_edge(3), engine_vertex(6), 3),
        ];
        // ORDER BY p, then the canonical whole-row tie break, whose first
        // column is the source identity.
        let mut ordered = created;
        ordered.sort_by_key(|&(source, _, _, value)| (value, source));
        let expected = ordered
            .into_iter()
            .map(|(source, edge, destination, value)| {
                GraphValueRow::from_owned_values(vec![
                    GraphValue::Vertex(source),
                    GraphValue::Edge(edge),
                    GraphValue::Vertex(destination),
                    int(value),
                    int(value + 10),
                    int(value * 2),
                ])
            })
            .collect::<Vec<_>>();
        assert_eq!(rows.value, expected);
        for (source, edge, destination, value) in created {
            assert_eq!(
                db.vertex(source).unwrap().unwrap().props,
                vec![(P, CanonicalScalar::Int(value))]
            );
            assert_eq!(
                db.vertex(destination).unwrap().unwrap().props,
                vec![(P, CanonicalScalar::Int(value * 2))]
            );
            let created = db.edge(edge).unwrap().unwrap();
            assert_eq!(
                (created.entry.src, created.entry.dst),
                (source, destination)
            );
            assert_eq!(created.props, vec![(P, CanonicalScalar::Int(value + 10))]);
        }
        db.compact(&commit).await.unwrap();
        drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        assert_eq!(db.frontier().unwrap().0, before.0 + 1);
        assert_eq!(db.vertices().unwrap().len(), 6);
        assert_eq!(db.edges().unwrap().len(), 3);
        for row in rows.value {
            let source = row.values()[0].as_vertex().unwrap();
            let edge = row.values()[1].as_edge().unwrap();
            let destination = row.values()[2].as_vertex().unwrap();
            assert_eq!(
                db.vertex(source).unwrap().unwrap().props[0].1,
                row.values()[3].as_scalar().unwrap().clone()
            );
            assert_eq!(db.edge(edge).unwrap().unwrap().entry.dst, destination);
        }
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn distinct_and_zero_result_pages_bound_returned_rows_without_suppressing_writes() {
    let ((), report) = run_async_under_lab(0xc8e7_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let before = db.frontier().unwrap();
        let (stats, rows, completion) = db
            .execute_graph_insert_query_autocommit_engine_governed(
                &txcx,
                &cx,
                &commit,
                &query("UNWIND [7,7,7] AS x CREATE (n:Copy {p:x}) RETURN DISTINCT n.p AS p"),
                policy(1, 3, 0),
            )
            .await
            .unwrap();
        assert_eq!(stats.created_vertices, 3);
        assert_eq!(
            rows.value,
            vec![GraphValueRow::from_owned_values(vec![int(7)])]
        );
        assert_eq!(rows.rows.result_rows, 1);
        assert!(
            matches!(completion, EmbeddedTxnCompletion::WriteCommitted { commit_seq }
            if commit_seq.0 == before.0 + 1)
        );

        let (stats, rows, completion) = db
            .execute_graph_insert_query_autocommit_engine_governed(
                &txcx,
                &cx,
                &commit,
                &query("UNWIND [4,5] AS x CREATE (n:Copy {p:x}) RETURN n LIMIT 0"),
                policy(0, 2, 0),
            )
            .await
            .unwrap();
        assert_eq!(stats.created_vertices, 2);
        assert!(rows.value.is_empty());
        assert_eq!(rows.rows.result_rows, 0);
        assert!(
            matches!(completion, EmbeddedTxnCompletion::WriteCommitted { commit_seq }
            if commit_seq.0 == before.0 + 2)
        );
        assert_eq!(db.vertices().unwrap().len(), 5);

        let (stats, rows, completion) = db
            .execute_graph_insert_query_autocommit_engine_governed(
                &txcx,
                &cx,
                &commit,
                &query("UNWIND [] AS x CREATE (n:Copy {p:x}) RETURN n"),
                policy(0, 0, 0),
            )
            .await
            .unwrap();
        assert_eq!(stats.created_vertices, 0);
        assert!(rows.value.is_empty());
        assert!(matches!(
            completion,
            EmbeddedTxnCompletion::ReadClosed { .. }
        ));
        assert_eq!(db.frontier().unwrap().0, before.0 + 2);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn return_expression_and_final_row_budget_failures_preserve_the_outer_prefix() {
    let ((), report) = run_async_under_lab(0xc8e7_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let before = db.frontier().unwrap();
        let mut transaction = db.begin(&txcx).unwrap();
        let mut prefix = WriteBatch::new(R);
        prefix.create_vertex(VId(99), vec![], vec![(P, CanonicalScalar::Int(99))]);
        transaction.write(&mut db, prefix).unwrap();
        let digest = transaction.staged_effect_digest().unwrap();
        for (text, rows, arithmetic) in [
            (
                "UNWIND [2,0] AS x CREATE (n:Copy {p:x}) RETURN 10/n.p AS q",
                2,
                true,
            ),
            (
                "UNWIND [2,0] AS x CREATE (n:Copy {p:x}) RETURN 10/n.p AS q LIMIT 0",
                0,
                true,
            ),
            ("UNWIND [2,1] AS x CREATE (n:Copy {p:x}) RETURN n", 1, false),
        ] {
            let result = transaction.execute_graph_insert_query_governed(
                &mut db,
                &cx,
                &query(text),
                policy(rows, 2, 0),
                allocate,
            );
            if arithmetic {
                assert!(
                    matches!(
                        result,
                        Err(GqlQueryError::Source(GraphInsertQueryError::Returning(_)))
                    ),
                    "{result:?}"
                );
            } else {
                assert!(matches!(result, Err(GqlQueryError::Rows(_))), "{result:?}");
            }
            assert_eq!(transaction.staged_effect_digest().unwrap(), digest);
            assert!(transaction.vertex(&db, VId(100)).unwrap().is_none());
            assert!(transaction.vertex(&db, VId(110)).unwrap().is_none());
            assert_eq!(
                transaction.vertex(&db, VId(99)).unwrap().unwrap().props,
                vec![(P, CanonicalScalar::Int(99))]
            );
            assert_eq!(db.frontier().unwrap(), before);
            assert!(db.vertices().unwrap().is_empty());
        }
        transaction.finish(&mut db, &commit).await.unwrap();
        assert_eq!(db.frontier().unwrap().0, before.0 + 1);
        assert_eq!(db.vertices().unwrap().len(), 1);
        assert!(db.vertex(VId(99)).unwrap().is_some());
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn autocommit_refusals_publish_no_writes_or_rows_and_release_the_snapshot_obligation() {
    let ((), report) = run_async_under_lab(0xc8e7_0004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let before = db.frontier().unwrap();
        let failing = query("UNWIND [2,0] AS x CREATE (n:Copy {p:x}) RETURN 10/n.p AS q");
        let result = db
            .execute_graph_insert_query_autocommit_engine_governed(
                &txcx,
                &cx,
                &commit,
                &failing,
                policy(2, 2, 0),
            )
            .await;
        assert!(
            matches!(
                result,
                Err(GqlQueryError::Source(GraphInsertQueryError::Returning(_)))
            ),
            "{result:?}"
        );
        assert_eq!(db.frontier().unwrap(), before);
        assert!(db.vertices().unwrap().is_empty());
        assert_eq!(txcx.outstanding_obligations(), 0);

        let simple = query("CREATE (n:Copy {p:9}) RETURN n,n.p");
        let result = db
            .execute_graph_insert_query_autocommit_governed(
                &txcx,
                &cx,
                &commit,
                &simple,
                policy(1, 1, 0),
                |_| Err::<ElementId, _>("allocator unavailable"),
            )
            .await;
        assert!(matches!(
            result,
            Err(GqlQueryError::Source(GraphInsertQueryError::Insertion(
                GraphInsertError::IdentitySource("allocator unavailable")
            )))
        ));
        assert_eq!(db.frontier().unwrap(), before);
        assert!(db.vertices().unwrap().is_empty());
        assert_eq!(txcx.outstanding_obligations(), 0);

        let cancelled = cx.with_checkpoint_probe(Arc::new(SimulationCheckpointProbe::new(Some(1))));
        let result = db
            .execute_graph_insert_query_autocommit_governed(
                &txcx,
                &cancelled,
                &commit,
                &simple,
                policy(1, 1, 0),
                |_| -> Result<ElementId, ()> { panic!("cancelled query cannot allocate") },
            )
            .await;
        assert!(
            matches!(result, Err(GqlQueryError::Interrupted(_))),
            "{result:?}"
        );
        assert_eq!(db.frontier().unwrap(), before);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn ownership_and_advanced_snapshot_refusals_precede_identity_allocation() {
    let ((), report) = run_async_under_lab(0xc8e7_0005, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut owner = Database::open_memory(&commit, keys()).await.unwrap();
        let mut other = Database::open_memory(&commit, keys()).await.unwrap();
        let mut transaction = owner.begin(&txcx).unwrap();
        let definition = query("CREATE (n:Copy {p:1}) RETURN n");
        let result = transaction.execute_graph_insert_query_governed(
            &mut other,
            &cx,
            &definition,
            policy(1, 1, 0),
            |_| -> Result<ElementId, ()> { panic!("wrong owner cannot allocate") },
        );
        assert!(matches!(
            result,
            Err(GqlQueryError::Source(GraphInsertQueryError::Insertion(
                GraphInsertError::Source(WriteTxnError::WrongDatabase)
            )))
        ));
        let mut winner = WriteBatch::new(R);
        winner.create_vertex(VId(10), vec![], vec![]);
        owner.write(&commit, winner).await.unwrap();
        let result = transaction.execute_graph_insert_query_governed(
            &mut owner,
            &cx,
            &definition,
            policy(1, 1, 0),
            |_| -> Result<ElementId, ()> { panic!("stale snapshot cannot allocate") },
        );
        assert!(matches!(
            result,
            Err(GqlQueryError::Source(GraphInsertQueryError::Insertion(
                GraphInsertError::Source(WriteTxnError::SnapshotAdvanced { .. })
            )))
        ));
        transaction.abort();
        assert!(other.vertices().unwrap().is_empty());
        assert_eq!(owner.vertices().unwrap().len(), 1);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn every_query_checkpoint_before_acceptance_preserves_existing_staged_work() {
    let ((), report) = run_async_under_lab(0xc8e7_0006, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let before = db.frontier().unwrap();
        let definition =
            query("UNWIND [2,1] AS x CREATE (n:Copy {p:x}) RETURN n.p AS p,n ORDER BY p");
        let prefix = || {
            let mut batch = WriteBatch::new(R);
            batch.create_vertex(VId(99), vec![], vec![(P, CanonicalScalar::Int(99))]);
            batch
        };
        let mut baseline = db.begin(&txcx).unwrap();
        baseline.write(&mut db, prefix()).unwrap();
        let probe = Arc::new(SimulationCheckpointProbe::new(None));
        let (_, expected) = baseline
            .execute_graph_insert_query_governed(
                &mut db,
                &cx.with_checkpoint_probe(probe.clone()),
                &definition,
                policy(2, 2, 0),
                allocate,
            )
            .unwrap();
        let calls = probe.calls();
        assert!(
            calls > 10,
            "the complete query must cross observable checkpoints"
        );
        baseline.abort();
        for cut in 1..=calls {
            let mut transaction = db.begin(&txcx).unwrap();
            transaction.write(&mut db, prefix()).unwrap();
            let digest = transaction.staged_effect_digest().unwrap();
            let probe = Arc::new(SimulationCheckpointProbe::new(Some(cut)));
            let result = transaction.execute_graph_insert_query_governed(
                &mut db,
                &cx.with_checkpoint_probe(probe.clone()),
                &definition,
                policy(2, 2, 0),
                allocate,
            );
            assert!(
                matches!(result, Err(GqlQueryError::Interrupted(_))),
                "checkpoint {cut}: {result:?}"
            );
            assert_eq!(probe.calls(), cut);
            assert_eq!(transaction.staged_effect_digest().unwrap(), digest);
            assert_eq!(transaction.vertices(&db).unwrap().len(), 1);
            assert!(transaction.vertex(&db, VId(99)).unwrap().is_some());
            assert_eq!(db.frontier().unwrap(), before);
            transaction.abort();
            assert_eq!(txcx.outstanding_obligations(), 0);
        }
        let mut transaction = db.begin(&txcx).unwrap();
        transaction.write(&mut db, prefix()).unwrap();
        let (_, actual) = transaction
            .execute_graph_insert_query_governed(
                &mut db,
                &cx,
                &definition,
                policy(2, 2, 0),
                allocate,
            )
            .unwrap();
        assert_eq!(actual.value, expected.value);
        transaction.finish(&mut db, &commit).await.unwrap();
        assert_eq!(db.vertices().unwrap().len(), 3);
        assert_eq!(db.frontier().unwrap().0, before.0 + 1);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
