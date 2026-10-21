//! Input occurrences and matched vertices of a chained MERGE are private.
//! Final row admission happens after the native RETURN operators, never before
//! the writes. Independent expected effects distinguish a true fix from paging
//! or aggregating an already-truncated match bag.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, WriteBatch};
use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::insertion::{GraphInsertPolicy, GraphInsertRequest};
use fgdb_gql::{
    GlaLimitDimension, GqlBudgetDimension, GqlParameters, GqlQueryError, GqlQueryPolicy,
    GraphSymbol, GraphSymbolKind, GraphVertexMergeRowsQueryError, GraphWriteChainError,
    PreparedGraphWriteChain, PreparedGraphWriteChainText,
};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, PurposeContexts, VId};
use std::sync::atomic::{AtomicU64, Ordering};

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);
const WIDE: VId = VId((1_u128 << 96) | 1);
const HIGH: u64 = 10_000_000;
const ACTIONS: &str = "ON CREATE SET n.q=10 ON MATCH SET n.q=n.q+1";

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x41; 32],
        DatabaseSecurityNamespaceId([0x42; 32]),
        [0x43; 32],
    )
}

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "Seed") => Some(GraphSymbol::Label(LabelId(1))),
        (GraphSymbolKind::Label, "Target") => Some(GraphSymbol::Label(LabelId(2))),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
        _ => None,
    }
}

fn int(value: i64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(value))
}

fn policy(results: u64) -> GraphInsertPolicy {
    GraphInsertPolicy::new(GqlQueryPolicy::new(HIGH, results, HIGH, HIGH), 100, 100)
}

fn prepare(text: &str) -> PreparedGraphWriteChain {
    assert!(PreparedGraphWriteChainText::is_chain(text).unwrap(), "{text}");
    PreparedGraphWriteChainText::prepare(text, R, symbols)
        .expect(text)
        .bind_parameters(&GqlParameters::new())
        .expect(text)
}

fn seed() -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    for (id, key) in [(101, 1), (102, 1), (103, 2)] {
        batch.create_vertex(VId(id), vec![LabelId(1)], vec![(P, CanonicalScalar::Int(key))]);
    }
    for (id, key, value) in [(VId(1), 1, 4), (WIDE, 1, 7), (VId(9), 9, 99)] {
        batch.create_vertex(
            id,
            vec![LabelId(2)],
            vec![(P, CanonicalScalar::Int(key)), (Q, CanonicalScalar::Int(value))],
        );
    }
    batch
}

fn allocate(next: &AtomicU64, request: GraphInsertRequest) -> Result<ElementId, ()> {
    assert!(matches!(request, GraphInsertRequest::Vertex { .. }));
    Ok(ElementId::Vertex(VId(u128::from(next.fetch_add(1, Ordering::Relaxed)))))
}

fn assert_targets<V: asupersync::fs::Vfs + Clone>(db: &Database<V>, actions: bool, created: VId) {
    for (id, value) in [(VId(1), if actions { 6 } else { 4 }), (WIDE, if actions { 9 } else { 7 })] {
        assert_eq!(
            db.vertex(id).unwrap().unwrap().props,
            vec![(P, CanonicalScalar::Int(1)), (Q, CanonicalScalar::Int(value))],
        );
    }
    assert_eq!(
        db.vertex(VId(9)).unwrap().unwrap().props,
        vec![(P, CanonicalScalar::Int(9)), (Q, CanonicalScalar::Int(99))],
    );
    let expected = if actions {
        vec![(P, CanonicalScalar::Int(2)), (Q, CanonicalScalar::Int(10))]
    } else {
        vec![(P, CanonicalScalar::Int(2))]
    };
    assert_eq!(db.vertex(created).unwrap().unwrap().props, expected);
}

