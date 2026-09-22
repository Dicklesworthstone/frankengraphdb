//! Public native routing over the real lab/MemVfs database. Zero graph-source
//! allowance must suffice for row-only aggregates, but never bypass ownership.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, NativeReadClass, PreparedNativeRead, QueryError, QueryResult,
    WriteBatch, WriteTxnError};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{GqlListParameter, GqlParameterValue, GqlParameters, GqlQueryError, GqlQueryPolicy,
    GraphAggregateValue, GraphExactAverage, GraphSymbol, GraphSymbolKind};
use fgdb_types::{CanonicalScalar, CommitSeq, DatabaseSecurityNamespaceId, PurposeContexts, VId};

fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0x61; 32], DatabaseSecurityNamespaceId([0x62; 32]), [0x63; 32])
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(0, 100, 1_000_000, 1_000_000)
}
fn no_symbols(_: GraphSymbolKind, _: &str) -> Option<GraphSymbol> {
    panic!("source-free native preparation must not resolve a graph symbol")
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
        _ => None,
    }
}
fn batch(id: u128, value: i64) -> WriteBatch {
    let mut batch = WriteBatch::new(RelationId(1));
    batch.create_vertex(VId(id), vec![], vec![(PropertyKeyId(1), CanonicalScalar::Int(value))]);
    batch
}
fn list_parameters(step: i64) -> GqlParameters {
    let mut args = GqlParameters::new().with_int64("step", step).unwrap();
    let values = [CanonicalScalar::Int(5), CanonicalScalar::Int(5), CanonicalScalar::Null,
        CanonicalScalar::Int(9)];
    args.insert("xs", GqlParameterValue::List(GqlListParameter::new(
        values.into_iter().map(GraphValue::Scalar).collect(),
    ).unwrap())).unwrap();
    args
}
fn check_summary(result: &QueryResult, total: i128, count: u64) {
    let QueryResult::Rows { columns, rows } = result else { panic!("read returned a write receipt") };
    assert_eq!(columns, &["total", "rows", "mean"]);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0].as_integer(), Some(total));
    assert_eq!(rows[0][1].as_count(), Some(count));
    assert_eq!(rows[0][2].as_average(), GraphExactAverage::new(total, 3));
}

