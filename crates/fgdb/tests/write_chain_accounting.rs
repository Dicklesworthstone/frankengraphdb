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
const HIGH: u64 = 10_000_000;
const GROUPED: &str = "UNWIND [1,2,3] AS x CREATE (a {p:x}) \
    WITH a.p AS p,size(range(1,32)) AS n CREATE (b {p:p+n}) \
    RETURN sum(b.p) AS total";

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
        _ => None,
    }
}

fn policy(rows: u64, work: u64, scratch: u64) -> GraphInsertPolicy {
    GraphInsertPolicy::new(GqlQueryPolicy::new(10_000, rows, work, scratch), 1_000, 1_000)
}

fn prepare(text: &str) -> PreparedGraphWriteChain {
    assert!(PreparedGraphWriteChainText::is_chain(text).unwrap(), "{text}");
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
            let mut database = Database::open_memory(&contexts.commit(), keys()).await.unwrap();
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
            transaction.finish(&mut database, &contexts.commit()).await.unwrap();
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
            for dimension in [GlaLimitDimension::WorkUnits, GlaLimitDimension::ScratchEntries] {
                let mut database = Database::open_memory(&contexts.commit(), keys()).await.unwrap();
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
                assert!(matches!(error, GqlQueryError::Evaluator(limit) if limit.dimension == dimension));
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
                transaction.finish(&mut database, &contexts.commit()).await.unwrap();
                assert_eq!(database.frontier().unwrap().0, basis.0 + 1);
                assert_eq!(database.vertices().unwrap().len() as u64, expected.vertices + 1);
                assert_eq!(contexts.txn().outstanding_obligations(), 0);
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
