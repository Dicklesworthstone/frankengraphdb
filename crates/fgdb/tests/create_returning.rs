//! CREATE RETURN stages its complete occurrence bag privately before computed
//! output reads, and exposes autocommit rows only after durable completion.

#![recursion_limit = "256"]

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, IdentityPermutation, MemVfs, WriteBatch, WriteTxnError};
use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GraphValue, GraphValueRow};
use fgdb_gql::insertion::{GraphInsertError, GraphInsertPolicy, GraphInsertRequest};
use fgdb_gql::{
    GqlParameterType, GqlParameters, GqlQueryError, GqlQueryPolicy, GraphInsertQueryError,
    GraphSymbol, GraphSymbolKind, PreparedGraphInsertQuery, PreparedGraphInsertQueryText,
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
                vec![(P, CanonicalScalar::Int(value).into())]
            );
            assert_eq!(
                db.vertex(destination).unwrap().unwrap().props,
                vec![(P, CanonicalScalar::Int(value * 2).into())]
            );
            let created = db.edge(edge).unwrap().unwrap();
            assert_eq!(
                (created.entry.src, created.entry.dst),
                (source, destination)
            );
            assert_eq!(
                created.props,
                vec![(P, CanonicalScalar::Int(value + 10).into())]
            );
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
                row.values()[3].as_scalar().unwrap().clone().into()
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

