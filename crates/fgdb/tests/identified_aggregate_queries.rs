//! Captured path aggregation over authoritative snapshots and canonical overlays.
use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, GqlError, MemVfs, ReadError, WriteBatch, WriteError, WriteTxnError};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GlaDirection, GraphColumn, GraphPathFunction, GraphPatternBuilder,
    GraphValueRow, IntegerComparison, PreparedGraphPattern, VertexPredicate};
use fgdb_gql::{GqlParameters, GqlQueryError, GqlQueryPolicy, GraphAggregate,
    GraphAggregateError, GraphAggregateRow, GraphIntegerBinary, GraphIntegerErrorKind,
    GraphIntegerExpression, GraphIntegerOp, GraphSetProjection, GraphSetValue,
    GraphWalkBounds, PreparedGraphAggregate, PreparedGraphAggregateText};
use fgdb_types::{CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId,
    EId, EmbeddedTxnCompletion, PurposeContexts, VId};

const P: PropertyKeyId = PropertyKeyId(1);
const R: RelationId = RelationId(1);
fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0x31; 32], DatabaseSecurityNamespaceId([0x32; 32]), [0x33; 32])
}
fn policy() -> GqlQueryPolicy { GqlQueryPolicy::new(100_000, 100_000, 5_000_000, 5_000_000) }
async fn seed(db: &mut Database<MemVfs>, cx: &CommitCx) -> CommitSeq {
    let mut batch = WriteBatch::new(R);
    for id in 1_u128..=3 {
        batch.create_vertex(VId(id), vec![], vec![(P, CanonicalScalar::Int(id as i64))]);
    }
    for (eid, from, to) in [(11, 1, 2), (12, 1, 2), (13, 2, 3), (14, 1, 3)] {
        batch.add_edge(EId(eid), VId(from), VId(to), vec![]);
    }
    db.write(cx, batch).await.unwrap()
}
fn input() -> PreparedGraphPattern<GraphValueRow> {
    let mut b = GraphPatternBuilder::new();
    b.vertex("a").unwrap().vertex("b").unwrap();
    b.walk("a", R, GlaDirection::Forward, "b", GraphWalkBounds::new(1, 2).unwrap()).unwrap();
    b.filter("a", VertexPredicate::IntegerProperty {
        key: P, comparison: IntegerComparison::Equal, value: 1,
    }).unwrap();
    b.capture_path("route").unwrap();
    b.prepare_values(&[
        GraphColumn::path("route", "route", GraphPathFunction::Value),
        GraphColumn::path("length", "route", GraphPathFunction::Length),
        GraphColumn::property("value", "b", P),
    ], 0, None).unwrap().with_duplicates()
}
fn aggregate(relational: bool) -> PreparedGraphAggregate {
    let declarations = [GraphAggregate::count_rows("occurrences"), GraphAggregate::count_distinct("routes", 0),
        GraphAggregate::sum_int("hops", 1), GraphAggregate::sum_int("values", 2)];
    if relational {
        PreparedGraphAggregate::prepare_relation(input().into(), &[], &declarations, 0, None).unwrap()
    } else { PreparedGraphAggregate::prepare(input(), &[], &declarations, 0, None).unwrap() }
}
fn numbers(rows: &[GraphAggregateRow]) -> (u64, u64, i128, i128) {
    assert_eq!(rows.len(), 1);
    let row = rows[0].values();
    (row[0].as_count().unwrap(), row[1].as_count().unwrap(),
        row[2].as_integer().unwrap(), row[3].as_integer().unwrap())
}

