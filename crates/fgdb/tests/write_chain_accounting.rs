//! The embedded chain adapter must retain each native step's complete costs,
//! not just the prefix through its writes. The reference below executes the
//! actual CREATE collectors independently of the adapter being checked.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, WriteBatch};
use fgdb_delta_types::{ElementId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GraphValue, GraphValueRow, PreparedGraphPattern};
use fgdb_gql::insertion::{GraphInsertPolicy, GraphInsertRequest};
use fgdb_gql::{
    GlaExecutionStats, GlaLimitDimension, GqlExecutionStats, GqlParameters, GqlQueryError,
    GqlQueryExecution, GqlQueryPolicy, GraphSymbol, GraphSymbolKind, GraphWriteChainStep,
    PreparedGraphWriteChain, PreparedGraphWriteChainText,
};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};
use std::sync::atomic::{AtomicU64, Ordering};

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);
const HIGH: u64 = 10_000_000;
const GROUPED: &str = "UNWIND [1,2,3] AS x CREATE (a {p:x}) \
    WITH a.p AS p,size(range(1,32)) AS n CREATE (b {p:p+n}) \
    RETURN sum(b.p) AS total";

#[test]
fn row_fed_edge_actions_preserve_occurrences_final_state_and_statement_rollback() {
    let ((), report) = run_async_under_lab(0xc4a1_1006, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let chain = prepare(
            "UNWIND [2,3,4] AS w MERGE (a {p:10}) MERGE (b {p:20}) \
             MERGE (a)-[r:R]->(b) ON CREATE SET r.p=w,r.q=1 \
             ON MATCH SET r.p=r.p+w,r.q=r.q+1 SET r.p=r.p+10 RETURN r.p,r.q",
        );
        let mut database = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        let basis = database.frontier().unwrap();
        let mut transaction = database.begin(&contexts.txn()).unwrap();
        let mut prefix = WriteBatch::new(R);
        prefix.create_vertex(VId(7), vec![], vec![(P, CanonicalScalar::Int(99))]);
        transaction.write(&mut database, prefix).unwrap();
        let digest = transaction.staged_effect_digest().unwrap();
        let next = AtomicU64::new(1_000);
        for (rows, effects) in [(3, 8), (2, 9)] {
            transaction
                .execute_graph_write_chain_governed(
                    &mut database,
                    &contexts.query(),
                    &chain,
                    policy(rows, HIGH, HIGH).with_mutation_limit(effects),
                    |request| allocate(&next, request),
                )
                .expect_err("late action or final-row refusal must discard the whole chain");
            assert_eq!(transaction.staged_effect_digest().unwrap(), digest);
            assert_eq!(database.frontier().unwrap(), basis);
            assert_eq!(transaction.vertices(&database).unwrap().len(), 1);
            assert!(transaction.edges(&database).unwrap().is_empty());
        }
        let (stats, result, receipt) = transaction
            .execute_graph_write_chain_governed(
                &mut database,
                &contexts.query(),
                &chain,
                policy(3, HIGH, HIGH).with_mutation_limit(9),
                |request| allocate(&next, request),
            )
            .unwrap();
        assert_eq!(stats.created_vertices, 2);
        assert_eq!(stats.created_edges, 1);
        assert_eq!(receipt.stats().mutation_effects, 9);
        assert_eq!(result.value.len(), 3);
        for row in &result.value {
            assert_eq!(
                row.values(),
                &[
                    GraphValue::Scalar(CanonicalScalar::Int(39)),
                    GraphValue::Scalar(CanonicalScalar::Int(3)),
                ]
            );
        }
        transaction
            .finish(&mut database, &contexts.commit())
            .await
            .unwrap();
        assert_eq!(database.frontier().unwrap().0, basis.0 + 1);
        assert_eq!(database.edges().unwrap().len(), 1);
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn edge_actions_freeze_parallel_targets_reprobe_changed_keys_and_keep_orientation() {
    let ((), report) = run_async_under_lab(0xc4a1_1007, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let mut database = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(1), vec![], vec![(P, CanonicalScalar::Int(10))]);
        seed.create_vertex(VId(2), vec![], vec![(P, CanonicalScalar::Int(20))]);
        seed.add_edge(
            EId(11),
            VId(2),
            VId(1),
            vec![(P, CanonicalScalar::Int(1)), (Q, CanonicalScalar::Int(2))],
        );
        seed.add_edge(
            EId(12),
            VId(1),
            VId(2),
            vec![(P, CanonicalScalar::Int(1)), (Q, CanonicalScalar::Int(3))],
        );
        database.write(&contexts.commit(), seed).await.unwrap();
        let chain = prepare(
            "UNWIND [0,1] AS i MERGE (a {p:10}) MERGE (b {p:20}) \
             MERGE (a)-[r:R {p:1}]-(b) ON MATCH SET r.p=2 ON CREATE SET r.q=9 \
             RETURN r.p,r.q,startNode(r),endNode(r)",
        );
        let mut transaction = database.begin(&contexts.txn()).unwrap();
        let next = AtomicU64::new(1_000);
        let mut allocated_rows = Vec::new();
        let (stats, result, receipt) = transaction
            .execute_graph_write_chain_governed(
                &mut database,
                &contexts.query(),
                &chain,
                policy(3, HIGH, HIGH).with_mutation_limit(3),
                |request| {
                    if let GraphInsertRequest::Edge { row, .. } = request {
                        allocated_rows.push(row);
                    }
                    allocate(&next, request)
                },
            )
            .unwrap();
        assert_eq!(
            allocated_rows,
            [1],
            "allocation keeps the original occurrence index"
        );
        assert_eq!(stats.created_edges, 1);
        assert_eq!(receipt.stats().mutation_effects, 3);
        assert_eq!(result.value.len(), 3);
        for (at, p, q, source, destination) in [
            (0, 2, 2, VId(2), VId(1)),
            (1, 2, 3, VId(1), VId(2)),
            (2, 1, 9, VId(1), VId(2)),
        ] {
            assert_eq!(
                result.value[at].values(),
                &[
                    GraphValue::Scalar(CanonicalScalar::Int(p)),
                    GraphValue::Scalar(CanonicalScalar::Int(q)),
                    GraphValue::Vertex(source),
                    GraphValue::Vertex(destination),
                ]
            );
        }
        transaction
            .finish(&mut database, &contexts.commit())
            .await
            .unwrap();
        assert_eq!(database.edges().unwrap().len(), 3);
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn edge_action_clauses_are_simultaneous_and_feed_later_writes_without_public_rows() {
    let ((), report) = run_async_under_lab(0xc4a1_1008, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        for tail in ["", " WITH r CREATE (c {p:r.p,q:r.q}) RETURN c.p,c.q"] {
            let mut database = Database::open_memory(&contexts.commit(), keys())
                .await
                .unwrap();
            let mut seed = WriteBatch::new(R);
            seed.create_vertex(VId(1), vec![], vec![(P, CanonicalScalar::Int(10))]);
            seed.create_vertex(VId(2), vec![], vec![(P, CanonicalScalar::Int(20))]);
            seed.add_edge(
                EId(11),
                VId(1),
                VId(2),
                vec![(P, CanonicalScalar::Int(1)), (Q, CanonicalScalar::Int(2))],
            );
            database.write(&contexts.commit(), seed).await.unwrap();
            let chain = prepare(&format!(
                "MERGE (a {{p:10}}) MERGE (b {{p:20}}) MERGE (a)-[r:R]->(b) \
                 ON MATCH SET r.p=r.q,r.q=r.p SET r.p=r.p+10{tail}",
            ));
            let mut transaction = database.begin(&contexts.txn()).unwrap();
            let next = AtomicU64::new(1_000);
            let (_, result, receipt) = transaction
                .execute_graph_write_chain_governed(
                    &mut database,
                    &contexts.query(),
                    &chain,
                    policy(u64::from(!tail.is_empty()), HIGH, HIGH).with_mutation_limit(3),
                    |request| allocate(&next, request),
                )
                .unwrap();
            assert_eq!(receipt.stats().mutation_effects, 3);
            if tail.is_empty() {
                assert!(result.value.is_empty());
                assert_eq!(result.rows.result_rows, 0);
            } else {
                assert_eq!(
                    result.value[0].values(),
                    &[
                        GraphValue::Scalar(CanonicalScalar::Int(12)),
                        GraphValue::Scalar(CanonicalScalar::Int(1)),
                    ]
                );
            }
            assert_eq!(
                transaction.edge(&database, EId(11)).unwrap().unwrap().props,
                vec![
                    (P, CanonicalScalar::Int(12).into()),
                    (Q, CanonicalScalar::Int(1).into())
                ]
            );
            transaction
                .finish(&mut database, &contexts.commit())
                .await
                .unwrap();
            assert_eq!(contexts.txn().outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0xd1; 32],
        DatabaseSecurityNamespaceId([0xd2; 32]),
        [0xd3; 32],
    )
}

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
        _ => None,
    }
}

fn policy(rows: u64, work: u64, scratch: u64) -> GraphInsertPolicy {
    GraphInsertPolicy::new(
        GqlQueryPolicy::new(10_000, rows, work, scratch),
        1_000,
        1_000,
    )
}

fn prepare(text: &str) -> PreparedGraphWriteChain {
    assert!(
        PreparedGraphWriteChainText::is_chain(text).unwrap(),
        "{text}"
    );
    PreparedGraphWriteChainText::prepare(text, R, symbols)
        .expect(text)
        .bind_parameters(&GqlParameters::new())
        .expect(text)
}

fn no_graph(
    _: &PreparedGraphPattern<GraphValueRow>,
    _: GqlQueryPolicy,
) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<(), ()>> {
    panic!("the source-free CREATE control must not read a graph")
}

fn allocate(next: &AtomicU64, request: GraphInsertRequest) -> Result<ElementId, ()> {
    let id = u128::from(next.fetch_add(1, Ordering::Relaxed));
    Ok(match request {
        GraphInsertRequest::Vertex { .. } => ElementId::Vertex(VId(id)),
        GraphInsertRequest::Edge { .. } => ElementId::Edge(EId(id)),
    })
}

struct Reference {
    full: GlaExecutionStats,
    write_prefix: GlaExecutionStats,
    records: u64,
    vertices: u64,
    rows: Vec<GraphValueRow>,
}

/// Ask the native per-step engines for their full executions, with no storage
/// adapter and no copied budget-composition implementation. Keep prefix costs
/// separately so the test proves that the missing RETURN/WITH cost is nonzero.
fn reference(chain: &PreparedGraphWriteChain) -> Reference {
    let mut expected = Reference {
        full: GlaExecutionStats::default(),
        write_prefix: GlaExecutionStats::default(),
        records: 0,
        vertices: 0,
        rows: Vec::new(),
    };
    let next = AtomicU64::new(1_000);
    let mut input = None;
    for step in chain.steps() {
        let (prefix, result) = match step {
            GraphWriteChainStep::Query(query) => {
                let proposal = query
                    .execute_governed_over(
                        input.take(),
                        policy(u64::MAX, HIGH, HIGH),
                        no_graph,
                        |request| allocate(&next, request),
                        || Ok(()),
                    )
                    .unwrap();
                let (insertion, result) = proposal.into_parts();
                (insertion.stats(), result)
            }
            GraphWriteChainStep::Insert(insertion) => {
                let proposal = insertion
                    .execute_governed_over(
                        input.take(),
                        policy(u64::MAX, HIGH, HIGH),
                        no_graph,
                        |request| allocate(&next, request),
                        || Ok(()),
                    )
                    .unwrap();
                let prefix = proposal.stats();
                let result = GqlQueryExecution {
                    value: Vec::new(),
                    rows: GqlExecutionStats {
                        snapshot_records: prefix.selection.snapshot_records,
                        result_rows: 0,
                    },
                    evaluator: prefix.evaluator,
                };
                (prefix, result)
            }
            _ => panic!("the independent control only admits source-free CREATE steps"),
        };
        expected.full.work_units += result.evaluator.work_units;
        expected.full.scratch_entries += result.evaluator.scratch_entries;
        expected.write_prefix.work_units += prefix.evaluator.work_units;
        expected.write_prefix.scratch_entries += prefix.evaluator.scratch_entries;
        expected.records += result.rows.snapshot_records;
        expected.vertices += prefix.created_vertices;
        input = Some(result.value);
    }
    expected.rows = input.unwrap();
    assert!(expected.full.work_units > expected.write_prefix.work_units);
    assert!(expected.full.scratch_entries > expected.write_prefix.scratch_entries);
    expected
}

#[test]
fn chain_counters_equal_complete_native_steps_not_write_prefixes() {
    let ((), report) = run_async_under_lab(0xc4a1_1001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        for text in [
            GROUPED.to_owned(),
            format!("{GROUPED} LIMIT 0"),
            "CREATE (a) WITH size(range(1,64)) AS n CREATE (b {p:n})".to_owned(),
        ] {
            let chain = prepare(&text);
            let expected = reference(&chain);
            let mut database = Database::open_memory(&contexts.commit(), keys())
                .await
                .unwrap();
            let basis = database.frontier().unwrap();
            let mut transaction = database.begin(&contexts.txn()).unwrap();
            let next = AtomicU64::new(1_000);
            let (stats, result, receipt) = transaction
                .execute_graph_write_chain_governed(
                    &mut database,
                    &contexts.query(),
                    &chain,
                    policy(expected.rows.len() as u64, HIGH, HIGH),
                    |request| allocate(&next, request),
                )
                .unwrap();
            assert_eq!(result.value, expected.rows, "{text}");
            assert_eq!(stats.evaluator, expected.full, "{text}");
            assert_eq!(result.evaluator, expected.full, "{text}");
            assert_eq!(receipt.stats().evaluator, expected.full, "{text}");
            assert_eq!(stats.selection.snapshot_records, expected.records);
            assert_eq!(stats.selection.result_rows, expected.rows.len() as u64);
            assert_eq!(stats.created_vertices, expected.vertices);
            assert_eq!(receipt.stats().created_vertices, expected.vertices);
            assert_eq!(receipt.stats().completed_statements, 1);
            assert_eq!(database.frontier().unwrap(), basis);
            if text == GROUPED {
                assert_eq!(
                    result.value[0].values(),
                    &[GraphValue::Scalar(CanonicalScalar::Int(102))],
                );
                assert_eq!(expected.vertices, 6);
            }
            transaction
                .finish(&mut database, &contexts.commit())
                .await
                .unwrap();
            assert_eq!(database.frontier().unwrap().0, basis.0 + 1);
            assert_eq!(database.vertices().unwrap().len() as u64, expected.vertices);
            assert_eq!(contexts.txn().outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn exact_complete_budgets_pass_and_one_below_restores_the_staged_prefix() {
    let ((), report) = run_async_under_lab(0xc4a1_1002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        for text in [GROUPED.to_owned(), format!("{GROUPED} LIMIT 0")] {
            let chain = prepare(&text);
            let expected = reference(&chain);
            for dimension in [
                GlaLimitDimension::WorkUnits,
                GlaLimitDimension::ScratchEntries,
            ] {
                let mut database = Database::open_memory(&contexts.commit(), keys())
                    .await
                    .unwrap();
                let basis = database.frontier().unwrap();
                let mut transaction = database.begin(&contexts.txn()).unwrap();
                let mut prefix = WriteBatch::new(R);
                prefix.create_vertex(VId(7), vec![], vec![(P, CanonicalScalar::Int(99))]);
                transaction.write(&mut database, prefix).unwrap();
                let digest = transaction.staged_effect_digest().unwrap();
                let next = AtomicU64::new(1_000);
                let mut tight = policy(expected.rows.len() as u64, HIGH, HIGH);
                if dimension == GlaLimitDimension::WorkUnits {
                    tight.query.evaluator.max_work_units = expected.full.work_units - 1;
                } else {
                    tight.query.evaluator.max_scratch_entries = expected.full.scratch_entries - 1;
                }
                let error = transaction
                    .execute_graph_write_chain_governed(
                        &mut database,
                        &contexts.query(),
                        &chain,
                        tight,
                        |request| allocate(&next, request),
                    )
                    .expect_err("a later step cannot reuse work spent by an earlier WITH");
                assert!(
                    matches!(error, GqlQueryError::Evaluator(limit) if limit.dimension == dimension)
                );
                assert_eq!(transaction.staged_effect_digest().unwrap(), digest);
                assert_eq!(transaction.vertices(&database).unwrap().len(), 1);
                assert_eq!(database.frontier().unwrap(), basis);
                assert!(database.vertices().unwrap().is_empty());
                // The same definition and transaction must remain usable. The
                // monotonically advancing allocator never reuses failed IDs.
                let (stats, result, _) = transaction
                    .execute_graph_write_chain_governed(
                        &mut database,
                        &contexts.query(),
                        &chain,
                        policy(
                            expected.rows.len() as u64,
                            expected.full.work_units,
                            expected.full.scratch_entries,
                        ),
                        |request| allocate(&next, request),
                    )
                    .unwrap();
                assert_eq!(stats.evaluator, expected.full);
                assert_eq!(result.value, expected.rows);
                transaction
                    .finish(&mut database, &contexts.commit())
                    .await
                    .unwrap();
                assert_eq!(database.frontier().unwrap().0, basis.0 + 1);
                assert_eq!(
                    database.vertices().unwrap().len() as u64,
                    expected.vertices + 1
                );
                assert_eq!(contexts.txn().outstanding_obligations(), 0);
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn merge_actions_count_repeated_rows_parallel_targets_and_selected_branches() {
    let ((), report) = run_async_under_lab(0xc4a1_1003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let chain = prepare(
            "UNWIND [1,1] AS k MERGE (a {p:k}) \
             ON CREATE SET a.p=99 ON MATCH SET a.p=1 \
             MERGE (b {p:2}) ON CREATE SET b.p=2",
        );
        let mut database = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        let mut seed = WriteBatch::new(R);
        for id in [1, 2] {
            seed.create_vertex(VId(id), vec![], vec![(P, CanonicalScalar::Int(1))]);
        }
        database.write(&contexts.commit(), seed).await.unwrap();
        let basis = database.frontier().unwrap();
        let mut transaction = database.begin(&contexts.txn()).unwrap();
        let mut prefix = WriteBatch::new(R);
        prefix.create_vertex(VId(7), vec![], vec![(P, CanonicalScalar::Int(99))]);
        transaction.write(&mut database, prefix).unwrap();
        let digest = transaction.staged_effect_digest().unwrap();
        let next = AtomicU64::new(1_000);
        let error = transaction
            .execute_graph_write_chain_governed(
                &mut database,
                &contexts.query(),
                &chain,
                policy(0, HIGH, HIGH).with_mutation_limit(4),
                |request| allocate(&next, request),
            )
            .expect_err("four matched actions leave no allowance for the next creation's SET");
        assert!(matches!(
            error,
            GqlQueryError::Source(fgdb_gql::GraphWriteChainError::Mutation(
                fgdb_gql::GraphMutationQueryError::Mutation(
                    fgdb_gql::GraphMutationError::EffectLimit {
                        limit: 0,
                        observed: 1
                    }
                )
            ))
        ));
        assert_eq!(transaction.staged_effect_digest().unwrap(), digest);
        assert_eq!(database.frontier().unwrap(), basis);
        assert_eq!(transaction.vertices(&database).unwrap().len(), 3);
        let (stats, result, receipt) = transaction
            .execute_graph_write_chain_governed(
                &mut database,
                &contexts.query(),
                &chain,
                policy(0, HIGH, HIGH).with_mutation_limit(5),
                |request| allocate(&next, request),
            )
            .unwrap();
        assert_eq!(receipt.stats().mutation_effects, 5);
        assert_eq!(stats.created_vertices, 1);
        assert!(result.value.is_empty());
        assert_eq!(result.rows.result_rows, 0);
        transaction
            .finish(&mut database, &contexts.commit())
            .await
            .unwrap();
        assert_eq!(database.frontier().unwrap().0, basis.0 + 1);
        assert_eq!(database.vertices().unwrap().len(), 4);
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn merge_actions_share_the_terminal_mutation_allowance_and_receipt() {
    let ((), report) = run_async_under_lab(0xc4a1_1004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let chain = prepare(
            "UNWIND [1,2] AS k MERGE (a {p:k}) ON CREATE SET a.p=k \
             MERGE (b {p:k+10}) ON CREATE SET b.p=k+10 \
             WITH a,b SET b.p=b.p+100 RETURN b.p",
        );
        let mut database = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        let basis = database.frontier().unwrap();
        let mut transaction = database.begin(&contexts.txn()).unwrap();
        let digest = transaction.staged_effect_digest().unwrap();
        let next = AtomicU64::new(1_000);
        let error = transaction
            .execute_graph_write_chain_governed(
                &mut database,
                &contexts.query(),
                &chain,
                policy(2, HIGH, HIGH).with_mutation_limit(5),
                |request| allocate(&next, request),
            )
            .expect_err("the terminal SET must not reset earlier MERGE action costs");
        assert!(matches!(
            error,
            GqlQueryError::Source(fgdb_gql::GraphWriteChainError::Mutation(
                fgdb_gql::GraphMutationQueryError::Mutation(
                    fgdb_gql::GraphMutationError::EffectLimit {
                        limit: 1,
                        observed: 2
                    }
                )
            ))
        ));
        assert_eq!(transaction.staged_effect_digest().unwrap(), digest);
        assert_eq!(database.frontier().unwrap(), basis);
        assert!(transaction.vertices(&database).unwrap().is_empty());
        let (stats, result, receipt) = transaction
            .execute_graph_write_chain_governed(
                &mut database,
                &contexts.query(),
                &chain,
                policy(2, HIGH, HIGH).with_mutation_limit(6),
                |request| allocate(&next, request),
            )
            .unwrap();
        assert_eq!(receipt.stats().mutation_effects, 6);
        assert_eq!(stats.created_vertices, 4);
        assert_eq!(result.value.len(), 2);
        assert_eq!(
            result.value[0].values(),
            &[GraphValue::Scalar(CanonicalScalar::Int(111))]
        );
        assert_eq!(
            result.value[1].values(),
            &[GraphValue::Scalar(CanonicalScalar::Int(112))]
        );
        transaction
            .finish(&mut database, &contexts.commit())
            .await
            .unwrap();
        assert_eq!(database.frontier().unwrap().0, basis.0 + 1);
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn merge_effect_refusal_precedes_rhs_evaluation_and_empty_input_costs_nothing() {
    let ((), report) = run_async_under_lab(0xc4a1_1005, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let mut database = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        let mut transaction = database.begin(&contexts.txn()).unwrap();
        let digest = transaction.staged_effect_digest().unwrap();
        let next = AtomicU64::new(1_000);
        let failing = prepare(
            "UNWIND [1] AS k MERGE (a {p:k}) ON CREATE SET a.p=10/(k-1) \
             MERGE (b {p:2})",
        );
        let error = transaction
            .execute_graph_write_chain_governed(
                &mut database,
                &contexts.query(),
                &failing,
                policy(0, HIGH, HIGH).with_mutation_limit(0),
                |request| allocate(&next, request),
            )
            .unwrap_err();
        assert!(matches!(
            error,
            GqlQueryError::Source(fgdb_gql::GraphWriteChainError::Mutation(
                fgdb_gql::GraphMutationQueryError::Mutation(
                    fgdb_gql::GraphMutationError::EffectLimit {
                        limit: 0,
                        observed: 1
                    }
                )
            ))
        ));
        assert_eq!(transaction.staged_effect_digest().unwrap(), digest);
        let empty = prepare(
            "UNWIND [] AS k MERGE (a {p:k}) ON CREATE SET a.p=99 \
             MERGE (b {p:2}) ON CREATE SET b.p=2",
        );
        let (stats, result, receipt) = transaction
            .execute_graph_write_chain_governed(
                &mut database,
                &contexts.query(),
                &empty,
                policy(0, HIGH, HIGH).with_mutation_limit(0),
                |request| allocate(&next, request),
            )
            .unwrap();
        assert_eq!(receipt.stats().mutation_effects, 0);
        assert_eq!(stats.created_vertices, 0);
        assert!(result.value.is_empty());
        assert_eq!(transaction.staged_effect_digest().unwrap(), digest);
        transaction.abort();
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn post_write_matches_join_owned_occurrences_and_feed_complete_mutation_clauses() {
    let ((), report) = run_async_under_lab(0xc4a1_1009, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let mut database = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        let chain = prepare(
            "UNWIND [1,2] AS k CREATE (n {p:k,q:k*10}) \
             WITH n,n.p AS before SET n.p=n.p+10 \
             WITH n,before,n.p AS key MATCH (m {p:key}) \
             WITH n,m,before UNWIND [0,0] AS copy SET m.q=m.q+before \
             WITH n,before,m.q AS updated MATCH (n) REMOVE n.p \
             RETURN before,updated,n.p ORDER BY before",
        );
        let next = AtomicU64::new(1_000);
        let mut transaction = database.begin(&contexts.txn()).unwrap();
        let (stats, result, _) = transaction
            .execute_graph_write_chain_governed(
                &mut database,
                &contexts.query(),
                &chain,
                policy(4, HIGH, HIGH),
                |request| allocate(&next, request),
            )
            .unwrap();
        assert_eq!(stats.created_vertices, 2);
        assert_eq!(result.rows.result_rows, 4);
        assert_eq!(
            result
                .value
                .iter()
                .map(|row| row.values().to_vec())
                .collect::<Vec<_>>(),
            [1, 1, 2, 2]
                .into_iter()
                .map(|before| vec![
                    GraphValue::Scalar(CanonicalScalar::Int(before)),
                    GraphValue::Scalar(CanonicalScalar::Int(before * 11)),
                    GraphValue::Scalar(CanonicalScalar::Null),
                ])
                .collect::<Vec<_>>()
        );
        // The duplicated input occurrences return twice, but each SET reads
        // the whole clause's entry state: neither target is incremented twice.
        let vertices = transaction.vertices(&database).unwrap();
        assert_eq!(vertices.len(), 2);
        for (vertex, value) in vertices.iter().zip([11, 22]) {
            assert_eq!(vertex.props, vec![(Q, CanonicalScalar::Int(value).into())]);
        }
        transaction
            .finish(&mut database, &contexts.commit())
            .await
            .unwrap();
        assert_eq!(database.frontier().unwrap().0, 1);
        let durable = database.vertices().unwrap();
        assert_eq!(durable.len(), vertices.len());
        for (durable, staged) in durable.iter().zip(&vertices) {
            assert_eq!((durable.vid, &durable.props), (staged.vid, &staged.props));
        }
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn post_write_matches_preserve_edge_domains_empty_bags_and_parameter_bindings() {
    let ((), report) = run_async_under_lab(0xc4a1_100a, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let mut database = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        let next = AtomicU64::new(1_000);
        let chain = prepare(
            "CREATE (a {p:2}),(b {p:3}),(a)-[made:R {q:1}]->(b) \
             WITH a,b,made MATCH (a)-[r:R]->(b) SET r.q=r.q+a.p \
             WITH a,b,r.q AS updated MATCH (a)-[again:R]->(b) REMOVE again.q \
             RETURN updated,again.q,a.p",
        );
        let mut transaction = database.begin(&contexts.txn()).unwrap();
        let (stats, result, _) = transaction
            .execute_graph_write_chain_governed(
                &mut database,
                &contexts.query(),
                &chain,
                policy(1, HIGH, HIGH),
                |request| allocate(&next, request),
            )
            .unwrap();
        assert_eq!((stats.created_vertices, stats.created_edges), (2, 1));
        assert_eq!(result.value.len(), 1);
        assert_eq!(
            result.value[0].values(),
            &[
                GraphValue::Scalar(CanonicalScalar::Int(3)),
                GraphValue::Scalar(CanonicalScalar::Null),
                GraphValue::Scalar(CanonicalScalar::Int(2)),
            ]
        );
        let edges = transaction.edges(&database).unwrap();
        assert_eq!(edges.len(), 1);
        assert!(edges[0].props.is_empty());
        transaction
            .finish(&mut database, &contexts.commit())
            .await
            .unwrap();

        // The leading-row helper also serves ordinary reads. Keep its
        // identity and property columns through a later WITH/MATCH boundary.
        let read = database
            .query(
                &contexts.query(),
                "WITH [2] AS keys UNWIND keys AS key MATCH (a {p:key}) \
                 WITH a MATCH (b {p:3}) RETURN a.p AS ap,b.p AS bp",
                &GqlParameters::new(),
                symbols,
                policy(1, HIGH, HIGH).query,
            )
            .unwrap();
        assert_eq!(
            read,
            fgdb::QueryResult::Rows {
                columns: vec!["ap".into(), "bp".into()],
                rows: vec![vec![
                    fgdb_gql::GraphAggregateValue::Value(GraphValue::Scalar(CanonicalScalar::Int(
                        2
                    ),)),
                    fgdb_gql::GraphAggregateValue::Value(GraphValue::Scalar(CanonicalScalar::Int(
                        3
                    ),)),
                ]],
            }
        );

        // No matched occurrence means no later mutation or result, while
        // the earlier CREATE remains part of this successful statement.
        let empty = prepare(
            "CREATE (a {p:9}) WITH a MATCH (n {p:99}) \
             SET n.q=1/0 RETURN n.q",
        );
        let mut transaction = database.begin(&contexts.txn()).unwrap();
        let (stats, result, receipt) = transaction
            .execute_graph_write_chain_governed(
                &mut database,
                &contexts.query(),
                &empty,
                policy(0, HIGH, HIGH).with_mutation_limit(0),
                |request| allocate(&next, request),
            )
            .unwrap();
        assert_eq!(stats.created_vertices, 1);
        assert!(result.value.is_empty());
        assert_eq!(receipt.stats().mutation_effects, 0);
        transaction
            .finish(&mut database, &contexts.commit())
            .await
            .unwrap();

        let text = "CREATE (a {p:$seed}) WITH a MATCH (a) WHERE a.p=$wanted \
                    SET a.q=a.p+$bump RETURN a.q+$suffix AS value";
        let arguments = GqlParameters::new()
            .with_int64("seed", 7)
            .unwrap()
            .with_int64("wanted", 7)
            .unwrap()
            .with_int64("bump", 5)
            .unwrap()
            .with_int64("suffix", 3)
            .unwrap();
        let prepared = PreparedGraphWriteChainText::prepare(text, R, symbols).unwrap();
        let chain = prepared.bind_parameters(&arguments).unwrap();
        let mut transaction = database.begin(&contexts.txn()).unwrap();
        let (_, result, _) = transaction
            .execute_graph_write_chain_governed(
                &mut database,
                &contexts.query(),
                &chain,
                policy(1, HIGH, HIGH),
                |request| allocate(&next, request),
            )
            .unwrap();
        assert_eq!(result.value.len(), 1);
        assert_eq!(
            result.value[0].values(),
            &[GraphValue::Scalar(CanonicalScalar::Int(15))]
        );
        transaction
            .finish(&mut database, &contexts.commit())
            .await
            .unwrap();
        assert_eq!(database.frontier().unwrap().0, 3);
        assert_eq!(database.vertices().unwrap().len(), 4);

        for (text, expected) in [
            (
                "CREATE (a {p:1}),(b {p:7}) WITH a,b,a.p AS old SET a.p=3 \
                 WITH a,b,old MATCH (n {p:7}) SET n.q=a.p+old RETURN old,a.p,n.q",
                [Some(1), Some(3), Some(4)],
            ),
            (
                "CREATE (a {p:1}),(b {p:7}) WITH a,b,a.p AS old SET a.p=3 \
                 WITH a,b,old MATCH (n {p:7}) SET n.q=a.p+old \
                 WITH old,a.p AS current,n.q AS updated RETURN old,current,updated",
                [Some(1), Some(3), Some(4)],
            ),
            (
                "CREATE (a {p:1}),(b {p:9}) WITH a,a.p AS old \
                 MATCH (n {p:9}) SET a.p=7 RETURN old,a.p,n.p",
                [Some(1), Some(7), Some(9)],
            ),
            (
                "CREATE (a {p:1}),(b {p:9}) WITH a,a.p AS old \
                 MATCH (n {p:9}) REMOVE a.p RETURN old,a.p,n.p",
                [Some(1), None, Some(9)],
            ),
        ] {
            let chain = prepare(text);
            let mut database = Database::open_memory(&contexts.commit(), keys())
                .await
                .unwrap();
            let mut transaction = database.begin(&contexts.txn()).unwrap();
            let (_, result, _) = transaction
                .execute_graph_write_chain_governed(
                    &mut database,
                    &contexts.query(),
                    &chain,
                    policy(1, HIGH, HIGH),
                    |request| allocate(&next, request),
                )
                .unwrap();
            assert_eq!(result.value.len(), 1, "{text}");
            assert_eq!(
                result.value[0].values(),
                expected
                    .map(|value| GraphValue::Scalar(
                        value.map_or(CanonicalScalar::Null, CanonicalScalar::Int,)
                    ))
                    .as_slice(),
                "{text}"
            );
            transaction.abort();
        }

        // Plain SET/REMOVE are legal row aliases. Framing must leave their
        // WITH/RETURN in the terminal read compiler.
        for alias in ["set", "remove"] {
            let text = format!(
                "CREATE (a {{p:1}}) WITH a AS {alias} MATCH (n {{p:1}}) \
                 WITH {alias} RETURN {alias}.p AS value",
            );
            let chain = prepare(&text);
            let mut database = Database::open_memory(&contexts.commit(), keys())
                .await
                .unwrap();
            let mut transaction = database.begin(&contexts.txn()).unwrap();
            let (_, result, _) = transaction
                .execute_graph_write_chain_governed(
                    &mut database,
                    &contexts.query(),
                    &chain,
                    policy(1, HIGH, HIGH),
                    |request| allocate(&next, request),
                )
                .unwrap();
            assert_eq!(result.value.len(), 1, "{text}");
            assert_eq!(
                result.value[0].values(),
                &[GraphValue::Scalar(CanonicalScalar::Int(1))],
                "{text}"
            );
            transaction.abort();
        }
        // The existing identifier contract refuses delimited keywords in
        // alias positions; framing must not widen that separate admission.
        for alias in ["`set`", "`remove`"] {
            let text = format!(
                "CREATE (a {{p:1}}) WITH a AS {alias} MATCH (n {{p:1}}) \
                 WITH {alias} RETURN {alias}.p AS value",
            );
            assert!(PreparedGraphWriteChainText::prepare(&text, R, symbols).is_err());
        }
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn post_write_match_mutations_share_allowances_and_restore_a_prefix_on_late_failure() {
    let ((), report) = run_async_under_lab(0xc4a1_100b, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let mut database = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        let mut seed = WriteBatch::new(R);
        for id in [1, 2] {
            seed.create_vertex(VId(id), vec![], vec![(P, CanonicalScalar::Int(id as i64))]);
        }
        database.write(&contexts.commit(), seed).await.unwrap();
        let basis = database.frontier().unwrap();
        let next = AtomicU64::new(1_000);
        let chain = prepare(
            "CREATE (audit {p:100}) WITH audit MATCH (n) WHERE n.p<3 \
             SET n.q=n.p+10 RETURN n.q ORDER BY n.q",
        );
        let prefix = || {
            let mut prefix = WriteBatch::new(R);
            prefix.create_vertex(VId(7), vec![], vec![(P, CanonicalScalar::Int(99))]);
            prefix
        };
        let mut transaction = database.begin(&contexts.txn()).unwrap();
        transaction.write(&mut database, prefix()).unwrap();
        let (measured, expected, receipt) = transaction
            .execute_graph_write_chain_governed(
                &mut database,
                &contexts.query(),
                &chain,
                policy(2, HIGH, HIGH),
                |request| allocate(&next, request),
            )
            .unwrap();
        assert_eq!(expected.value.len(), 2);
        assert!(measured.selection.snapshot_records > 0);
        let effects = receipt.stats().mutation_effects;
        assert_eq!(effects, 2);
        transaction.abort();

        let exact = GraphInsertPolicy::new(
            GqlQueryPolicy::new(
                measured.selection.snapshot_records,
                2,
                measured.evaluator.work_units,
                measured.evaluator.scratch_entries,
            ),
            1,
            0,
        )
        .with_mutation_limit(effects);
        for refusal in 0..5 {
            let mut transaction = database.begin(&contexts.txn()).unwrap();
            transaction.write(&mut database, prefix()).unwrap();
            let digest = transaction.staged_effect_digest().unwrap();
            let mut tight = exact;
            match refusal {
                0 => {
                    tight.query.rows = fgdb_gql::GqlExecutionBudget::new(
                        measured.selection.snapshot_records - 1,
                        2,
                    )
                }
                1 => tight.query.evaluator.max_work_units -= 1,
                2 => tight.query.evaluator.max_scratch_entries -= 1,
                3 => tight = tight.with_mutation_limit(effects - 1),
                _ => {
                    tight.query.rows =
                        fgdb_gql::GqlExecutionBudget::new(measured.selection.snapshot_records, 1)
                }
            }
            transaction
                .execute_graph_write_chain_governed(
                    &mut database,
                    &contexts.query(),
                    &chain,
                    tight,
                    |request| allocate(&next, request),
                )
                .expect_err("the later MATCH/SET must share the original statement allowance");
            assert_eq!(transaction.staged_effect_digest().unwrap(), digest);
            assert_eq!(transaction.vertices(&database).unwrap().len(), 3);
            assert_eq!(database.frontier().unwrap(), basis);
            for id in [1, 2] {
                assert_eq!(
                    transaction
                        .vertex(&database, VId(id))
                        .unwrap()
                        .unwrap()
                        .props,
                    vec![(P, CanonicalScalar::Int(id as i64).into())]
                );
            }
            let (stats, actual, _) = transaction
                .execute_graph_write_chain_governed(
                    &mut database,
                    &contexts.query(),
                    &chain,
                    exact,
                    |request| allocate(&next, request),
                )
                .unwrap();
            assert_eq!(stats.evaluator, measured.evaluator);
            assert_eq!(stats.selection, measured.selection);
            assert_eq!(actual.value, expected.value);
            transaction.abort();
        }

        let failing = prepare(
            "CREATE (audit {p:100}) WITH audit MATCH (n) WHERE n.p<3 \
             SET n.q=100/(n.p-2) RETURN n.q LIMIT 0",
        );
        let mut transaction = database.begin(&contexts.txn()).unwrap();
        transaction.write(&mut database, prefix()).unwrap();
        let digest = transaction.staged_effect_digest().unwrap();
        let error = transaction
            .execute_graph_write_chain_governed(
                &mut database,
                &contexts.query(),
                &failing,
                policy(0, HIGH, HIGH),
                |request| allocate(&next, request),
            )
            .unwrap_err();
        assert!(matches!(
            error,
            GqlQueryError::Source(fgdb_gql::GraphWriteChainError::Mutation(
                fgdb_gql::GraphMutationQueryError::Mutation(
                    fgdb_gql::GraphMutationError::Arithmetic {
                        row: 1,
                        action: 0,
                        ..
                    }
                )
            ))
        ));
        assert_eq!(transaction.staged_effect_digest().unwrap(), digest);
        assert_eq!(transaction.vertices(&database).unwrap().len(), 3);
        transaction
            .finish(&mut database, &contexts.commit())
            .await
            .unwrap();
        assert_eq!(database.frontier().unwrap().0, basis.0 + 1);
        assert_eq!(database.vertices().unwrap().len(), 3);
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn post_write_match_deletes_preserve_occurrences_and_deleted_identities() {
    let ((), report) = run_async_under_lab(0xc4a1_100c, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        for (deletion, retired_vertex) in [
            ("DELETE r", false),
            ("DELETE a,r", true),
            ("DETACH DELETE a", true),
        ] {
            let mut database = Database::open_memory(&contexts.commit(), keys())
                .await
                .unwrap();
            let mut seed = WriteBatch::new(R);
            seed.create_vertex(VId(1), vec![], vec![(P, CanonicalScalar::Int(1))]);
            seed.create_vertex(VId(2), vec![], vec![(P, CanonicalScalar::Int(2))]);
            seed.add_edge(EId(11), VId(1), VId(2), vec![(P, CanonicalScalar::Int(5))]);
            database.write(&contexts.commit(), seed).await.unwrap();
            let chain = prepare(&format!(
                "CREATE (audit {{p:100}}) WITH audit MATCH (a {{p:1}})-[r:R]->(b) \
                 WITH audit,a,b,r,r.p AS before UNWIND [0,0] AS copy \
                 {deletion} RETURN before,a,r,b,audit.p,copy",
            ));
            let next = AtomicU64::new(1_000);
            let mut transaction = database.begin(&contexts.txn()).unwrap();
            let (stats, result, receipt) = transaction
                .execute_graph_write_chain_governed(
                    &mut database,
                    &contexts.query(),
                    &chain,
                    policy(2, HIGH, HIGH),
                    |request| allocate(&next, request),
                )
                .unwrap();
            assert_eq!(stats.created_vertices, 1, "{deletion}");
            assert_eq!(result.value.len(), 2, "{deletion}");
            for row in result.value {
                assert_eq!(
                    row.values(),
                    &[
                        GraphValue::Scalar(CanonicalScalar::Int(5)),
                        GraphValue::Vertex(VId(1)),
                        GraphValue::Edge(EId(11)),
                        GraphValue::Vertex(VId(2)),
                        GraphValue::Scalar(CanonicalScalar::Int(100)),
                        GraphValue::Scalar(CanonicalScalar::Int(0)),
                    ],
                    "{deletion}",
                );
            }
            assert_eq!(
                receipt.stats().mutation_effects,
                if deletion == "DELETE a,r" { 2 } else { 1 },
                "duplicate occurrences do not duplicate deletion effects: {deletion}",
            );
            assert_eq!(
                transaction.vertex(&database, VId(1)).unwrap().is_none(),
                retired_vertex,
                "{deletion}",
            );
            assert!(transaction.vertex(&database, VId(2)).unwrap().is_some());
            assert!(transaction.edges(&database).unwrap().is_empty());
            transaction
                .finish(&mut database, &contexts.commit())
                .await
                .unwrap();
            assert_eq!(database.frontier().unwrap().0, 2);
            assert_eq!(
                database.vertices().unwrap().len(),
                if retired_vertex { 2 } else { 3 }
            );
            assert!(database.edges().unwrap().is_empty());
        }
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn post_write_match_deletes_read_staged_graph_and_feed_later_writes() {
    let ((), report) = run_async_under_lab(0xc4a1_100d, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let mut database = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        let next = AtomicU64::new(1_000);
        // Both the relationship selected for deletion and its endpoints were
        // created earlier in this same statement. The later MATCH must see
        // the deleted relationship absent before it creates the replacement.
        let chain = prepare(
            "CREATE (a {p:1}),(b {p:2}),(a)-[old:R {p:7}]->(b) \
             WITH a,b MATCH (a)-[r:R]->(b) WITH a,b,r,r.p AS before DELETE r \
             WITH a,b,before MATCH (a) CREATE (a)-[replacement:R {p:before+1}]->(b) \
             RETURN replacement.p,a.p,b.p",
        );
        let mut transaction = database.begin(&contexts.txn()).unwrap();
        let (stats, result, _) = transaction
            .execute_graph_write_chain_governed(
                &mut database,
                &contexts.query(),
                &chain,
                policy(1, HIGH, HIGH),
                |request| allocate(&next, request),
            )
            .unwrap();
        assert_eq!((stats.created_vertices, stats.created_edges), (2, 2));
        assert_eq!(result.value.len(), 1);
        assert_eq!(
            result.value[0].values(),
            &[
                GraphValue::Scalar(CanonicalScalar::Int(8)),
                GraphValue::Scalar(CanonicalScalar::Int(1)),
                GraphValue::Scalar(CanonicalScalar::Int(2)),
            ],
        );
        let edges = transaction.edges(&database).unwrap();
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].props, vec![(P, CanonicalScalar::Int(8).into())]);
        transaction
            .finish(&mut database, &contexts.commit())
            .await
            .unwrap();
        assert_eq!(database.edges().unwrap().len(), 1);

        // A computed target uses the existing typed mutation expression;
        // selection finishes before any target is retired.
        let computed = prepare(
            "CREATE (audit {p:100}) WITH audit MATCH (n {p:2}) \
             WITH audit,n.p AS before,[n] AS targets DETACH DELETE targets[0] \
             RETURN before,audit.p",
        );
        let mut transaction = database.begin(&contexts.txn()).unwrap();
        let (_, result, _) = transaction
            .execute_graph_write_chain_governed(
                &mut database,
                &contexts.query(),
                &computed,
                policy(1, HIGH, HIGH),
                |request| allocate(&next, request),
            )
            .unwrap();
        assert_eq!(
            result.value[0].values(),
            &[
                GraphValue::Scalar(CanonicalScalar::Int(2)),
                GraphValue::Scalar(CanonicalScalar::Int(100)),
            ],
        );
        assert_eq!(transaction.vertices(&database).unwrap().len(), 2);
        assert!(transaction.edges(&database).unwrap().is_empty());
        transaction
            .finish(&mut database, &contexts.commit())
            .await
            .unwrap();

        let empty = prepare(
            "CREATE (audit {p:200}) WITH audit MATCH (n {p:99}) \
             DETACH DELETE n RETURN n",
        );
        let mut transaction = database.begin(&contexts.txn()).unwrap();
        let (stats, result, receipt) = transaction
            .execute_graph_write_chain_governed(
                &mut database,
                &contexts.query(),
                &empty,
                policy(0, HIGH, HIGH).with_mutation_limit(0),
                |request| allocate(&next, request),
            )
            .unwrap();
        assert_eq!(stats.created_vertices, 1);
        assert_eq!(receipt.stats().mutation_effects, 0);
        assert!(result.value.is_empty());
        transaction
            .finish(&mut database, &contexts.commit())
            .await
            .unwrap();
        assert_eq!(database.frontier().unwrap().0, 3);
        assert_eq!(database.vertices().unwrap().len(), 3);
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn post_write_match_delete_failures_restore_the_existing_transaction_prefix() {
    let ((), report) = run_async_under_lab(0xc4a1_100e, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let mut database = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(1), vec![], vec![(P, CanonicalScalar::Int(1))]);
        seed.create_vertex(VId(2), vec![], vec![(P, CanonicalScalar::Int(2))]);
        seed.add_edge(EId(11), VId(1), VId(2), vec![(P, CanonicalScalar::Int(5))]);
        database.write(&contexts.commit(), seed).await.unwrap();
        let basis = database.frontier().unwrap();
        let next = AtomicU64::new(1_000);
        let mut transaction = database.begin(&contexts.txn()).unwrap();
        let mut prefix = WriteBatch::new(R);
        prefix.create_vertex(VId(7), vec![], vec![(P, CanonicalScalar::Int(99))]);
        transaction.write(&mut database, prefix).unwrap();
        let digest = transaction.staged_effect_digest().unwrap();
        for (text, incident) in [
            (
                "CREATE (audit {p:100}) WITH audit MATCH (n {p:1}) DELETE n RETURN n",
                true,
            ),
            (
                "CREATE (audit {p:100}) WITH audit MATCH (n {p:1}) \
                 DETACH DELETE n RETURN n.p LIMIT 0",
                false,
            ),
            (
                "MATCH (victim {p:1}) CREATE (audit {p:100}) WITH victim,audit \
                 MATCH (n {p:2}) DETACH DELETE victim RETURN victim.p LIMIT 0",
                false,
            ),
            (
                "CREATE (audit {p:100}) WITH audit MATCH (a {p:1})-[r:R]->(b) \
                 DELETE r RETURN r.p LIMIT 0",
                false,
            ),
        ] {
            let chain = prepare(text);
            let error = transaction
                .execute_graph_write_chain_governed(
                    &mut database,
                    &contexts.query(),
                    &chain,
                    policy(1, HIGH, HIGH),
                    |request| allocate(&next, request),
                )
                .unwrap_err();
            if incident {
                assert!(
                    matches!(
                        error,
                        GqlQueryError::Source(fgdb_gql::GraphWriteChainError::Mutation(
                            fgdb_gql::GraphMutationQueryError::Mutation(
                                fgdb_gql::GraphMutationError::IncidentRelationships
                            )
                        ))
                    ),
                    "{text}: {error:?}"
                );
            } else {
                assert!(
                    matches!(
                        error,
                        GqlQueryError::Source(fgdb_gql::GraphWriteChainError::Mutation(
                            fgdb_gql::GraphMutationQueryError::Mutation(
                                fgdb_gql::GraphMutationError::DeletedElementRead { .. }
                            )
                        ))
                    ),
                    "{text}: {error:?}"
                );
            }
            assert_eq!(
                transaction.staged_effect_digest().unwrap(),
                digest,
                "{text}"
            );
            assert_eq!(transaction.vertices(&database).unwrap().len(), 3, "{text}");
            assert_eq!(transaction.edges(&database).unwrap().len(), 1, "{text}");
            assert_eq!(database.frontier().unwrap(), basis);
        }
        // Retrying through the same transaction retains only the successful
        // allocation, alongside its earlier staged prefix.
        let retry = prepare(
            "CREATE (audit {p:100}) WITH audit MATCH (n {p:1}) \
             WITH audit,n,n.p AS before DETACH DELETE n RETURN n,before,audit.p",
        );
        let (_, result, _) = transaction
            .execute_graph_write_chain_governed(
                &mut database,
                &contexts.query(),
                &retry,
                policy(1, HIGH, HIGH),
                |request| allocate(&next, request),
            )
            .unwrap();
        assert_eq!(
            result.value[0].values(),
            &[
                GraphValue::Vertex(VId(1)),
                GraphValue::Scalar(CanonicalScalar::Int(1)),
                GraphValue::Scalar(CanonicalScalar::Int(100)),
            ],
        );
        assert_eq!(transaction.vertices(&database).unwrap().len(), 3);
        assert!(transaction.edges(&database).unwrap().is_empty());
        transaction
            .finish(&mut database, &contexts.commit())
            .await
            .unwrap();
        assert_eq!(database.frontier().unwrap().0, basis.0 + 1);
        assert!(database.vertex(VId(1)).unwrap().is_none());
        assert!(database.vertex(VId(7)).unwrap().is_some());
        assert_eq!(database.vertices().unwrap().len(), 3);
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn deletion_keywords_stay_legal_row_aliases_between_writes() {
    let ((), report) = run_async_under_lab(0xc4a1_100f, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        for alias in ["delete", "detach"] {
            for tail in [
                format!("WITH {alias} ORDER BY {alias}.p RETURN {alias}.p"),
                format!("WITH {alias},n DELETE n RETURN {alias}.p"),
            ] {
                let mut database = Database::open_memory(&contexts.commit(), keys())
                    .await
                    .unwrap();
                let text = format!(
                    "CREATE (a {{p:1}}),(b {{p:2}}) WITH a AS {alias} \
                     MATCH (n {{p:2}}) {tail}",
                );
                let chain = prepare(&text);
                let next = AtomicU64::new(1_000);
                let mut transaction = database.begin(&contexts.txn()).unwrap();
                let (_, result, _) = transaction
                    .execute_graph_write_chain_governed(
                        &mut database,
                        &contexts.query(),
                        &chain,
                        policy(1, HIGH, HIGH),
                        |request| allocate(&next, request),
                    )
                    .unwrap();
                assert_eq!(result.value.len(), 1, "{text}");
                assert_eq!(
                    result.value[0].values(),
                    &[GraphValue::Scalar(CanonicalScalar::Int(1))],
                    "{text}",
                );
                assert_eq!(
                    transaction.vertices(&database).unwrap().len(),
                    if tail.contains("DELETE n") { 1 } else { 2 },
                    "{text}",
                );
                transaction.abort();
            }
        }
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
