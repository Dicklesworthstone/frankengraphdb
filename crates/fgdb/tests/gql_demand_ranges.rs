//! Native demand retrieval must preserve required conflicts without treating
//! the whole graph (or every reached payload) as a read. These tests exercise
//! the ordinary CALL, RETURN, rollback and completion owners, not a mock host.
#![recursion_limit = "256"]

use asupersync::lab::run_async_under_lab;
use fgdb::{
    Database, DatabaseKeys, MemVfs, QueryResult, QueryValue, WriteBatch, WriteError, WriteTxnError,
};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind, PreparedGraphSetText};
use fgdb_types::{
    CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId,
};

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "Node") => Some(GraphSymbol::Label(LabelId(1))),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    }
}

fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100, 10_000_000, 100_000)
}

async fn fixture(cx: &CommitCx) -> Database<MemVfs> {
    let keys = DatabaseKeys::new(
        [0x91; 32],
        DatabaseSecurityNamespaceId([0x92; 32]),
        [0x93; 32],
    );
    let mut db = Database::open_memory(cx, keys).await.unwrap();
    let mut batch = WriteBatch::new(R);
    for id in [0, 1, 3, 9, 10] {
        batch.create_vertex(
            VId(id),
            vec![LabelId(1)],
            vec![(P, CanonicalScalar::Int(7))],
        );
    }
    batch.add_edge(EId(0), VId(0), VId(1), vec![]);
    batch.add_edge(EId(1), VId(1), VId(3), vec![]);
    batch.add_edge(EId(2), VId(10), VId(9), vec![]);
    db.write(cx, batch).await.unwrap();
    db
}

fn query(access: &str, projection: &str) -> String {
    format!(
        "CALL hybrid.search(seeds => [0], max_hops => 2, graph_access => '{access}', \
         label => 'Node', relation => 'R', eligible => [3], k => 1) \
         YIELD node, graph_hops RETURN {projection}"
    )
}

fn rows(result: QueryResult) -> Vec<Vec<QueryValue>> {
    match result {
        QueryResult::Rows { rows, .. } => rows,
        QueryResult::Write { .. } => panic!("read performed a write"),
    }
}

fn assert_conflict<T: core::fmt::Debug>(result: Result<T, WriteTxnError>) {
    assert!(
        matches!(
            &result,
            Err(WriteTxnError::Write(WriteError::FirstCommitterWins {
                law: "FG-LAW-FCW-READ-01",
                ..
            }))
        ),
        "expected retained read conflict: {result:?}"
    );
}