#[test]
fn real_eids_survive_live_historical_pinned_and_staged_aggregate_reads() {
    let ((), report) = run_async_under_lab(0x31a6_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit(); let cx = contexts.query(); let txcx = contexts.txn();
        let vfs = MemVfs::new().unwrap(); let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys()).await.unwrap();
        let basis = seed(&mut db, &commit).await;
        let pinned = db.read_session().unwrap();
        let mut txn = db.begin(&txcx).unwrap();
        // Raw routes: 11,12,14,11/13,12/13. Endpoints are 2,2,3,3,3.
        for relational in [false, true] {
            let plan = aggregate(relational);
            for result in [
                db.execute_graph_aggregate_governed(&cx, &plan, policy()).unwrap(),
                db.execute_graph_aggregate_governed_at(&cx, &plan, basis, policy()).unwrap(),
                pinned.execute_graph_aggregate_governed(&cx, &plan, policy()).unwrap(),
                pinned.execute_graph_aggregate_governed_at(&cx, &plan, basis, policy()).unwrap(),
                txn.execute_graph_aggregate_governed(&db, &cx, &plan, policy()).unwrap(),
            ] { assert_eq!(numbers(&result.value), (5, 5, 7, 13)); }
        }
        let mut changes = WriteBatch::new(R);
        changes.delete_edge(EId(12));
        changes.add_edge(EId(15), VId(2), VId(1), vec![]);
        txn.write(&mut db, changes).unwrap();
        // New routes: 11,14,11/13,11/15; deleting one parallel EId must
        // not remove the other, and a closed WALK remains a real route.
        for relational in [false, true] {
            let plan = aggregate(relational);
            assert_eq!(numbers(&txn.execute_graph_aggregate_governed(&db, &cx, &plan, policy()).unwrap().value), (4, 4, 6, 9));
            assert_eq!(numbers(&db.execute_graph_aggregate_governed(&cx, &plan, policy()).unwrap().value), (5, 5, 7, 13));
        }
        let next = txn.commit(&mut db, &commit).await.unwrap();
        assert_eq!(next.0, basis.0 + 1);
        db.compact(&commit).await.unwrap(); drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &path, keys()).await.unwrap();
        for relational in [false, true] {
            let plan = aggregate(relational);
            assert_eq!(numbers(&db.execute_graph_aggregate_governed(&cx, &plan, policy()).unwrap().value), (4, 4, 6, 9));
            assert_eq!(numbers(&db.execute_graph_aggregate_governed_at(&cx, &plan, basis, policy()).unwrap().value), (5, 5, 7, 13));
            assert_eq!(numbers(&pinned.execute_graph_aggregate_governed(&cx, &plan, policy()).unwrap().value), (5, 5, 7, 13));
        }
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn input_pipeline_pages_preserve_owned_captured_keys_across_reopen() {
    let ((), report) = run_async_under_lab(0x31a6_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit(); let cx = contexts.query();
        let vfs = MemVfs::new().unwrap(); let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys()).await.unwrap();
        let basis = seed(&mut db, &commit).await;
        let relation = fgdb_gql::PreparedGraphSet::from(input()).with_page(1, Some(2));
        let plan = PreparedGraphAggregate::prepare_relation(relation, &[0], &[GraphAggregate::count_rows("n")], 0, None).unwrap();
        let old = db.execute_graph_aggregate_governed(&cx, &plan, policy()).unwrap().value;
        let paths: Vec<_> = old.iter().map(|row| row.keys()[0].as_path().unwrap().steps().to_vec()).collect();
        assert_eq!(paths, vec![vec![(EId(11), VId(2)), (EId(13), VId(3))], vec![(EId(12), VId(2))]]);
        db.compact(&commit).await.unwrap(); drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &path, keys()).await.unwrap();
        assert_eq!(db.execute_graph_aggregate_governed_at(&cx, &plan, basis, policy()).unwrap().value, old);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

fn bad_projection() -> PreparedGraphAggregate {
    let expression = GraphIntegerExpression::prepare(&[
        GraphIntegerOp::Literal(Some(1)), GraphIntegerOp::Column(2), GraphIntegerOp::Literal(Some(3)),
        GraphIntegerOp::Binary(GraphIntegerBinary::Subtract), GraphIntegerOp::Binary(GraphIntegerBinary::Divide),
    ]).unwrap();
    PreparedGraphAggregate::prepare_projected(input(),
        vec![GraphSetProjection::new("bad", GraphSetValue::Integer(expression))], &[],
        &[GraphAggregate::count_rows("n")], 0, Some(0)).unwrap()
}