/// A WITH after CREATE (openCypher Create6 "... WITH n WHERE ... RETURN"):
/// its WHERE, its pages and the RETURN read a carried created element's
/// frozen properties, an aggregating WITH sums them, and neither the WHERE
/// nor the WITH's LIMIT limits which rows create.
#[test]
fn a_with_after_create_filters_pages_and_aggregates_the_created_rows() {
    let ((), report) = run_async_under_lab(0xc8e7_0007, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut created = 0;
        for (statement, expected, vertices, edges) in [
            (
                "UNWIND [1,2,3,4,5] AS x CREATE (n:Copy {p:x}) \
                 WITH n WHERE n.p % 2 = 0 RETURN n.p AS p ORDER BY p",
                vec![vec![int(2)], vec![int(4)]],
                5,
                0,
            ),
            (
                "UNWIND [1,2,3,4,5] AS x CREATE (n:Copy {p:x}) WITH sum(n.p) AS s RETURN s",
                vec![vec![int(15)]],
                5,
                0,
            ),
            (
                "UNWIND [1,2,3] AS x CREATE (a:Copy {p:x})-[e:R {p:x*10}]->(b:Copy) \
                 WITH e, a.p AS p WHERE e.p > 10 RETURN e.p AS ep, p ORDER BY ep",
                vec![vec![int(20), int(2)], vec![int(30), int(3)]],
                6,
                3,
            ),
            (
                "UNWIND [1,2,3,4] AS x CREATE (n:Copy {p:x}) \
                 WITH n, n.p AS p ORDER BY p DESC LIMIT 2 RETURN n.p AS q ORDER BY q",
                vec![vec![int(3)], vec![int(4)]],
                4,
                0,
            ),
            (
                "UNWIND [3,1,3] AS x CREATE (n:Copy {p:x}) \
                 RETURN DISTINCT [v IN [n] | v.p][0] AS p ORDER BY p",
                vec![vec![int(1)], vec![int(3)]],
                3,
                0,
            ),
            (
                "CREATE (a:Copy {p:4})-[e:R {p:9}]->(b:Copy {p:5}) \
                 RETURN [v IN [a,b] | v.p][1] AS p, [r IN [e] | r.p][0] AS ep",
                vec![vec![int(5), int(9)]],
                2,
                1,
            ),
            (
                "UNWIND [2,3,4] AS x CREATE (n:Copy {p:x}) \
                 RETURN sum([v IN [n] | v.p][0]) AS total",
                vec![vec![int(9)]],
                3,
                0,
            ),
            (
                "UNWIND [1,2,3] AS x CREATE (n:Copy {p:x}) \
                 WITH collect(n) AS ns RETURN [v IN ns | v.p] AS values",
                vec![vec![GraphValue::List(
                    vec![int(1), int(2), int(3)].into_boxed_slice(),
                )]],
                3,
                0,
            ),
            (
                "UNWIND [1,2,3] AS x CREATE (n:Copy {p:x}) \
                 WITH [n] AS ns WHERE [v IN ns | v.p][0] > 1 \
                 RETURN [v IN ns | v.p][0] AS p ORDER BY p",
                vec![vec![int(2)], vec![int(3)]],
                3,
                0,
            ),
        ] {
            let (stats, rows, completion) = db
                .execute_graph_insert_query_autocommit_engine_governed(
                    &txcx,
                    &cx,
                    &commit,
                    &query(statement),
                    policy(2, 6, 3),
                )
                .await
                .expect(statement);
            assert_eq!(
                (stats.created_vertices, stats.created_edges),
                (vertices, edges),
                "{statement}"
            );
            let expected: Vec<_> = expected
                .into_iter()
                .map(GraphValueRow::from_owned_values)
                .collect();
            assert_eq!(rows.value, expected, "{statement}");
            assert_eq!(rows.rows.result_rows, expected.len() as u64, "{statement}");
            assert_eq!(rows.rows.snapshot_records, 0, "{statement}");
            assert!(
                matches!(completion, EmbeddedTxnCompletion::WriteCommitted { .. }),
                "{statement}"
            );
            created += vertices;
        }
        assert_eq!(db.vertices().unwrap().len() as u64, created);
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
            (
                "UNWIND [2,0] AS x CREATE (n:Copy {p:x}) \
                 RETURN [v IN [n] | 10/v.p][0] AS q LIMIT 0",
                0,
                true,
            ),
            (
                "UNWIND [2,0] AS x CREATE (n:Copy {p:x}) \
                 WITH collect(n) AS ns RETURN [v IN ns | 10/v.p] AS values LIMIT 0",
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
                vec![(P, CanonicalScalar::Int(99).into())]
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
        let definition = query(
            "UNWIND [2,1] AS x CREATE (n:Copy {p:x}) \
             RETURN [v IN [n] | v.p][0] AS p,n ORDER BY p",
        );
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

#[test]
fn computed_output_sees_the_complete_creation_bag_and_prior_overlay() {
    let ((), report) = run_async_under_lab(0xc8e7_0008, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, cx, txcx) = (contexts.commit(), contexts.query(), contexts.txn());
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut transaction = db.begin(&txcx).unwrap();
        let mut prefix = WriteBatch::new(R);
        prefix.create_vertex(VId(99), vec![], vec![(P, CanonicalScalar::Int(12))]);
        transaction.write(&mut db, prefix).unwrap();
        // The first occurrence reads an identity created by the *last*
        // occurrence, as well as an earlier transaction-local creation.
        let definition = PreparedGraphInsertQueryText::prepare_with_parameter_types(
            "UNWIND [7,9] AS x CREATE (n:Copy {p:x}) \
             RETURN [v IN $refs | v.p] AS values",
            R,
            &[("refs", GqlParameterType::List)],
            symbols,
        )
        .unwrap()
        .bind_parameters(
            &GqlParameters::new()
                .with_list(
                    "refs",
                    vec![GraphValue::Vertex(VId(110)), GraphValue::Vertex(VId(99))],
                )
                .unwrap(),
        )
        .unwrap();
        let (stats, output) = transaction
            .execute_graph_insert_query_governed(
                &mut db,
                &cx,
                &definition,
                policy(2, 2, 0),
                allocate,
            )
            .unwrap();
        let expected = GraphValueRow::from_owned_values(vec![GraphValue::List(
            vec![int(9), int(12)].into_boxed_slice(),
        )]);
        assert_eq!(output.value, vec![expected.clone(), expected]);
        assert_eq!(stats.created_vertices, 2);
        assert_eq!(output.rows.snapshot_records, 0);
        assert!(output.evaluator.work_units > stats.evaluator.work_units);
        assert!(db.vertices().unwrap().is_empty());
        transaction.finish(&mut db, &commit).await.unwrap();
        assert_eq!(db.vertices().unwrap().len(), 3);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn computed_output_budgets_continue_creation_and_restore_the_outer_prefix() {
    let ((), report) = run_async_under_lab(0xc8e7_0009, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, cx, txcx) = (contexts.commit(), contexts.query(), contexts.txn());
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let definition = query(
            "UNWIND [3,1,2] AS x CREATE (n:Copy {p:x}) \
             WITH collect(n) AS ns RETURN [v IN ns | v.p] AS values",
        );
        let mut baseline = db.begin(&txcx).unwrap();
        let (creation, expected) = baseline
            .execute_graph_insert_query_governed(
                &mut db,
                &cx,
                &definition,
                policy(1, 3, 0),
                allocate,
            )
            .unwrap();
        assert!(expected.evaluator.work_units > creation.evaluator.work_units);
        assert!(expected.evaluator.scratch_entries > creation.evaluator.scratch_entries);
        baseline.abort();
        for budget in [
            GqlQueryPolicy::new(0, 1, expected.evaluator.work_units - 1, 2_000_000),
            GqlQueryPolicy::new(0, 1, 5_000_000, expected.evaluator.scratch_entries - 1),
        ] {
            let mut transaction = db.begin(&txcx).unwrap();
            let mut prefix = WriteBatch::new(R);
            prefix.create_vertex(VId(99), vec![], vec![(P, CanonicalScalar::Int(12))]);
            transaction.write(&mut db, prefix).unwrap();
            let digest = transaction.staged_effect_digest().unwrap();
            let failure = transaction.execute_graph_insert_query_governed(
                &mut db,
                &cx,
                &definition,
                GraphInsertPolicy::new(budget, 3, 0),
                allocate,
            );
            assert!(
                matches!(failure, Err(GqlQueryError::Evaluator(_))),
                "{failure:?}"
            );
            assert_eq!(transaction.staged_effect_digest().unwrap(), digest);
            assert_eq!(transaction.vertices(&db).unwrap().len(), 1);
            transaction.abort();
        }
        assert!(db.vertices().unwrap().is_empty());
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn computed_output_point_dependencies_survive_success_and_statement_rollback() {
    let ((), report) = run_async_under_lab(0xc8e7_0010, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, cx, txcx) = (contexts.commit(), contexts.query(), contexts.txn());
        for fail_output in [false, true] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let mut seed = WriteBatch::new(R);
            seed.create_vertex(VId(99), vec![], vec![(P, CanonicalScalar::Int(12))]);
            db.write(&commit, seed).await.unwrap();
            let text = if fail_output {
                "CREATE (n:Copy {p:1}) RETURN [v IN $refs | v.p][0] AS p, 1/0 AS failure"
            } else {
                "CREATE (n:Copy {p:1}) RETURN [v IN $refs | v.p][0] AS p"
            };
            let definition = PreparedGraphInsertQueryText::prepare_with_parameter_types(
                text,
                R,
                &[("refs", GqlParameterType::List)],
                symbols,
            )
            .unwrap()
            .bind_parameters(
                &GqlParameters::new()
                    .with_list("refs", vec![GraphValue::Vertex(VId(99))])
                    .unwrap(),
            )
            .unwrap();
            let mut transaction = db.begin(&txcx).unwrap();
            let mut prefix = WriteBatch::new(R);
            prefix.create_vertex(VId(98), vec![], vec![]);
            transaction.write(&mut db, prefix).unwrap();
            let result = transaction.execute_graph_insert_query_governed(
                &mut db,
                &cx,
                &definition,
                policy(1, 1, 0),
                allocate,
            );
            assert_eq!(result.is_err(), fail_output, "{result:?}");
            if let Ok((_, rows)) = result {
                assert_eq!(rows.value[0].values(), &[int(12)]);
            }
            let mut winner = WriteBatch::new(R);
            winner.set_vertex_property(VId(99), P, Some(CanonicalScalar::Int(13)));
            db.write(&commit, winner).await.unwrap();
            assert!(matches!(
                transaction.commit(&mut db, &commit).await,
                Err(WriteTxnError::Write(fgdb::WriteError::FirstCommitterWins {
                    law: "FG-LAW-FCW-READ-01",
                    ..
                }))
            ));
            assert_eq!(db.vertices().unwrap().len(), 1);
            assert!(db.vertex(VId(98)).unwrap().is_none());
            assert!(db.vertex(VId(100)).unwrap().is_none());
        }
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn pure_proposal_does_not_read_created_identities_from_a_precreation_source() {
    let definition = query("CREATE (n:Copy {p:7}) RETURN [v IN [n] | v.p][0] AS p");
    let result = definition.execute_governed(
        policy(1, 1, 0),
        |_, _| -> Result<_, GqlQueryError<(), ()>> { panic!("standalone CREATE has no source") },
        allocate,
        || Ok(()),
    );
    assert!(
        matches!(
            result,
            Err(GqlQueryError::Source(GraphInsertQueryError::Returning(_)))
        ),
        "{result:?}"
    );
}

#[test]
fn computed_private_with_feeds_later_native_create_and_set_steps() {
    let ((), report) = run_async_under_lab(0xc8e7_0011, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, cx, txcx) = (contexts.commit(), contexts.query(), contexts.txn());
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        for (text, created, expected) in [
            (
                "CREATE (a:Copy {p:7}) WITH a, [v IN [a] | v.p][0] AS old \
                 CREATE (b:Copy {p:old+1}) RETURN [v IN [a,b] | v.p] AS values",
                2,
                GraphValue::List(vec![int(7), int(8)].into_boxed_slice()),
            ),
            (
                "CREATE (a:Copy {p:7}) WITH a, [a][0] AS b, [v IN [a] | v.p][0] AS old \
                 SET a.p=b.p+old RETURN a.p AS total",
                1,
                int(14),
            ),
        ] {
            let chain = fgdb_gql::PreparedGraphWriteChainText::prepare(text, R, symbols)
                .expect(text)
                .bind_parameters(&GqlParameters::new())
                .unwrap();
            let mut transaction = db.begin(&txcx).unwrap();
            let (stats, rows, _) = transaction
                .execute_graph_write_chain_engine_governed(&mut db, &cx, &chain, policy(1, 2, 0))
                .expect(text);
            assert_eq!(stats.created_vertices, created);
            assert_eq!(rows.value[0].values(), &[expected]);
            transaction.finish(&mut db, &commit).await.unwrap();
        }
        assert_eq!(db.vertices().unwrap().len(), 3);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
