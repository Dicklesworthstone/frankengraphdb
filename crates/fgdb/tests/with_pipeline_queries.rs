//! Relational pipelines use the real Chronicle/Strata and transaction read path.

use asupersync::lab::run_async_under_lab;
use fgdb::{
    Database, DatabaseKeys, GqlError, MemVfs, ReadError, WriteBatch, WriteError, WriteTxnError,
};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::algebra::GraphValueRow;
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GraphSetExecutionError, GraphSymbol,
    GraphSymbolKind, PreparedGraphSet, PreparedGraphSetText,
};
use fgdb_types::{
    CanonicalScalar, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId,
};

const P: PropertyKeyId = PropertyKeyId(1);
const R: RelationId = RelationId(1);
fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x41; 32],
        DatabaseSecurityNamespaceId([0x42; 32]),
        [0x43; 32],
    )
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        _ => None,
    }
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 10_000, 20_000_000, 20_000_000)
}
fn query(text: &str) -> PreparedGraphSet {
    PreparedGraphSetText::prepare(text, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}
fn ranked() -> PreparedGraphSet {
    query(
        "MATCH (n) WITH n AS owner,n.p+1 AS score ORDER BY score DESC LIMIT 2 WHERE score>2 RETURN owner,score*10 AS rank ORDER BY owner",
    )
}
fn pairs(rows: &[GraphValueRow]) -> Vec<(VId, i64)> {
    rows.iter()
        .map(|row| {
            let Some(CanonicalScalar::Int(score)) = row.get(1).unwrap().as_scalar() else {
                panic!("integer rank")
            };
            (row.get(0).unwrap().as_vertex().unwrap(), *score)
        })
        .collect()
}
async fn seed(db: &mut Database<MemVfs>, cx: &fgdb_types::CommitCx) -> CommitSeq {
    let mut batch = WriteBatch::new(R);
    for (id, value) in [(1, 1), (2, 2), (3, 3), (4, 0)] {
        batch.create_vertex(VId(id), vec![], vec![(P, CanonicalScalar::Int(value))]);
    }
    db.write(cx, batch).await.unwrap()
}
fn change(id: u128, value: i64) -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    batch.set_vertex_property(VId(id), P, Some(CanonicalScalar::Int(value)));
    batch
}