#[test]
fn hidden_or_failed_path_summaries_retain_parallel_edge_phantom_observations() {
    let ((), report) = run_async_under_lab(0x31a6_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit(); let cx = contexts.query(); let txcx = contexts.txn();
        for mode in 0..3 {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            seed(&mut db, &commit).await;
            let mut txn = db.begin(&txcx).unwrap();
            let mut prefix = WriteBatch::new(R);
            prefix.create_vertex(VId(99), vec![], vec![]);
            txn.write(&mut db, prefix).unwrap();
            let before = txn.staged_effect_digest().unwrap();
            let plan = match mode {
                1 => bad_projection(),
                2 => PreparedGraphAggregate::prepare(input(), &[], &[GraphAggregate::count_rows("n")], 0, Some(0)).unwrap(),
                _ => aggregate(false),
            };
            let result = txn.execute_graph_aggregate_governed(&db, &cx, &plan, policy());
            if mode == 1 {
                assert!(matches!(result, Err(GqlQueryError::Source(GraphAggregateError::InputExpression { error, .. }))
                    if error.kind == GraphIntegerErrorKind::DivisionByZero));
            } else if mode == 2 { assert!(result.unwrap().value.is_empty()); }
            else { assert_eq!(numbers(&result.unwrap().value), (5, 5, 7, 13)); }
            assert_eq!(txn.staged_effect_digest().unwrap(), before);
            let mut winner = WriteBatch::new(R);
            winner.add_edge(EId(16), VId(1), VId(2), vec![]);
            db.write(&commit, winner).await.unwrap();
            // No second transaction read is allowed to repair a missing query
            // observation. A new parallel edge changes route identity/count.
            assert!(matches!(txn.commit(&mut db, &commit).await,
                Err(WriteTxnError::Write(WriteError::FirstCommitterWins { .. }))));
            assert!(db.vertex(VId(99)).unwrap().is_none());
        }
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn failed_path_aggregate_preserves_an_outer_write_that_can_still_commit() {
    let ((), report) = run_async_under_lab(0x31a6_0004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit(); let cx = contexts.query(); let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let basis = seed(&mut db, &commit).await;
        let mut txn = db.begin(&txcx).unwrap();
        let mut prefix = WriteBatch::new(R);
        prefix.create_vertex(VId(99), vec![], vec![]);
        txn.write(&mut db, prefix).unwrap();
        let before = txn.staged_effect_digest().unwrap();
        assert!(matches!(txn.execute_graph_aggregate_governed(&db, &cx, &bad_projection(), policy()),
            Err(GqlQueryError::Source(GraphAggregateError::InputExpression { .. }))));
        assert_eq!(txn.staged_effect_digest().unwrap(), before);
        assert_eq!(db.frontier().unwrap(), basis);
        assert!(matches!(txn.finish(&mut db, &commit).await.unwrap(), EmbeddedTxnCompletion::WriteCommitted { .. }));
        assert!(db.vertex(VId(99)).unwrap().is_some());
        assert_eq!(db.frontier().unwrap().0, basis.0 + 1);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn source_route_and_group_costs_share_exact_limits_and_preflight_precedence() {
    let ((), report) = run_async_under_lab(0x31a6_0005, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit(); let cx = contexts.query(); let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let other = Database::open_memory(&commit, keys()).await.unwrap();
        let basis = seed(&mut db, &commit).await;
        let pinned = db.read_session().unwrap();
        let mut txn = db.begin(&txcx).unwrap();
        for relational in [false, true] {
            let plan = aggregate(relational);
            let measured = db.execute_graph_aggregate_governed(&cx, &plan, policy()).unwrap();
            let caps = [measured.rows.snapshot_records, measured.rows.result_rows,
                measured.evaluator.work_units, measured.evaluator.scratch_entries];
            assert_eq!(db.execute_graph_aggregate_governed(&cx, &plan,
                GqlQueryPolicy::new(caps[0], caps[1], caps[2], caps[3])).unwrap(), measured);
            for dimension in 0..4 {
                let mut cap = caps; cap[dimension] -= 1;
                assert!(db.execute_graph_aggregate_governed(&cx, &plan,
                    GqlQueryPolicy::new(cap[0], cap[1], cap[2], cap[3])).is_err());
            }
            let zero = GqlQueryPolicy::new(0, 0, 0, 0);
            assert!(matches!(txn.execute_graph_aggregate_governed(&other, &cx, &plan, zero),
                Err(GqlQueryError::Source(GraphAggregateError::Source(WriteTxnError::WrongDatabase)))));
            for result in [
                db.execute_graph_aggregate_governed_at(&cx, &plan, CommitSeq(basis.0 + 1), zero),
                pinned.execute_graph_aggregate_governed_at(&cx, &plan, CommitSeq(basis.0 + 1), zero),
            ] { assert!(matches!(result, Err(GqlQueryError::Source(GraphAggregateError::Source(
                    GqlError::Read(ReadError::BeyondFrontier { .. })))))); }
            assert_eq!(numbers(&txn.execute_graph_aggregate_governed(&db, &cx, &plan, policy()).unwrap().value), (5, 5, 7, 13));
        }
        assert!(matches!(txn.finish(&mut db, &commit).await.unwrap(), EmbeddedTxnCompletion::ReadClosed { .. }));
        assert_eq!(db.frontier().unwrap(), basis);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn native_hidden_capture_count_executes_instead_of_refusing_identified_edges() {
    let ((), report) = run_async_under_lab(0x31a6_0006, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit(); let cx = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        for mode in ["WALK", "ACYCLIC", "SIMPLE", "ALL SHORTEST WALK", "ANY SHORTEST WALK"] {
            let text = format!("MATCH route = {mode} (a)-[:R*1..2]->(b) WHERE a.p=1 RETURN COUNT(*) AS paths");
            let plan = PreparedGraphAggregateText::prepare(&text, |kind, name| {
                use fgdb_gql::{GraphSymbol, GraphSymbolKind};
                match (kind, name) {
                    (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
                    (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
                    _ => None,
                }
            }).unwrap().bind_parameters(&GqlParameters::new()).unwrap();
            assert!(plan.input_pattern().plan().requires_identified_edges());
            let expected = match mode { "ALL SHORTEST WALK" => 3, "ANY SHORTEST WALK" => 2, _ => 5 };
            assert_eq!(db.execute_graph_aggregate_governed(&cx, &plan, policy()).unwrap().value[0].values()[0].as_count(), Some(expected));
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