#[test]
fn database_view_and_reusable_native_template_execute_list_aggregates_without_a_scan() {
    let ((), report) = run_async_under_lab(0x7366_6101, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, batch(1, 100)).await.unwrap();
        let view = db.read_session().unwrap();
        let basis = view.frontier();
        let text = "UNWIND $xs AS x RETURN SUM(x*$step) AS total,COUNT(*) AS rows,AVG(x*$step) AS mean";
        let args = list_parameters(2);
        let prepared = PreparedNativeRead::prepare(text, &args, no_symbols).unwrap();
        assert_eq!(prepared.facade_class(), NativeReadClass::PipelineAggregate);
        let PreparedNativeRead::PipelineAggregate(native) = &prepared else { unreachable!() };
        assert!(native.is_source_free());
        let first = db.query(&cx, text, &args, no_symbols, policy()).unwrap();
        check_summary(&first, 38, 4);
        assert_eq!(prepared.execute(&db, &cx, &args, policy()).unwrap(), first);
        assert_eq!(view.query(&cx, text, &args, no_symbols, policy()).unwrap(), first);
        assert_eq!(db.frontier().unwrap(), basis);
        db.write(&commit, batch(2, 900)).await.unwrap();
        assert_eq!(db.query(&cx, text, &args, no_symbols, policy()).unwrap(), first);
        drop(db);
        assert_eq!(prepared.execute_in_view(&view, &cx, &args, policy()).unwrap(), first);
        check_summary(&prepared.execute_in_view(&view, &cx, &list_parameters(3), policy()).unwrap(), 57, 4);
        assert_eq!(view.frontier(), basis);
        assert!(prepared.execute_in_view(&view, &cx, &GqlParameters::new(), policy()).is_err());
        assert_eq!(contexts.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn standalone_and_empty_input_counts_observe_output_and_work_limits() {
    let ((), report) = run_async_under_lab(0x7366_6102, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let db = Database::open_memory(&commit, keys()).await.unwrap();
        let args = GqlParameters::new();
        for (text, count) in [
            ("RETURN COUNT(*) AS rows", 1),
            ("UNWIND [] AS x RETURN COUNT(*) AS rows", 0),
        ] {
            assert_eq!(db.query(&cx, text, &args, no_symbols, policy()).unwrap(), QueryResult::Rows {
                columns: vec!["rows".to_owned()], rows: vec![vec![GraphAggregateValue::Count(count)]],
            });
        }
        assert!(matches!(db.query(&cx, "RETURN COUNT(*)", &args, no_symbols,
            GqlQueryPolicy::new(0, 0, 1_000_000, 1_000_000)),
            Err(QueryError::Aggregate(GqlQueryError::Rows(_)))));
        assert!(db.query(&cx, "RETURN COUNT(*) LIMIT 0", &args, no_symbols,
            GqlQueryPolicy::new(0, 0, 0, 0)).is_err());
        assert!(matches!(db.query(&cx, "RETURN SUM(1/0) LIMIT 0", &args, no_symbols, policy()),
            Err(QueryError::Aggregate(GqlQueryError::Source(_)))));
        assert_eq!(db.frontier().unwrap(), CommitSeq(0));
        assert_eq!(contexts.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn source_free_transaction_reads_retain_owner_checks_staged_effects_and_graph_lane() {
    let ((), report) = run_async_under_lab(0x7366_6103, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let other = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, batch(1, 4)).await.unwrap();
        let mut transaction = db.begin(&txcx).unwrap();
        transaction.write(&mut db, batch(2, 7)).unwrap();
        let params = GqlParameters::new();
        let text = "WITH 7 AS x RETURN SUM(x) AS total";
        let prepared = PreparedNativeRead::prepare(text, &params, no_symbols).unwrap();
        let expected = QueryResult::Rows {
            columns: vec!["total".to_owned()], rows: vec![vec![GraphAggregateValue::Integer(7)]],
        };
        assert_eq!(transaction.query(&db, &cx, text, &params, no_symbols, policy()).unwrap(), expected);
        assert_eq!(prepared.execute_in_transaction(&transaction, &db, &cx, &params, policy()).unwrap(), expected);
        let wrong = transaction.query(&other, &cx, text, &params, no_symbols,
            GqlQueryPolicy::new(0, 0, 0, 0));
        assert!(matches!(wrong, Err(QueryError::Transaction(error))
            if matches!(*error, WriteTxnError::WrongDatabase)));
        let wrong = prepared.execute_in_transaction(&transaction, &other, &cx, &params,
            GqlQueryPolicy::new(0, 0, 0, 0));
        assert!(matches!(wrong, Err(QueryError::Transaction(error))
            if matches!(*error, WriteTxnError::WrongDatabase)));
        assert!(transaction.query(&db, &cx, "RETURN SUM(1/0) LIMIT 0", &params, no_symbols, policy()).is_err());
        let graph = "MATCH (n) WITH n.p AS p RETURN SUM(p) AS total";
        let graph_prepared = PreparedNativeRead::prepare(graph, &params, symbols).unwrap();
        let PreparedNativeRead::PipelineAggregate(native) = &graph_prepared else { unreachable!() };
        assert!(!native.is_source_free());
        let graph_policy = GqlQueryPolicy::new(100, 100, 1_000_000, 1_000_000);
        let staged = graph_prepared.execute_in_transaction(&transaction, &db, &cx, &params, graph_policy).unwrap();
        assert_eq!(staged, QueryResult::Rows {
            columns: vec!["total".to_owned()], rows: vec![vec![GraphAggregateValue::Integer(11)]],
        });
        assert_eq!(db.query(&cx, graph, &params, symbols, graph_policy).unwrap(), QueryResult::Rows {
            columns: vec!["total".to_owned()], rows: vec![vec![GraphAggregateValue::Integer(4)]],
        });
        assert_eq!(db.frontier().unwrap(), CommitSeq(1));
        transaction.abort();
        assert_eq!(contexts.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
