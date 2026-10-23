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
