//! Native CALL shares source, traversal and fusion scratch admission.
#![recursion_limit = "256"]

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, QueryResult, QueryValue, WriteBatch};
use fgdb_delta_types::RelationId;
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind, PreparedGraphSetText};
use fgdb_types::{CommitCx, DatabaseSecurityNamespaceId, PurposeContexts, VId};

fn symbols(_: GraphSymbolKind, _: &str) -> Option<GraphSymbol> {
    None
}

fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(1000, 100, 1_000_000, 100_000)
}

async fn open(cx: &CommitCx) -> Database<MemVfs> {
    let mut db = Database::open_memory(
        cx,
        DatabaseKeys::new([0x61; 32], DatabaseSecurityNamespaceId([0x62; 32]), [0x63; 32]),
    ).await.unwrap();
    let mut batch = WriteBatch::new(RelationId(1));
    for id in [1, 2, 9] {
        batch.create_vertex(VId(id), vec![], vec![]);
    }
    db.write(cx, batch).await.unwrap();
    db
}

fn rows(result: QueryResult) -> Vec<Vec<QueryValue>> {
    match result {
        QueryResult::Rows { rows, .. } => rows,
        QueryResult::Write { .. } => panic!("retrieval wrote the database"),
    }
}

fn parameters(seeds: &[u128], empty: bool) -> GqlParameters {
    GqlParameters::new()
        .with_list("seeds", seeds.iter().map(|&id| GraphValue::Vertex(VId(id))).collect())
        .unwrap()
        .with_list("eligible", if empty { vec![] } else {
            seeds.iter().map(|&id| GraphValue::Vertex(VId(id))).collect()
        })
        .unwrap()
}

#[test]
fn native_exact_and_short_budgets_include_kernels_in_both_hosts() {
    let ((), report) = run_async_under_lab(0xbeac_5501, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let db = open(&c.commit()).await;
        for transaction in [false, true] {
            for empty in [false, true] {
                for limit_zero in [false, true] {
                    let text = format!(
                        "CALL hybrid.search(seeds => $seeds, max_hops => 0, \
                         include_seeds => true, graph_access => 'demand', \
                         eligible => $eligible, candidates => 4, k => 4) \
                         YIELD node RETURN node{}",
                        if limit_zero { " LIMIT 0" } else { "" },
                    );
                    let parameters = parameters(&[2, 1, 2, 99], empty);
                    let bound = PreparedGraphSetText::prepare(&text, symbols).unwrap()
                        .bind_parameters(&parameters).unwrap();
                    // Fresh transaction witnesses for every invocation. A
                    // baseline call must not warm a short-budget attempt.
                    let execute = |allowance| {
                        if transaction {
                            let mut txn = db.begin(&c.txn()).unwrap();
                            let result = txn.execute_graph_set_governed(
                                &db, &c.query(), &bound, allowance,
                            ).map_err(|error| error.to_string());
                            txn.abort();
                            result
                        } else {
                            db.execute_graph_set_governed(&c.query(), &bound, allowance)
                                .map_err(|error| error.to_string())
                        }
                    };
                    let baseline = execute(policy()).unwrap();
                    assert_eq!(baseline.value.len(), if empty || limit_zero { 0 } else { 2 });
                    // Source + distance/queue state survives an empty endpoint
                    // filter. Accepting two endpoints also retains both rankers.
                    assert!(baseline.evaluator.scratch_entries >= if empty { 6 } else { 20 });
                    let exact = GqlQueryPolicy::new(
                        baseline.rows.snapshot_records,
                        baseline.rows.result_rows,
                        baseline.evaluator.work_units,
                        baseline.evaluator.scratch_entries,
                    );
                    let replay = execute(exact).unwrap();
                    assert_eq!(replay.value, baseline.value);
                    assert_eq!(replay.rows, baseline.rows);
                    assert_eq!(replay.evaluator, baseline.evaluator);
                    let mut short = exact;
                    short.evaluator.max_scratch_entries -= 1;
                    assert!(execute(short).is_err());
                    let mut short = exact;
                    short.evaluator.max_work_units -= 1;
                    assert!(execute(short).is_err());
                    // Independently named identities verify native result
                    // conversion, not just equality with a metered baseline.
                    let expected = if empty || limit_zero { vec![] } else {
                        vec![1, 2].into_iter().map(|id| {
                            vec![QueryValue::Value(GraphValue::Vertex(VId(id)))]
                        }).collect()
                    };
                    if transaction {
                        let mut txn = db.begin(&c.txn()).unwrap();
                        assert_eq!(rows(txn.query(
                            &db, &c.query(), &text, &parameters, symbols, policy(),
                        ).unwrap()), expected);
                        txn.abort();
                    } else {
                        assert_eq!(rows(db.query(
                            &c.query(), &text, &parameters, symbols, policy(),
                        ).unwrap()), expected);
                    }
                }
            }
        }
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn refused_native_scratch_preserves_staged_state_and_savepoints() {
    let ((), report) = run_async_under_lab(0xbeac_5502, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let mut db = open(&c.commit()).await;
        let basis = db.frontier().unwrap();
        let text = "CALL hybrid.search(seeds => [3], max_hops => 0, \
            include_seeds => true, graph_access => 'demand', k => 1) \
            YIELD node RETURN node";
        let bound = PreparedGraphSetText::prepare(text, symbols).unwrap()
            .bind_parameters(&GqlParameters::new()).unwrap();
        let mut baseline = db.begin(&c.txn()).unwrap();
        let mut edit = WriteBatch::new(RelationId(1));
        edit.create_vertex(VId(3), vec![], vec![]);
        baseline.write(&mut db, edit.clone()).unwrap();
        let counts = baseline.execute_graph_set_governed(
            &db, &c.query(), &bound, policy(),
        ).unwrap();
        baseline.abort();
        let mut txn = db.begin(&c.txn()).unwrap();
        txn.write(&mut db, edit).unwrap();
        txn.savepoint(&db, "staged").unwrap();
        let digest = txn.staged_effect_digest().unwrap();
        let mut short = policy();
        short.evaluator.max_scratch_entries = counts.evaluator.scratch_entries - 1;
        assert!(txn.execute_graph_set_governed(&db, &c.query(), &bound, short).is_err());
        assert_eq!(txn.staged_effect_digest().unwrap(), digest);
        assert_eq!(db.frontier().unwrap(), basis);
        txn.rollback_to_savepoint(&db, "staged").unwrap();
        assert_eq!(txn.staged_effect_digest().unwrap(), digest);
        assert_eq!(rows(txn.query(
            &db, &c.query(), text, &GqlParameters::new(), symbols, policy(),
        ).unwrap()), vec![vec![QueryValue::Value(GraphValue::Vertex(VId(3)))]]);
        txn.abort();
        assert!(rows(db.query(
            &c.query(), text, &GqlParameters::new(), symbols, policy(),
        ).unwrap()).is_empty());
        assert_eq!(db.frontier().unwrap(), basis);
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
