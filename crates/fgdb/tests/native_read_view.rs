//! Native readers must stay on the view's immutable generation, not the writer.
//! All data is published through the real lab/MemVfs Chronicle/Strata path.

use asupersync::lab::run_async_under_lab;
use fgdb::{
    Database, DatabaseKeys, GqlError, NativeReadClass, PreparedNativeRead, QueryError,
    QueryResult, ReadError, WriteBatch,
};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GraphAggregateError, GraphAggregateValue,
    GraphSetExecutionError, GraphSymbol, GraphSymbolKind,
};
use fgdb_types::{CanonicalScalar, CommitSeq, DatabaseSecurityNamespaceId, PurposeContexts, VId};
use std::cell::Cell;

const PROPERTY: PropertyKeyId = PropertyKeyId(1);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0x91; 32], DatabaseSecurityNamespaceId([0x92; 32]), [0x93; 32])
}

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PROPERTY)),
        _ => None,
    }
}

fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(10_000, 10_000, 1_000_000, 1_000_000)
}

fn batch(vid: u128, value: i64) -> WriteBatch {
    let mut batch = WriteBatch::new(RelationId(1));
    batch.create_vertex(VId(vid), vec![], vec![(PROPERTY, CanonicalScalar::Int(value))]);
    batch
}

fn integers(values: &[i64]) -> QueryResult {
    QueryResult::Rows {
        columns: vec!["p".to_owned()],
        rows: values.iter().map(|value| vec![GraphAggregateValue::Value(
            GraphValue::Scalar(CanonicalScalar::Int(*value)),
        )]).collect(),
    }
}