#[test]
fn final_groups_and_pages_do_not_limit_match_unwind_or_owned_inputs() {
    let ((), report) = run_async_under_lab(0xc4a2_1001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        for (prefix, key, extra_creations) in [
            ("MATCH (s:Seed)", "s.p", 0),
            ("UNWIND [1,1,2] AS k", "k", 0),
            ("UNWIND [1,1,2] AS k CREATE (s:Seed {p:k}) WITH k", "k", 3),
        ] {
            for actions in [false, true] {
                for (tail, limit, expected) in [
                    ("RETURN count(n) AS total", 1, vec![vec![int(5)]]),
                    ("RETURN DISTINCT n.p AS p ORDER BY p SKIP 1 LIMIT 1", 1, vec![vec![int(2)]]),
                    ("RETURN n.p AS p LIMIT 0", 0, vec![]),
                ] {
                    let vfs = MemVfs::new().unwrap();
                    let path = vfs.database_dir();
                    let mut db = Database::create_with_vfs(&contexts.commit(), vfs.clone(), &path, keys())
                        .await
                        .unwrap();
                    db.write(&contexts.commit(), seed()).await.unwrap();
                    let before = db.frontier().unwrap();
                    let text = format!(
                        "{prefix} MERGE (n:Target {{p:{key}}}) {} {tail}",
                        if actions { ACTIONS } else { "" },
                    );
                    let chain = prepare(&text);
                    let next = AtomicU64::new(1_000);
                    let mut transaction = db.begin(&contexts.txn()).unwrap();
                    let (stats, rows, receipt) = transaction
                        .execute_graph_write_chain_governed(
                            &mut db, &contexts.query(), &chain, policy(limit),
                            |request| allocate(&next, request),
                        )
                        .unwrap_or_else(|error| panic!("{text}: {error:?}"));
                    let actual: Vec<_> = rows.value.iter().map(|row| row.values().to_vec()).collect();
                    assert_eq!(actual, expected, "{text}");
                    assert_eq!(stats.selection.result_rows, expected.len() as u64);
                    assert_eq!(receipt.stats().selection.result_rows, expected.len() as u64);
                    assert_eq!(stats.created_vertices, 1 + extra_creations);
                    assert_eq!(next.load(Ordering::Relaxed), 1_001 + extra_creations);
                    assert_eq!(db.frontier().unwrap(), before);
                    transaction.finish(&mut db, &contexts.commit()).await.unwrap();
                    assert_eq!(db.frontier().unwrap().0, before.0 + 1);
                    assert_eq!(db.vertices().unwrap().len() as u64, 7 + extra_creations);
                    let created = VId(1_000 + u128::from(extra_creations));
                    assert_targets(&db, actions, created);
                    db.compact(&contexts.commit()).await.unwrap();
                    drop(db);
                    let db = Database::open_with_vfs(&contexts.commit(), vfs, &path, keys())
                        .await
                        .unwrap();
                    assert_targets(&db, actions, created);
                    assert_eq!(db.frontier().unwrap().0, before.0 + 1);
                    assert_eq!(contexts.txn().outstanding_obligations(), 0);
                }
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn final_row_refusal_and_limit_zero_data_error_preserve_the_prior_prefix() {
    let ((), report) = run_async_under_lab(0xc4a2_1002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let mut db = Database::open_memory(&contexts.commit(), keys()).await.unwrap();
        db.write(&contexts.commit(), seed()).await.unwrap();
        let before = db.frontier().unwrap();
        let mut transaction = db.begin(&contexts.txn()).unwrap();
        let mut prefix = WriteBatch::new(R);
        prefix.create_vertex(VId(500), vec![], vec![]);
        transaction.write(&mut db, prefix).unwrap();
        let digest = transaction.staged_effect_digest().unwrap();
        let next = AtomicU64::new(1_000);
        for (tail, limit) in [
            ("RETURN n.p AS p", 4),
            ("RETURN 1/(n.q-10) AS bad LIMIT 0", 0),
        ] {
            let text = format!("MATCH (s:Seed) MERGE (n:Target {{p:s.p}}) {ACTIONS} {tail}");
            let error = transaction
                .execute_graph_write_chain_governed(
                    &mut db, &contexts.query(), &prepare(&text), policy(limit),
                    |request| allocate(&next, request),
                )
                .expect_err("the final projection must refuse after all five occurrences");
            if limit == 4 {
                assert!(matches!(error, GqlQueryError::Rows(error)
                    if error.dimension == GqlBudgetDimension::ResultRows && error.observed == 5));
            } else {
                assert!(matches!(error,
                    GqlQueryError::Source(GraphWriteChainError::VertexMerge(
                        GraphVertexMergeRowsQueryError::Returning(_)
                    ))
                ));
            }
            assert_eq!(transaction.staged_effect_digest().unwrap(), digest);
            assert_eq!(transaction.vertices(&db).unwrap().len(), 7);
            assert_eq!(db.frontier().unwrap(), before);
        }
        assert_eq!(next.load(Ordering::Relaxed), 1_002, "failed creations spend IDs");
        let accepted = prepare(&format!(
            "MATCH (s:Seed) MERGE (n:Target {{p:s.p}}) {ACTIONS} \
             RETURN count(n) AS total,sum(n.q) AS value"
        ));
        let (_, result, _) = transaction
            .execute_graph_write_chain_governed(
                &mut db, &contexts.query(), &accepted, policy(1),
                |request| allocate(&next, request),
            )
            .unwrap();
        assert_eq!(result.value.len(), 1);
        assert_eq!(result.value[0].values(), &[int(5), int(40)]);
        transaction.finish(&mut db, &contexts.commit()).await.unwrap();
        assert_eq!(db.frontier().unwrap().0, before.0 + 1);
        assert!(db.vertex(VId(500)).unwrap().is_some());
        assert!(db.vertex(VId(1_000)).unwrap().is_none());
        assert!(db.vertex(VId(1_001)).unwrap().is_none());
        assert!(db.vertex(VId(1_002)).unwrap().is_some());
        assert_targets(&db, true, VId(1_002));
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn private_occurrences_still_spend_snapshot_work_scratch_and_creation_limits() {
    let ((), report) = run_async_under_lab(0xc4a2_1003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let query = prepare(&format!(
            "MATCH (s:Seed) MERGE (n:Target {{p:s.p}}) {ACTIONS} RETURN count(n) AS total"
        ));
        let mut db = Database::open_memory(&contexts.commit(), keys()).await.unwrap();
        db.write(&contexts.commit(), seed()).await.unwrap();
        let before = db.frontier().unwrap();
        let mut transaction = db.begin(&contexts.txn()).unwrap();
        let mut prefix = WriteBatch::new(R);
        prefix.create_vertex(VId(500), vec![], vec![]);
        transaction.write(&mut db, prefix).unwrap();
        let digest = transaction.staged_effect_digest().unwrap();
        let next = AtomicU64::new(1_000);
        for dimension in 0..4 {
            let mut budget = policy(1);
            match dimension {
                0 => budget.query.rows = fgdb_gql::GqlExecutionBudget::new(0, 1),
                1 => budget.query.evaluator.max_work_units = 0,
                2 => budget.query.evaluator.max_scratch_entries = 0,
                _ => budget.max_vertices = 0,
            }
            let error = transaction
                .execute_graph_write_chain_governed(
                    &mut db, &contexts.query(), &query, budget,
                    |request| allocate(&next, request),
                )
                .expect_err("only final-result admission moves out of the private input");
            match dimension {
                0 => assert!(matches!(error, GqlQueryError::Rows(error)
                    if error.dimension == GqlBudgetDimension::SnapshotRecords)),
                1 => assert!(matches!(error, GqlQueryError::Evaluator(error)
                    if error.dimension == GlaLimitDimension::WorkUnits)),
                2 => assert!(matches!(error, GqlQueryError::Evaluator(error)
                    if error.dimension == GlaLimitDimension::ScratchEntries)),
                _ => assert!(matches!(error, GqlQueryError::Source(_))),
            }
            assert_eq!(transaction.staged_effect_digest().unwrap(), digest);
            assert_eq!(db.frontier().unwrap(), before);
            assert_eq!(next.load(Ordering::Relaxed), 1_000);
        }
        let (_, result, _) = transaction
            .execute_graph_write_chain_governed(
                &mut db, &contexts.query(), &query, policy(1),
                |request| allocate(&next, request),
            )
            .unwrap();
        assert_eq!(result.value[0].values(), &[int(5)]);
        transaction.finish(&mut db, &contexts.commit()).await.unwrap();
        assert_targets(&db, true, VId(1_000));
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