#[test]
fn native_demand_allows_disjoint_writes_that_complete_directory_profiles_still_refuse() {
    let ((), report) = run_async_under_lab(0xbeac_7401, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        for property in [false, true] {
            for access in ["demand", "indexed", "materialized"] {
                let mut db = fixture(&c.commit()).await;
                let mut txn = db.begin(&c.txn()).unwrap();
                let statement = query(access, "node, graph_hops");
                let parameters = GqlParameters::new();
                let expected = vec![vec![
                    QueryValue::Value(GraphValue::Vertex(VId(3))),
                    QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Int(2))),
                ]];
                assert_eq!(
                    rows(
                        txn.query(&db, &c.query(), &statement, &parameters, symbols, policy())
                            .unwrap()
                    ),
                    expected
                );
                let mut concurrent = WriteBatch::new(R);
                if property {
                    concurrent.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(9)));
                } else {
                    concurrent.add_edge(EId(9), VId(9), VId(10), vec![]);
                }
                db.write(&c.commit(), concurrent).await.unwrap();
                assert_eq!(
                    rows(
                        txn.query(&db, &c.query(), &statement, &parameters, symbols, policy())
                            .unwrap()
                    ),
                    expected
                );
                let result = txn.finish(&mut db, &c.commit()).await;
                if access == "demand" {
                    assert!(result.is_ok(), "{property}: {result:?}");
                } else {
                    assert_conflict(result);
                }
            }
        }
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn a_real_return_property_read_remains_a_dependency_even_under_limit_zero() {
    let ((), report) = run_async_under_lab(0xbeac_7402, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        for property in [false, true] {
            for zero in [false, true] {
                let mut db = fixture(&c.commit()).await;
                let mut txn = db.begin(&c.txn()).unwrap();
                txn.savepoint(&db, "before").unwrap();
                let projection = if property { "node.p" } else { "node" };
                let statement = format!(
                    "{}{}",
                    query("demand", projection),
                    if zero { " LIMIT 0" } else { "" }
                );
                let result = rows(
                    txn.query(
                        &db,
                        &c.query(),
                        &statement,
                        &GqlParameters::new(),
                        symbols,
                        policy(),
                    )
                    .unwrap(),
                );
                if zero {
                    assert!(result.is_empty());
                } else {
                    assert_eq!(
                        result,
                        vec![vec![QueryValue::Value(if property {
                            GraphValue::Scalar(CanonicalScalar::Int(7))
                        } else {
                            GraphValue::Vertex(VId(3))
                        })]]
                    );
                }
                txn.rollback_to_savepoint(&db, "before").unwrap();
                let mut concurrent = WriteBatch::new(R);
                concurrent.set_vertex_property(VId(3), P, Some(CanonicalScalar::Int(9)));
                db.write(&c.commit(), concurrent).await.unwrap();
                let result = txn.finish(&mut db, &c.commit()).await;
                if property {
                    assert_conflict(result);
                } else {
                    assert!(
                        result.is_ok(),
                        "unread payload became a dependency: {result:?}"
                    );
                }
            }
        }
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn native_empty_and_late_failed_pages_keep_directional_insertion_gaps_after_rollback() {
    let ((), report) = run_async_under_lab(0xbeac_7403, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        for late_error in [false, true] {
            for outgoing in [false, true] {
                let mut db = fixture(&c.commit()).await;
                let mut txn = db.begin(&c.txn()).unwrap();
                txn.savepoint(&db, "before").unwrap();
                let projection = if late_error { "1 / 0 AS fail" } else { "node" };
                // No relation selector: this must cover future relations too.
                let statement = format!(
                    "CALL hybrid.search(seeds => [9], max_hops => 1, include_seeds => true, \
                     graph_access => 'demand', direction => 'out', k => 1) \
                     YIELD node RETURN {projection} LIMIT 0"
                );
                let result = txn.query(
                    &db,
                    &c.query(),
                    &statement,
                    &GqlParameters::new(),
                    symbols,
                    policy(),
                );
                if late_error {
                    assert!(result.is_err());
                } else {
                    assert!(rows(result.unwrap()).is_empty());
                }
                txn.rollback_to_savepoint(&db, "before").unwrap();
                let mut concurrent = WriteBatch::new(RelationId(77));
                let (src, dst) = if outgoing {
                    (VId(9), VId(3))
                } else {
                    (VId(3), VId(9))
                };
                concurrent.add_edge(EId(u128::MAX), src, dst, vec![]);
                db.write(&c.commit(), concurrent).await.unwrap();
                let result = txn.finish(&mut db, &c.commit()).await;
                if outgoing {
                    assert_conflict(result);
                } else {
                    assert!(
                        result.is_ok(),
                        "unread incoming range became a dependency: {result:?}"
                    );
                }
            }
        }
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn native_projected_witnesses_replay_with_exact_and_one_unit_short_allowances() {
    let ((), report) = run_async_under_lab(0xbeac_7404, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let db = fixture(&c.commit()).await;
        for missing in [false, true] {
            let mut statement = query("demand", "node, graph_hops");
            if missing {
                statement = statement.replace("seeds => [0]", "seeds => [99]");
            }
            let bound = PreparedGraphSetText::prepare(&statement, symbols)
                .unwrap()
                .bind_parameters(&GqlParameters::new())
                .unwrap();
            let run = |allowance| {
                let txn = db.begin(&c.txn()).unwrap();
                let result = txn.execute_graph_set_governed(&db, &c.query(), &bound, allowance);
                txn.abort();
                result
            };
            let baseline = run(policy()).unwrap();
            assert_eq!(baseline.value.len(), usize::from(!missing));
            if missing {
                assert_eq!(baseline.rows.snapshot_records, 0);
            }
            assert!(baseline.evaluator.scratch_entries > 0);
            let exact = GqlQueryPolicy::new(
                baseline.rows.snapshot_records,
                baseline.rows.result_rows,
                baseline.evaluator.work_units,
                baseline.evaluator.scratch_entries,
            );
            let actual = run(exact).unwrap();
            assert_eq!(actual.value, baseline.value);
            assert_eq!(actual.evaluator, baseline.evaluator);
            assert_eq!(actual.rows, baseline.rows);
            let mut short = exact;
            short.evaluator.max_work_units -= 1;
            assert!(run(short).is_err());
            let mut short = exact;
            short.evaluator.max_scratch_entries -= 1;
            assert!(run(short).is_err());
        }
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