fn cases() -> [(&'static str, NativeReadClass); 7] {
    [
        ("MATCH (n) RETURN n.p AS p ORDER BY p", NativeReadClass::Pattern),
        ("MATCH (n) RETURN COUNT(*) AS total, SUM(n.p) AS amount", NativeReadClass::Aggregate),
        (
            "MATCH (n) WITH n.p AS p RETURN COUNT(*) AS total, SUM(p) AS amount",
            NativeReadClass::PipelineAggregate,
        ),
        (
            "MATCH (n) RETURN n.p AS p UNION ALL MATCH (m) RETURN m.p AS p",
            NativeReadClass::Set,
        ),
        (
            "MATCH (n) FOR SYSTEM_TIME AS OF SEQ 1 RETURN n.p AS p ORDER BY p",
            NativeReadClass::TemporalPattern,
        ),
        (
            "MATCH (n) FOR SYSTEM_TIME AS OF SEQ 1 RETURN COUNT(*) AS total, SUM(n.p) AS amount",
            NativeReadClass::TemporalAggregate,
        ),
        (
            "MATCH (n) FOR SYSTEM_TIME AS OF SEQ 1 RETURN n.p AS p UNION ALL MATCH (m) RETURN m.p AS p",
            NativeReadClass::TemporalSet,
        ),
    ]
}

#[test]
fn all_native_classes_remain_generation_exact_after_writes_and_writer_drop() {
    let ((), report) = run_async_under_lab(0x7669_6501, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        assert_eq!(db.write(&commit, batch(1, 4)).await.unwrap(), CommitSeq(1));
        let basis = db.write(&commit, batch(2, 7)).await.unwrap();
        let view = db.read_session().unwrap();
        let clone = view.clone();
        assert!(view.shares_decoded_state_with(&clone));
        let root_before = view.partition_root();
        let manifest_before = view.manifest();
        let params = GqlParameters::new();
        let cases = cases();
        let mut plans = Vec::new();
        let mut expected = Vec::new();
        for (text, class) in &cases {
            let prepared = PreparedNativeRead::prepare(text, &params, symbols).unwrap();
            assert_eq!(prepared.facade_class(), *class, "{text}");
            let result = db.query(&cx, text, &params, symbols, policy()).unwrap();
            assert_eq!(view.query(&cx, text, &params, symbols, policy()).unwrap(), result);
            assert_eq!(prepared.execute_in_view(&clone, &cx, &params, policy()).unwrap(), result);
            plans.push(prepared);
            expected.push(result);
        }
        assert_eq!(expected[0], integers(&[4, 7]));
        assert_eq!(expected[4], integers(&[4]));

        let advanced = db.write(&commit, batch(3, 11)).await.unwrap();
        assert!(advanced > basis);
        let fresh = db.read_session().unwrap();
        assert!(!view.shares_decoded_state_with(&fresh));
        for (index, (text, _)) in cases.iter().enumerate() {
            let result = plans[index].execute_in_view(&fresh, &cx, &params, policy()).unwrap();
            assert_eq!(result, db.query(&cx, text, &params, symbols, policy()).unwrap());
            if index < 4 {
                assert_ne!(result, expected[index], "the successor really changes this answer");
            } else {
                assert_eq!(result, expected[index], "temporal reads select the older cut");
            }
        }
        drop(db);
        drop(view);
        for (index, (text, _)) in cases.iter().enumerate() {
            assert_eq!(
                clone.query(&cx, text, &params, symbols, policy()).unwrap(),
                expected[index],
                "{text}",
            );
            assert_eq!(
                plans[index].execute_in_view(&clone, &cx, &params, policy()).unwrap(),
                expected[index],
            );
        }
        assert_eq!(clone.frontier(), basis);
        assert_eq!(clone.partition_root(), root_before);
        assert_eq!(clone.manifest(), manifest_before);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn prepared_native_reads_rebind_parameters_without_reopening_or_resolving() {
    let ((), report) = run_async_under_lab(0x7669_6502, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, batch(1, 4)).await.unwrap();
        db.write(&commit, batch(2, 11)).await.unwrap();
        let view = db.read_session().unwrap();
        let low = GqlParameters::new().with_int64("floor", 3).unwrap();
        let high = GqlParameters::new().with_int64("floor", 8).unwrap();
        let calls = Cell::new(0);
        let text = "MATCH (n) WHERE n.p > $floor RETURN n.p AS p ORDER BY p";
        let plan = PreparedNativeRead::prepare(text, &low, |kind: GraphSymbolKind, name: &str| {
            calls.set(calls.get() + 1);
            symbols(kind, name)
        }).unwrap();
        let prepared_calls = calls.get();
        assert!(prepared_calls > 0);
        db.write(&commit, batch(3, 23)).await.unwrap();
        drop(db);
        for _ in 0..3 {
            assert_eq!(plan.execute_in_view(&view, &cx, &low, policy()).unwrap(), integers(&[4, 11]));
            assert_eq!(plan.execute_in_view(&view, &cx, &high, policy()).unwrap(), integers(&[11]));
        }
        assert!(matches!(
            plan.execute_in_view(&view, &cx, &GqlParameters::new(), GqlQueryPolicy::new(0, 0, 0, 0)),
            Err(QueryError::PatternText(_)),
        ));
        assert_eq!(calls.get(), prepared_calls);
        assert_eq!(view.query(&cx, text, &low, symbols, policy()).unwrap(), integers(&[4, 11]));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn future_history_cannot_escape_the_view_even_with_zero_budget_or_empty_output() {
    let ((), report) = run_async_under_lab(0x7669_6503, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let basis = db.write(&commit, batch(1, 4)).await.unwrap();
        let view = db.read_session().unwrap();
        assert_eq!(db.write(&commit, batch(2, 11)).await.unwrap(), CommitSeq(2));
        let params = GqlParameters::new();
        let future = [
            "MATCH (n) FOR SYSTEM_TIME AS OF SEQ 2 RETURN n.p AS p LIMIT 0",
            "MATCH (n) FOR SYSTEM_TIME AS OF SEQ 2 RETURN COUNT(*) AS total",
            "MATCH (n) FOR SYSTEM_TIME AS OF SEQ 2 RETURN n.p AS p EXCEPT MATCH (m) RETURN m.p AS p",
        ];
        for (index, text) in future.iter().enumerate() {
            // The live reader can serve the cut. The old view must not borrow it,
            // even when a query's result would have been empty anyway.
            db.query(&cx, text, &params, symbols, policy()).unwrap();
            let plan = PreparedNativeRead::prepare(text, &params, symbols).unwrap();
            for allowance in [policy(), GqlQueryPolicy::new(0, 0, 0, 0)] {
                let error = plan.execute_in_view(&view, &cx, &params, allowance).unwrap_err();
                let source = match (index, error) {
                    (0, QueryError::Pattern(GqlQueryError::Source(GqlError::Read(error)))) => error,
                    (1, QueryError::Aggregate(GqlQueryError::Source(GraphAggregateError::Source(
                        GqlError::Read(error),
                    )))) => error,
                    (2, QueryError::Set(GqlQueryError::Source(GraphSetExecutionError::Source(
                        GqlError::Read(error),
                    )))) => error,
                    (_, error) => panic!("history refusal was masked: {error:?}"),
                };
                assert!(matches!(source, ReadError::BeyondFrontier {
                    asked: CommitSeq(2), frontier,
                } if frontier == basis));
            }
        }
        assert_eq!(view.query(&cx, "MATCH (n) RETURN n.p AS p", &params, symbols, policy()).unwrap(), integers(&[4]));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn native_output_refusals_leave_the_retained_generation_reusable() {
    let ((), report) = run_async_under_lab(0x7669_6504, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, batch(1, 4)).await.unwrap();
        let view = db.read_session().unwrap();
        let identity = (view.frontier(), view.manifest(), view.partition_root());
        let params = GqlParameters::new();
        let no_output = GqlQueryPolicy::new(10_000, 0, 1_000_000, 1_000_000);
        for (text, _) in cases() {
            let expected = view.query(&cx, text, &params, symbols, policy()).unwrap();
            let error = view.query(&cx, text, &params, symbols, no_output).unwrap_err();
            assert!(matches!(error,
                QueryError::Pattern(GqlQueryError::Budget(_))
                    | QueryError::Aggregate(GqlQueryError::Budget(_))
                    | QueryError::Set(GqlQueryError::Budget(_))
            ));
            assert_eq!(view.query(&cx, text, &params, symbols, policy()).unwrap(), expected);
        }
        assert_eq!((view.frontier(), view.manifest(), view.partition_root()), identity);
        assert_eq!(db.frontier().unwrap(), view.frontier());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