#[test]
fn ranked_pipeline_pins_history_and_survives_compaction_and_reopen() {
    let ((), report) = run_async_under_lab(0x71f0_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let old = seed(&mut db, &commit).await;
        let pinned = db.read_session().unwrap();
        let query = ranked();
        let original = vec![(VId(2), 30), (VId(3), 40)];
        assert_eq!(
            pairs(
                &db.execute_graph_set_governed(&cx, &query, policy())
                    .unwrap()
                    .value
            ),
            original
        );
        let latest = db.write(&commit, change(1, 9)).await.unwrap();
        let current = vec![(VId(1), 100), (VId(3), 40)];
        assert_eq!(
            pairs(
                &db.execute_graph_set_governed(&cx, &query, policy())
                    .unwrap()
                    .value
            ),
            current
        );
        assert_eq!(
            pairs(
                &pinned
                    .execute_graph_set_governed(&cx, &query, policy())
                    .unwrap()
                    .value
            ),
            original
        );
        assert_eq!(
            pairs(
                &db.execute_graph_set_governed_at(&cx, &query, old, policy())
                    .unwrap()
                    .value
            ),
            original
        );
        assert!(matches!(
            pinned.execute_graph_set_governed_at(
                &cx,
                &query,
                latest,
                GqlQueryPolicy::new(0, 0, 0, 0)
            ),
            Err(GqlQueryError::Source(GraphSetExecutionError::Source(
                GqlError::Read(ReadError::BeyondFrontier { .. })
            )))
        ));
        db.compact(&commit).await.unwrap();
        drop(pinned);
        drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        assert_eq!(
            pairs(
                &db.execute_graph_set_governed(&cx, &query, policy())
                    .unwrap()
                    .value
            ),
            current
        );
        assert_eq!(
            pairs(
                &db.execute_graph_set_governed_at(&cx, &query, old, policy())
                    .unwrap()
                    .value
            ),
            original
        );
        assert_eq!(db.frontier().unwrap(), latest);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn pipeline_reads_canonical_staged_values_without_publishing_or_changing_effects() {
    let ((), report) = run_async_under_lab(0x71f0_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let frontier = seed(&mut db, &commit).await;
        let query = ranked();
        let mut txn = db.begin(&txcx).unwrap();
        txn.write(&mut db, change(1, 9)).unwrap();
        let digest = txn.staged_effect_digest().unwrap();
        let first = txn
            .execute_graph_set_governed(&db, &cx, &query, policy())
            .unwrap();
        assert_eq!(pairs(&first.value), vec![(VId(1), 100), (VId(3), 40)]);
        assert_eq!(
            txn.execute_graph_set_governed(&db, &cx, &query, policy())
                .unwrap(),
            first
        );
        assert_eq!(txn.staged_effect_digest().unwrap(), digest);
        assert_eq!(
            pairs(
                &db.execute_graph_set_governed(&cx, &query, policy())
                    .unwrap()
                    .value
            ),
            vec![(VId(2), 30), (VId(3), 40)]
        );
        txn.abort();
        assert_eq!(db.frontier().unwrap(), frontier);
        let mut reader = db.begin(&txcx).unwrap();
        reader
            .execute_graph_set_governed(&db, &cx, &query, policy())
            .unwrap();
        reader.finish(&mut db, &commit).await.unwrap();
        assert_eq!(
            db.frontier().unwrap(),
            frontier,
            "read-only completion must not publish a marker"
        );
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn filtered_empty_and_limited_rows_keep_transaction_conflict_witnesses() {
    let ((), report) = run_async_under_lab(0x71f0_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        for text in [
            "MATCH (n) WITH n,n.p AS p WHERE p>100 RETURN n",
            "MATCH (n) WITH n,n.p AS p ORDER BY p DESC LIMIT 1 RETURN n",
            "MATCH (n) WITH n,n.p AS p RETURN n LIMIT 0",
        ] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            seed(&mut db, &commit).await;
            let mut txn = db.begin(&txcx).unwrap();
            txn.execute_graph_set_governed(&db, &cx, &query(text), policy())
                .unwrap();
            db.write(&commit, change(4, 200)).await.unwrap();
            assert!(matches!(
                txn.finish(&mut db, &commit).await,
                Err(WriteTxnError::Write(WriteError::FirstCommitterWins { .. }))
            ));
            assert_eq!(txcx.outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn relational_adapters_share_all_caps_and_validate_lifecycle_before_zero_budget() {
    let ((), report) = run_async_under_lab(0x71f0_0004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let frontier = seed(&mut db, &commit).await;
        let query = ranked();
        let measured = db
            .execute_graph_set_governed(&cx, &query, policy())
            .unwrap();
        let caps = [
            measured.rows.snapshot_records,
            measured.rows.result_rows,
            measured.evaluator.work_units,
            measured.evaluator.scratch_entries,
        ];
        assert_eq!(
            db.execute_graph_set_governed(
                &cx,
                &query,
                GqlQueryPolicy::new(caps[0], caps[1], caps[2], caps[3])
            )
            .unwrap(),
            measured
        );
        for dimension in 0..4 {
            let mut limit = caps;
            limit[dimension] -= 1;
            assert!(
                db.execute_graph_set_governed(
                    &cx,
                    &query,
                    GqlQueryPolicy::new(limit[0], limit[1], limit[2], limit[3])
                )
                .is_err()
            );
            assert_eq!(db.frontier().unwrap(), frontier);
        }
        let zero = GqlQueryPolicy::new(0, 0, 0, 0);
        assert!(matches!(
            db.execute_graph_set_governed_at(&cx, &query, CommitSeq(frontier.0 + 1), zero),
            Err(GqlQueryError::Source(GraphSetExecutionError::Source(
                GqlError::Read(ReadError::BeyondFrontier { .. })
            )))
        ));
        let other = Database::open_memory(&commit, keys()).await.unwrap();
        let txn = db.begin(&txcx).unwrap();
        assert!(matches!(
            txn.execute_graph_set_governed(&other, &cx, &query, zero),
            Err(GqlQueryError::Source(GraphSetExecutionError::Source(
                WriteTxnError::WrongDatabase
            )))
        ));
        txn.abort();
        let mut finished = db.begin(&txcx).unwrap();
        finished.finish(&mut db, &commit).await.unwrap();
        assert!(matches!(
            finished.execute_graph_set_governed(&db, &cx, &query, zero),
            Err(GqlQueryError::Source(GraphSetExecutionError::Source(
                WriteTxnError::Finished
            )))
        ));
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn late_arithmetic_failure_preserves_outer_effects_and_observed_reads() {
    let ((), report) = run_async_under_lab(0x71f0_0005, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let frontier = seed(&mut db, &commit).await;
        let mut txn = db.begin(&txcx).unwrap();
        let mut prefix = WriteBatch::new(R);
        prefix.create_vertex(VId(99), vec![], vec![]);
        txn.write(&mut db, prefix).unwrap();
        let digest = txn.staged_effect_digest().unwrap();
        let query = query("MATCH (n) WITH n,n.p AS value RETURN n,10/value AS q LIMIT 0");
        assert!(matches!(
            txn.execute_graph_set_governed(&db, &cx, &query, policy()),
            Err(GqlQueryError::Source(
                GraphSetExecutionError::Projection { .. }
            ))
        ));
        assert_eq!(txn.staged_effect_digest().unwrap(), digest);
        assert_eq!(db.frontier().unwrap(), frontier);
        assert!(db.vertex(VId(99)).unwrap().is_none());
        db.write(&commit, change(4, 1)).await.unwrap();
        assert!(matches!(
            txn.finish(&mut db, &commit).await,
            Err(WriteTxnError::Write(WriteError::FirstCommitterWins { .. }))
        ));
        assert!(db.vertex(VId(99)).unwrap().is_none());
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn parallel_edges_keep_bag_multiplicity_through_with_and_union() {
    let ((), report) = run_async_under_lab(0x71f0_0006, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        let mut edges = WriteBatch::new(R);
        for (id, dst) in [(10, 2), (11, 2), (12, 3)] {
            edges.add_edge(EId(id), VId(1), VId(dst), vec![]);
        }
        let frontier = db.write(&commit, edges).await.unwrap();
        for (text, expected) in [
            (
                "MATCH (a)-[:R]->(b) WITH b.p AS score RETURN score",
                vec![2, 2, 3],
            ),
            (
                "MATCH (a)-[:R]->(b) WITH DISTINCT b.p AS score RETURN score",
                vec![2, 3],
            ),
            (
                "MATCH (a)-[:R]->(b) WITH b.p AS score WHERE score=2 RETURN score UNION ALL MATCH (n) WITH n.p AS score WHERE score=3 RETURN score",
                vec![2, 2, 3],
            ),
        ] {
            let rows = db
                .execute_graph_set_governed(&cx, &query(text), policy())
                .unwrap()
                .value;
            let actual = rows
                .iter()
                .map(|row| match row.get(0).unwrap().as_scalar() {
                    Some(CanonicalScalar::Int(value)) => *value,
                    _ => panic!("integer score"),
                })
                .collect::<Vec<_>>();
            assert_eq!(actual, expected);
        }
        assert_eq!(db.frontier().unwrap(), frontier);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

/// fgdb-694ga item 1: a paged query folds its order-preserving projection
/// row by row, and that projection reads `x.p` of an element value through
/// the same source as the unpaged query. LIMIT and SKIP select exactly the
/// unpaged rows' slice, element reads included.
#[test]
fn a_folded_page_reads_element_properties_as_the_unpaged_query_does() {
    let ((), report) = run_async_under_lab(0x71f0_0009, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        let rows = |text: &str| -> Vec<GraphValueRow> {
            db.execute_graph_set_governed(&cx, &query(text), policy())
                .unwrap()
                .value
        };
        let unpaged = rows("MATCH (n) WITH collect(n) AS ns UNWIND ns AS x RETURN x.p AS v");
        assert_eq!(unpaged.len(), 4);
        assert_eq!(
            rows("MATCH (n) WITH collect(n) AS ns UNWIND ns AS x RETURN x.p AS v LIMIT 2"),
            unpaged[..2]
        );
        assert_eq!(
            rows("MATCH (n) WITH collect(n) AS ns UNWIND ns AS x RETURN x.p AS v SKIP 1 LIMIT 2"),
            unpaged[1..3]
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

/// fgdb-xnd7x: `x.p` over a path's nodes or relationships reads the stored
/// property at the query's own cut: the frontier, a pinned read view and an
/// older sequence each see their own value. A key the catalog lacks is NULL.
#[test]
fn path_elements_read_their_properties_at_the_query_cut() {
    use fgdb_gql::algebra::GraphValue;
    let ((), report) = run_async_under_lab(0x71f0_0007, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        let mut edges = WriteBatch::new(R);
        edges.add_edge(EId(10), VId(1), VId(2), vec![(P, CanonicalScalar::Int(7))]);
        edges.add_edge(EId(11), VId(2), VId(3), vec![(P, CanonicalScalar::Int(8))]);
        let old = db.write(&commit, edges).await.unwrap();
        let pinned = db.read_session().unwrap();
        db.write(&commit, change(2, 20)).await.unwrap();
        let ints = |value: &GraphValue| -> Vec<Option<i64>> {
            let GraphValue::List(items) = value else {
                panic!("a list")
            };
            items
                .iter()
                .map(|item| match item.as_scalar() {
                    Some(CanonicalScalar::Int(value)) => Some(*value),
                    Some(CanonicalScalar::Null) => None,
                    _ => panic!("an integer or NULL"),
                })
                .collect()
        };
        let paths = query(
            "MATCH p = (a)-[:R]->(b)-[:R]->(c) \
             RETURN [x IN nodes(p) | x.p] AS v, [e IN relationships(p) | e.p] AS w, \
             [x IN nodes(p) | x.q] AS missing",
        );
        let answer = |rows: Vec<GraphValueRow>| {
            assert_eq!(rows.len(), 1);
            let row = &rows[0];
            (
                ints(row.get(0).unwrap()),
                ints(row.get(1).unwrap()),
                ints(row.get(2).unwrap()),
            )
        };
        let edges = vec![Some(7), Some(8)];
        let missing = vec![None, None, None];
        assert_eq!(
            answer(
                db.execute_graph_set_governed(&cx, &paths, policy())
                    .unwrap()
                    .value
            ),
            (
                vec![Some(1), Some(20), Some(3)],
                edges.clone(),
                missing.clone()
            )
        );
        let before = (vec![Some(1), Some(2), Some(3)], edges, missing);
        assert_eq!(
            answer(
                pinned
                    .execute_graph_set_governed(&cx, &paths, policy())
                    .unwrap()
                    .value
            ),
            before
        );
        assert_eq!(
            answer(
                db.execute_graph_set_governed_at(&cx, &paths, old, policy())
                    .unwrap()
                    .value
            ),
            before
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

/// An UNWIND list that reads carried elements' properties evaluates with the
/// source's element reader, at the query's own cut, like a RETURN projection.
/// ORDER BY keeps the resident executor; an unordered LIMIT folds the UNWIND
/// into its page. Both refused with IncompatibleOperands before.
#[test]
fn unwound_comprehensions_read_carried_element_properties() {
    let ((), report) = run_async_under_lab(0x71f0_0008, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        let mut edges = WriteBatch::new(R);
        edges.add_edge(EId(10), VId(1), VId(2), vec![(P, CanonicalScalar::Int(7))]);
        edges.add_edge(EId(11), VId(2), VId(3), vec![(P, CanonicalScalar::Int(8))]);
        let old = db.write(&commit, edges).await.unwrap();
        let pinned = db.read_session().unwrap();
        db.write(&commit, change(2, 20)).await.unwrap();
        let ints = |rows: Vec<GraphValueRow>| -> Vec<i64> {
            rows.iter()
                .map(|row| {
                    assert_eq!(row.len(), 1);
                    let Some(CanonicalScalar::Int(value)) = row.get(0).unwrap().as_scalar() else {
                        panic!("an integer")
                    };
                    *value
                })
                .collect()
        };
        let carried = query(
            "MATCH (n) WITH [n, n] AS items UNWIND [x IN items | x.p] AS v RETURN v ORDER BY v",
        );
        let path = query(
            "MATCH p = (a)-[:R]->(b)-[:R]->(c) \
             UNWIND [x IN nodes(p) | x.p] + [e IN relationships(p) | e.p] AS v RETURN v LIMIT 10",
        );
        for (prepared, now, before) in [
            (
                &carried,
                vec![0, 0, 1, 1, 3, 3, 20, 20],
                vec![0, 0, 1, 1, 2, 2, 3, 3],
            ),
            (&path, vec![1, 20, 3, 7, 8], vec![1, 2, 3, 7, 8]),
        ] {
            let live = db
                .execute_graph_set_governed(&cx, prepared, policy())
                .unwrap();
            assert_eq!(ints(live.value), now);
            let session = pinned
                .execute_graph_set_governed(&cx, prepared, policy())
                .unwrap();
            assert_eq!(ints(session.value), before);
            let history = db
                .execute_graph_set_governed_at(&cx, prepared, old, policy())
                .unwrap();
            assert_eq!(ints(history.value), before);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

mod transaction_element_properties {
    use super::*;
    use fgdb::{PreparedNativeRead, QueryError, QueryResult, QueryValue, WriteTxn};
    use fgdb_delta_types::ElementId;
    use fgdb_gql::algebra::GraphValue;
    use fgdb_types::context::SimulationCheckpointProbe;
    use std::collections::BTreeMap;
    use std::sync::Arc;

    const ROWS: &str =
        "UNWIND $elements AS element WITH element AS alias RETURN alias, alias.p AS value";
    const FOLDED: &str = "UNWIND $elements AS element WITH element.p AS value \
        RETURN COUNT(value) AS n, SUM(value) AS total";
    const MATERIALIZED: &str = "UNWIND $elements AS element \
        WITH element.p AS value ORDER BY value RETURN COUNT(value) AS n, SUM(value) AS total";
    const OUTPUT: &str = "UNWIND $elements AS element \
        RETURN [item IN COLLECT(element) | item.p] AS values";
    const VERTEX_OUTPUT: &str = "MATCH (n) RETURN [item IN COLLECT(n) | item.p] AS values";
    const EDGE_OUTPUT: &str = "MATCH (a)-[e:R]->(b) RETURN [item IN COLLECT(e) | item.p] AS values";

    fn scalar_list(values: &[Option<i64>]) -> QueryValue {
        QueryValue::Value(GraphValue::List(
            values
                .iter()
                .map(|value| {
                    GraphValue::Scalar(value.map_or(CanonicalScalar::Null, CanonicalScalar::Int))
                })
                .collect::<Vec<_>>()
                .into(),
        ))
    }

    fn parameters(elements: &[ElementId]) -> GqlParameters {
        GqlParameters::new()
            .with_list(
                "elements",
                elements
                    .iter()
                    .map(|element| match element {
                        ElementId::Vertex(id) => GraphValue::Vertex(*id),
                        ElementId::Edge(id) => GraphValue::Edge(*id),
                    })
                    .collect(),
            )
            .unwrap()
    }

    fn rows(result: QueryResult) -> Vec<Vec<QueryValue>> {
        match result {
            QueryResult::Rows { rows, .. } => rows,
            QueryResult::Write { .. } => panic!("a property read published a write"),
        }
    }

    fn properties(result: QueryResult) -> BTreeMap<ElementId, CanonicalScalar> {
        rows(result)
            .into_iter()
            .map(|row| {
                let element = match row[0].as_value().unwrap() {
                    GraphValue::Vertex(id) => ElementId::Vertex(*id),
                    GraphValue::Edge(id) => ElementId::Edge(*id),
                    _ => panic!("a carried element identity"),
                };
                (
                    element,
                    row[1].as_value().unwrap().as_scalar().unwrap().clone(),
                )
            })
            .collect()
    }

    fn read(
        txn: &WriteTxn,
        db: &Database<MemVfs>,
        cx: &fgdb_types::QueryCx,
        text: &str,
        params: &GqlParameters,
    ) -> QueryResult {
        txn.query(db, cx, text, params, symbols, policy())
            .unwrap_or_else(|error| panic!("{text}: {error:?}"))
    }

    async fn seed_edges(db: &mut Database<MemVfs>, cx: &fgdb_types::CommitCx) {
        seed(db, cx).await;
        let mut edges = WriteBatch::new(R);
        edges.add_edge(EId(10), VId(1), VId(2), vec![(P, CanonicalScalar::Int(7))]);
        edges.add_edge(EId(11), VId(2), VId(3), vec![(P, CanonicalScalar::Int(8))]);
        db.write(cx, edges).await.unwrap();
    }

    #[test]
    fn aggregate_output_reads_collected_and_supplied_elements_at_the_selected_cut() {
        let ((), report) = run_async_under_lab(0x71f0_0105, |root| async move {
            let c = PurposeContexts::narrow_runtime_root(&root);
            let cx = c.query();
            let mut db = Database::open_memory(&c.commit(), keys()).await.unwrap();
            seed_edges(&mut db, &c.commit()).await;
            let basis = db.frontier().unwrap();
            let pinned = db.read_session().unwrap();
            let params = parameters(&[
                ElementId::Vertex(VId(4)),
                ElementId::Vertex(VId(9)),
                ElementId::Edge(EId(10)),
                ElementId::Edge(EId(11)),
                ElementId::Edge(EId(90)),
                ElementId::Edge(EId(999)),
            ]);
            let statements = [
                VERTEX_OUTPUT,
                EDGE_OUTPUT,
                // Neither the zero-valued vertex nor any edge is an input
                // binding here. Their properties still come from this cut.
                "MATCH (n) WHERE n.p > 0 RETURN COUNT(*) AS n, \
                    [item IN $elements | item.p] AS values",
                OUTPUT,
                "MATCH (n) RETURN n.p AS key, \
                    [item IN COLLECT(n) | item.p] AS values ORDER BY key DESC LIMIT 2",
                "UNWIND $elements AS element RETURN DISTINCT \
                    [item IN COLLECT(element) | item.p] AS values ORDER BY values LIMIT 0",
            ];
            let old = vec![
                vec![vec![scalar_list(&[Some(1), Some(2), Some(3), Some(0)])]],
                vec![vec![scalar_list(&[Some(7), Some(8)])]],
                vec![vec![
                    QueryValue::Count(3),
                    scalar_list(&[Some(0), None, Some(7), Some(8), None, None]),
                ]],
                vec![vec![scalar_list(&[
                    Some(0),
                    None,
                    Some(7),
                    Some(8),
                    None,
                    None,
                ])]],
                vec![
                    vec![
                        QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Int(3))),
                        scalar_list(&[Some(3)]),
                    ],
                    vec![
                        QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Int(2))),
                        scalar_list(&[Some(2)]),
                    ],
                ],
                vec![],
            ];
            // A statement declares only the parameters it reads: an unused
            // declaration is refused at preparation.
            let none = GqlParameters::new();
            let check = |db: &Database<MemVfs>, expected: &[Vec<Vec<QueryValue>>]| {
                for (index, text) in statements.iter().enumerate() {
                    let params = if text.contains("$elements") {
                        &params
                    } else {
                        &none
                    };
                    let prepared = PreparedNativeRead::prepare(text, params, symbols).unwrap();
                    assert_eq!(
                        rows(prepared.execute(db, &cx, params, policy()).unwrap()),
                        expected[index],
                        "current {text}"
                    );
                    assert_eq!(
                        rows(pinned.query(&cx, text, params, symbols, policy()).unwrap()),
                        old[index],
                        "saved {text}"
                    );
                    let native = match prepared {
                        PreparedNativeRead::Aggregate(template) => {
                            Some(template.bind_parameters(params).unwrap())
                        }
                        PreparedNativeRead::PipelineAggregate(template)
                            if !template.requires_relational_input() =>
                        {
                            Some(template.bind_parameters(params).unwrap())
                        }
                        _ => None,
                    };
                    let Some(native) = native else { continue };
                    let historical = db
                        .execute_graph_aggregate_governed_at(&cx, &native, basis, policy())
                        .unwrap();
                    let saved = pinned
                        .execute_graph_aggregate_governed(&cx, &native, policy())
                        .unwrap();
                    assert_eq!(historical.value, saved.value, "historical {text}");
                    let measured = db
                        .execute_graph_aggregate_governed(&cx, &native, policy())
                        .unwrap();
                    let caps = [
                        measured.rows.snapshot_records,
                        measured.rows.result_rows,
                        measured.evaluator.work_units,
                        measured.evaluator.scratch_entries,
                    ];
                    assert_eq!(
                        db.execute_graph_aggregate_governed(
                            &cx,
                            &native,
                            GqlQueryPolicy::new(caps[0], caps[1], caps[2], caps[3]),
                        )
                        .unwrap(),
                        measured
                    );
                    for dimension in 0..4 {
                        let mut smaller = caps;
                        assert!(smaller[dimension] > 0);
                        smaller[dimension] -= 1;
                        assert!(
                            db.execute_graph_aggregate_governed(
                                &cx,
                                &native,
                                GqlQueryPolicy::new(smaller[0], smaller[1], smaller[2], smaller[3]),
                            )
                            .is_err(),
                            "{text}, dimension={dimension}"
                        );
                    }
                }
            };
            check(&db, &old);
            let mut later = WriteBatch::new(R);
            later.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(10)));
            later.set_vertex_property(VId(2), P, None);
            later.set_vertex_property(VId(4), P, Some(CanonicalScalar::Null));
            later.set_edge_property(EId(10), P, Some(CanonicalScalar::Int(70)));
            later.delete_vertex(VId(3));
            later.create_vertex(VId(9), vec![], vec![(P, CanonicalScalar::Int(90))]);
            later.add_edge(
                EId(90),
                VId(1),
                VId(9),
                vec![(P, CanonicalScalar::Int(900))],
            );
            db.write(&c.commit(), later).await.unwrap();
            let current = vec![
                vec![vec![scalar_list(&[Some(10), None, None, Some(90)])]],
                vec![vec![scalar_list(&[Some(70), Some(900)])]],
                vec![vec![
                    QueryValue::Count(2),
                    scalar_list(&[None, Some(90), Some(70), None, Some(900), None]),
                ]],
                vec![vec![scalar_list(&[
                    None,
                    Some(90),
                    Some(70),
                    None,
                    Some(900),
                    None,
                ])]],
                vec![
                    vec![
                        QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Int(90))),
                        scalar_list(&[Some(90)]),
                    ],
                    vec![
                        QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Int(10))),
                        scalar_list(&[Some(10)]),
                    ],
                ],
                vec![],
            ];
            check(&db, &current);
            db.compact(&c.commit()).await.unwrap();
            check(&db, &current);
            assert_eq!(pinned.frontier(), basis);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn database_and_saved_view_aggregate_carried_properties_at_their_own_cut() {
        let ((), report) = run_async_under_lab(0x71f0_0104, |root| async move {
            let c = PurposeContexts::narrow_runtime_root(&root);
            let cx = c.query();
            let mut db = Database::open_memory(&c.commit(), keys()).await.unwrap();
            seed_edges(&mut db, &c.commit()).await;
            let basis = db.frontier().unwrap();
            let pinned = db.read_session().unwrap();
            let params = parameters(&[
                ElementId::Vertex(VId(1)),
                ElementId::Vertex(VId(2)),
                ElementId::Vertex(VId(3)),
                ElementId::Vertex(VId(4)),
                ElementId::Vertex(VId(9)),
                ElementId::Vertex(VId(999)),
                ElementId::Edge(EId(10)),
                ElementId::Edge(EId(11)),
                ElementId::Edge(EId(90)),
                ElementId::Edge(EId(999)),
            ]);
            let plans = [FOLDED, MATERIALIZED].map(|text| {
                let PreparedNativeRead::PipelineAggregate(template) =
                    PreparedNativeRead::prepare(text, &params, symbols).unwrap()
                else {
                    panic!("the public aggregate pipeline classification")
                };
                (text, template.bind_relation_parameters(&params).unwrap())
            });
            let expected =
                |count, total| vec![vec![QueryValue::Count(count), QueryValue::Integer(total)]];
            let check = |db: &Database<MemVfs>, current: (u64, i128)| {
                for (text, query) in &plans {
                    assert_eq!(
                        rows(db.query(&cx, text, &params, symbols, policy()).unwrap()),
                        expected(current.0, current.1)
                    );
                    assert_eq!(
                        rows(pinned.query(&cx, text, &params, symbols, policy()).unwrap()),
                        expected(6, 21)
                    );
                    for mode in 0..3 {
                        let execute = |allowance| match mode {
                            0 => db.execute_graph_set_aggregate_governed(&cx, query, allowance),
                            1 => pinned.execute_graph_set_aggregate_governed(&cx, query, allowance),
                            _ => db.execute_graph_set_aggregate_governed_at(
                                &cx, query, basis, allowance,
                            ),
                        };
                        let measured = execute(policy()).unwrap();
                        let want = if mode == 0 { current } else { (6, 21) };
                        assert_eq!(measured.value.len(), 1);
                        assert_eq!(measured.value[0].values()[0].as_count(), Some(want.0));
                        assert_eq!(measured.value[0].values()[1].as_integer(), Some(want.1));
                        let caps = [
                            measured.rows.snapshot_records,
                            measured.rows.result_rows,
                            measured.evaluator.work_units,
                            measured.evaluator.scratch_entries,
                        ];
                        // Supplied identities remain expression inputs; reading
                        // their properties does not manufacture a graph scan.
                        assert_eq!(caps[0], 0);
                        assert_eq!(
                            execute(GqlQueryPolicy::new(caps[0], caps[1], caps[2], caps[3]))
                                .unwrap(),
                            measured
                        );
                        for dimension in 1..4 {
                            let mut smaller = caps;
                            assert!(smaller[dimension] > 0);
                            smaller[dimension] -= 1;
                            assert!(
                                execute(GqlQueryPolicy::new(
                                    smaller[0], smaller[1], smaller[2], smaller[3],
                                ))
                                .is_err()
                            );
                        }
                    }
                }
            };
            check(&db, (6, 21));

            let mut later = WriteBatch::new(R);
            later.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(10)));
            later.set_vertex_property(VId(2), P, None);
            later.set_vertex_property(VId(4), P, Some(CanonicalScalar::Null));
            later.set_edge_property(EId(10), P, Some(CanonicalScalar::Int(70)));
            later.delete_vertex(VId(3));
            later.create_vertex(VId(9), vec![], vec![(P, CanonicalScalar::Int(90))]);
            later.add_edge(
                EId(90),
                VId(1),
                VId(9),
                vec![(P, CanonicalScalar::Int(900))],
            );
            db.write(&c.commit(), later).await.unwrap();
            check(&db, (4, 1070));
            db.compact(&c.commit()).await.unwrap();
            check(&db, (4, 1070));
            assert_eq!(pinned.frontier(), basis);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn carried_vertex_and_edge_properties_follow_the_canonical_overlay_and_saved_basis() {
        let ((), report) = run_async_under_lab(0x71f0_0101, |root| async move {
            let c = PurposeContexts::narrow_runtime_root(&root);
            let mut db = Database::open_memory(&c.commit(), keys()).await.unwrap();
            seed_edges(&mut db, &c.commit()).await;
            let basis = db.frontier().unwrap();
            let mut txn = db.begin(&c.txn()).unwrap();
            let mut prefix = WriteBatch::new(R);
            prefix.create_vertex(VId(8), vec![], vec![(P, CanonicalScalar::Int(80))]);
            txn.write(&mut db, prefix).unwrap();
            txn.savepoint(&db, "prefix").unwrap();
            let saved = txn.staged_effect_digest().unwrap();

            let mut change = WriteBatch::new(R);
            change.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(10)));
            change.set_vertex_property(VId(2), P, None);
            change.set_vertex_property(VId(4), P, Some(CanonicalScalar::Null));
            change.set_edge_property(EId(10), P, Some(CanonicalScalar::Int(70)));
            change.delete_vertex(VId(3));
            change.create_vertex(VId(9), vec![], vec![(P, CanonicalScalar::Int(90))]);
            change.add_edge(
                EId(90),
                VId(1),
                VId(9),
                vec![(P, CanonicalScalar::Int(900))],
            );
            // This requested alias never becomes an edge or acquires properties.
            change.ensure_edge_by_triple(
                EId(999),
                VId(1),
                VId(2),
                vec![(P, CanonicalScalar::Int(666))],
            );
            txn.write(&mut db, change).unwrap();
            let staged = txn.staged_effect_digest().unwrap();
            let elements = [
                ElementId::Vertex(VId(1)),
                ElementId::Vertex(VId(2)),
                ElementId::Vertex(VId(3)),
                ElementId::Vertex(VId(4)),
                ElementId::Vertex(VId(8)),
                ElementId::Vertex(VId(9)),
                ElementId::Vertex(VId(999)),
                ElementId::Edge(EId(10)),
                ElementId::Edge(EId(11)),
                ElementId::Edge(EId(90)),
                ElementId::Edge(EId(999)),
            ];
            let params = parameters(&elements);
            let expected = |values: &[Option<i64>]| {
                elements
                    .iter()
                    .copied()
                    .zip(
                        values
                            .iter()
                            .map(|value| value.map_or(CanonicalScalar::Null, CanonicalScalar::Int)),
                    )
                    .collect::<BTreeMap<_, _>>()
            };
            assert_eq!(
                properties(read(&txn, &db, &c.query(), ROWS, &params)),
                expected(&[
                    Some(10),
                    None,
                    None,
                    None,
                    Some(80),
                    Some(90),
                    None,
                    Some(70),
                    None,
                    Some(900),
                    None,
                ])
            );
            // Element expressions retain their own work/copy allowance; an
            // identity carried by UNWIND does not manufacture a pattern scan.
            let relation = PreparedGraphSetText::prepare(ROWS, symbols)
                .unwrap()
                .bind_parameters(&params)
                .unwrap();
            let measured = txn
                .execute_graph_set_governed(&db, &c.query(), &relation, policy())
                .unwrap();
            let caps = [
                measured.rows.snapshot_records,
                measured.rows.result_rows,
                measured.evaluator.work_units,
                measured.evaluator.scratch_entries,
            ];
            assert_eq!(caps[0], 0);
            assert_eq!(
                txn.execute_graph_set_governed(
                    &db,
                    &c.query(),
                    &relation,
                    GqlQueryPolicy::new(caps[0], caps[1], caps[2], caps[3]),
                )
                .unwrap(),
                measured
            );
            for dimension in 1..4 {
                let mut smaller = caps;
                smaller[dimension] -= 1;
                assert!(
                    txn.execute_graph_set_governed(
                        &db,
                        &c.query(),
                        &relation,
                        GqlQueryPolicy::new(smaller[0], smaller[1], smaller[2], smaller[3]),
                    )
                    .is_err()
                );
            }
            for text in [FOLDED, MATERIALIZED] {
                assert_eq!(
                    rows(read(&txn, &db, &c.query(), text, &params)),
                    vec![vec![QueryValue::Count(5), QueryValue::Integer(1150)]]
                );
            }
            for (text, values) in [
                (
                    OUTPUT,
                    vec![
                        Some(10),
                        None,
                        None,
                        None,
                        Some(80),
                        Some(90),
                        None,
                        Some(70),
                        None,
                        Some(900),
                        None,
                    ],
                ),
                (
                    VERTEX_OUTPUT,
                    vec![Some(10), None, None, Some(80), Some(90)],
                ),
                (EDGE_OUTPUT, vec![Some(70), Some(900)]),
            ] {
                // Only OUTPUT reads $elements; an unused declaration refuses.
                let bound = if text.contains("$elements") {
                    params.clone()
                } else {
                    GqlParameters::new()
                };
                assert_eq!(
                    rows(read(&txn, &db, &c.query(), text, &bound)),
                    vec![vec![scalar_list(&values)]],
                    "staged aggregate output {text}"
                );
            }
            // Path-produced values use the same callback as supplied identities.
            let path = "MATCH path = (a)-[:R]->(b) \
                RETURN [n IN nodes(path) | n.p] AS vertices, \
                [e IN relationships(path) | e.p] AS edges";
            let path_rows = rows(read(&txn, &db, &c.query(), path, &GqlParameters::new()));
            let list = |values: &[Option<i64>]| {
                QueryValue::Value(GraphValue::List(
                    values
                        .iter()
                        .map(|value| {
                            GraphValue::Scalar(
                                value.map_or(CanonicalScalar::Null, CanonicalScalar::Int),
                            )
                        })
                        .collect::<Vec<_>>()
                        .into(),
                ))
            };
            assert_eq!(path_rows.len(), 2);
            assert!(path_rows.contains(&vec![list(&[Some(10), None]), list(&[Some(70)])]));
            assert!(path_rows.contains(&vec![list(&[Some(10), Some(90)]), list(&[Some(900)])]));
            assert_eq!(txn.staged_effect_digest().unwrap(), staged);
            assert_eq!(db.frontier().unwrap(), basis);

            txn.rollback_to_savepoint(&db, "prefix").unwrap();
            assert_eq!(txn.staged_effect_digest().unwrap(), saved);
            let before = expected(&[
                Some(1),
                Some(2),
                Some(3),
                Some(0),
                Some(80),
                None,
                None,
                Some(7),
                Some(8),
                None,
                None,
            ]);
            assert_eq!(
                properties(read(&txn, &db, &c.query(), ROWS, &params)),
                before
            );
            let mut later = WriteBatch::new(R);
            later.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(1000)));
            later.set_edge_property(EId(10), P, Some(CanonicalScalar::Int(7000)));
            db.write(&c.commit(), later).await.unwrap();
            assert_eq!(
                properties(read(&txn, &db, &c.query(), ROWS, &params)),
                before
            );
            for text in [FOLDED, MATERIALIZED] {
                assert_eq!(
                    rows(read(&txn, &db, &c.query(), text, &params)),
                    vec![vec![QueryValue::Count(7), QueryValue::Integer(101)]]
                );
            }
            for (text, values) in [
                (
                    OUTPUT,
                    vec![
                        Some(1),
                        Some(2),
                        Some(3),
                        Some(0),
                        Some(80),
                        None,
                        None,
                        Some(7),
                        Some(8),
                        None,
                        None,
                    ],
                ),
                (
                    VERTEX_OUTPUT,
                    vec![Some(1), Some(2), Some(3), Some(0), Some(80)],
                ),
                (EDGE_OUTPUT, vec![Some(7), Some(8)]),
            ] {
                // Only OUTPUT reads $elements; an unused declaration refuses.
                let bound = if text.contains("$elements") {
                    params.clone()
                } else {
                    GqlParameters::new()
                };
                assert_eq!(
                    rows(read(&txn, &db, &c.query(), text, &bound)),
                    vec![vec![scalar_list(&values)]],
                    "rolled-back aggregate output at saved basis {text}"
                );
            }
            txn.abort();
            assert_eq!(c.outstanding_obligations(), 0);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn supplied_element_reads_retain_field_absence_and_cascade_conflicts() {
        let ((), report) = run_async_under_lab(0x71f0_0102, |root| async move {
            let c = PurposeContexts::narrow_runtime_root(&root);
            for (case, target, conflicts) in [
                (0, ElementId::Vertex(VId(1)), true),
                (1, ElementId::Vertex(VId(1)), false),
                (2, ElementId::Vertex(VId(99)), true),
                (3, ElementId::Edge(EId(10)), true),
                (4, ElementId::Edge(EId(10)), false),
                (5, ElementId::Edge(EId(10)), true),
                (6, ElementId::Edge(EId(99)), true),
                (7, ElementId::Vertex(VId(1)), true),
                (8, ElementId::Vertex(VId(4)), true),
                (9, ElementId::Edge(EId(11)), true),
                (10, ElementId::Edge(EId(10)), true),
            ] {
                for text in [
                    ROWS,
                    FOLDED,
                    MATERIALIZED,
                    OUTPUT,
                    "UNWIND $elements AS element \
                        RETURN [item IN COLLECT(element) | item.p] AS values LIMIT 0",
                    "UNWIND $elements AS element RETURN element.p LIMIT 0",
                ] {
                    let mut db = Database::open_memory(&c.commit(), keys()).await.unwrap();
                    seed_edges(&mut db, &c.commit()).await;
                    if case == 8 || case == 9 {
                        let mut remove = WriteBatch::new(R);
                        if case == 8 {
                            remove.set_vertex_property(VId(4), P, None);
                        } else {
                            remove.set_edge_property(EId(11), P, None);
                        }
                        db.write(&c.commit(), remove).await.unwrap();
                    }
                    let mut txn = db.begin(&c.txn()).unwrap();
                    let mut prefix = WriteBatch::new(R);
                    prefix.create_vertex(VId(1000), vec![], vec![]);
                    txn.write(&mut db, prefix).unwrap();
                    let _ = read(&txn, &db, &c.query(), text, &parameters(&[target]));
                    let mut winner = WriteBatch::new(R);
                    match case {
                        0 | 7 => {
                            winner.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(50)))
                        }
                        1 => winner.set_vertex_property(
                            VId(1),
                            PropertyKeyId(2),
                            Some(CanonicalScalar::Int(50)),
                        ),
                        2 => winner.create_vertex(
                            VId(99),
                            vec![],
                            vec![(P, CanonicalScalar::Int(50))],
                        ),
                        3 => winner.set_edge_property(EId(10), P, Some(CanonicalScalar::Int(50))),
                        4 => winner.set_edge_property(
                            EId(10),
                            PropertyKeyId(2),
                            Some(CanonicalScalar::Int(50)),
                        ),
                        5 => winner.delete_vertex(VId(2)),
                        6 => winner.add_edge(
                            EId(99),
                            VId(1),
                            VId(2),
                            vec![(P, CanonicalScalar::Int(50))],
                        ),
                        8 => winner.set_vertex_property(VId(4), P, Some(CanonicalScalar::Int(50))),
                        9 => winner.set_edge_property(EId(11), P, Some(CanonicalScalar::Int(50))),
                        10 => winner.delete_edge(EId(10)),
                        _ => unreachable!(),
                    };
                    db.write(&c.commit(), winner).await.unwrap();
                    if case == 7 {
                        // Restoring a value cannot erase its intervening write.
                        db.write(&c.commit(), change(1, 1)).await.unwrap();
                    }
                    let frontier = db.frontier().unwrap();
                    let result = txn.finish(&mut db, &c.commit()).await;
                    if conflicts {
                        assert!(
                            matches!(
                                result,
                                Err(WriteTxnError::Write(WriteError::FirstCommitterWins { .. }))
                            ),
                            "case={case}, {text}: {result:?}"
                        );
                        assert_eq!(db.frontier().unwrap(), frontier);
                        assert!(db.vertex(VId(1000)).unwrap().is_none());
                    } else {
                        result.unwrap();
                        assert!(db.vertex(VId(1000)).unwrap().is_some());
                    }
                    assert_eq!(c.outstanding_obligations(), 0);
                }
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn every_property_checkpoint_preserves_interruption_retry_and_refused_read_witnesses() {
        let ((), report) = run_async_under_lab(0x71f0_0103, |root| async move {
            let c = PurposeContexts::narrow_runtime_root(&root);
            for text in [
                ROWS,
                FOLDED,
                MATERIALIZED,
                OUTPUT,
                "MATCH (n) WHERE n.p = 99 RETURN COUNT(*) AS n, \
                    [item IN $elements | item.p] AS values",
            ] {
                let mut db = Database::open_memory(&c.commit(), keys()).await.unwrap();
                seed_edges(&mut db, &c.commit()).await;
                let mut txn = db.begin(&c.txn()).unwrap();
                txn.savepoint(&db, "before").unwrap();
                let mut stage = WriteBatch::new(R);
                for id in 1000..1064 {
                    stage.create_vertex(VId(id), vec![], vec![(P, CanonicalScalar::Int(99))]);
                }
                txn.write(&mut db, stage).unwrap();
                let digest = txn.staged_effect_digest().unwrap();
                let params = parameters(&[ElementId::Vertex(VId(1)), ElementId::Edge(EId(10))]);
                let prepared = PreparedNativeRead::prepare(text, &params, symbols).unwrap();
                let probe = Arc::new(SimulationCheckpointProbe::new(None));
                let expected = prepared
                    .execute_in_transaction(
                        &txn,
                        &db,
                        &c.query().with_checkpoint_probe(Arc::clone(&probe)),
                        &params,
                        policy(),
                    )
                    .unwrap();
                let calls = probe.calls();
                assert!(
                    calls > 64,
                    "the canonical prefix is polled during each point read"
                );
                for stop in 1..=calls {
                    let probe = Arc::new(SimulationCheckpointProbe::new(Some(stop)));
                    let result = prepared.execute_in_transaction(
                        &txn,
                        &db,
                        &c.query().with_checkpoint_probe(Arc::clone(&probe)),
                        &params,
                        policy(),
                    );
                    let interrupted = match &result {
                        Err(QueryError::TransactionSet(error)) => {
                            matches!(**error, GqlQueryError::Interrupted(_))
                        }
                        Err(QueryError::TransactionAggregate(error)) => {
                            matches!(**error, GqlQueryError::Interrupted(_))
                        }
                        _ => false,
                    };
                    assert!(interrupted, "{text}, stop={stop}: {result:?}");
                    assert_eq!(
                        probe.calls(),
                        stop,
                        "no source or evaluator continuation after refusal"
                    );
                    assert_eq!(txn.staged_effect_digest().unwrap(), digest);
                }
                assert_eq!(
                    prepared
                        .execute_in_transaction(&txn, &db, &c.query(), &params, policy())
                        .unwrap(),
                    expected
                );
                txn.rollback_to_savepoint(&db, "before").unwrap();
                let mut prefix = WriteBatch::new(R);
                prefix.create_vertex(VId(2000), vec![], vec![]);
                txn.write(&mut db, prefix).unwrap();
                // Successful reads touched only vertex 1 and edge 10. A read
                // interrupted inside the canonical-prefix walk retains the
                // existing allocation-free ALL-CHANGE witness. Neither retry
                // nor rollback may erase it, even for this unrelated mutation.
                db.write(&c.commit(), change(4, 40)).await.unwrap();
                let result = txn.finish(&mut db, &c.commit()).await;
                assert!(
                    matches!(
                        result,
                        Err(WriteTxnError::Write(WriteError::FirstCommitterWins { .. }))
                    ),
                    "{text}: {result:?}"
                );
                assert!(db.vertex(VId(2000)).unwrap().is_none());
                assert_eq!(c.outstanding_obligations(), 0);
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }
}
