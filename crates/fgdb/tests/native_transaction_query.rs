//! External-consumer regressions for the native transaction read facade.
//! These use the real lab/MemVfs database and its normal publication/FCW path.

use asupersync::lab::run_async_under_lab;
use fgdb::{
    Database, DatabaseKeys, NativeReadClass, PreparedNativeRead, QueryError, QueryResult,
    ReadError, WriteBatch, WriteError, WriteTxnError,
};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GraphAggregateValue, GraphSymbol, GraphSymbolKind,
};
use fgdb_types::{
    CanonicalScalar, CommitSeq, DatabaseSecurityNamespaceId, EmbeddedTxnCompletion,
    PurposeContexts, VId,
};
use std::cell::Cell;
use std::error::Error;

const PROPERTY: PropertyKeyId = PropertyKeyId(1);
const PERSON: LabelId = LabelId(1);
const OTHER: LabelId = LabelId(2);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0xa1; 32],
        DatabaseSecurityNamespaceId([0xa2; 32]),
        [0xa3; 32],
    )
}

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PROPERTY)),
        (GraphSymbolKind::Label, "Person") => Some(GraphSymbol::Label(PERSON)),
        (GraphSymbolKind::Label, "Other") => Some(GraphSymbol::Label(OTHER)),
        _ => None,
    }
}

fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(10_000, 10_000, 1_000_000, 1_000_000)
}

fn batch(vid: u128, value: i64, labels: Vec<LabelId>) -> WriteBatch {
    let mut batch = WriteBatch::new(RelationId(1));
    batch.create_vertex(
        VId(vid),
        labels,
        vec![(PROPERTY, CanonicalScalar::Int(value))],
    );
    batch
}

fn integers(column: &str, values: &[i64]) -> QueryResult {
    QueryResult::Rows {
        columns: vec![column.to_owned()],
        rows: values
            .iter()
            .map(|value| {
                vec![GraphAggregateValue::Value(GraphValue::Scalar(
                    CanonicalScalar::Int(*value),
                ))]
            })
            .collect(),
    }
}

#[test]
fn every_non_temporal_facade_reads_staged_effects_and_matches_the_committed_result() {
    let ((), report) = run_async_under_lab(0x6e61_7401, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, batch(1, 4, vec![])).await.unwrap();
        let basis = db.write(&commit, batch(2, 7, vec![])).await.unwrap();
        let params = GqlParameters::new();
        let cases = [
            (
                "MATCH (n) RETURN n.p AS p ORDER BY p",
                NativeReadClass::Pattern,
            ),
            (
                "MATCH (n) RETURN COUNT(*) AS total, SUM(n.p) AS amount",
                NativeReadClass::Aggregate,
            ),
            (
                "MATCH (n) WITH n.p AS p RETURN COUNT(*) AS total, SUM(p) AS amount",
                NativeReadClass::PipelineAggregate,
            ),
            (
                "MATCH (n) RETURN n.p AS p UNION ALL MATCH (m) RETURN m.p AS p",
                NativeReadClass::Set,
            ),
        ];
        let plans: Vec<_> = cases
            .iter()
            .map(|(text, class)| {
                let plan = PreparedNativeRead::prepare(text, &params, symbols).unwrap();
                assert_eq!(plan.facade_class(), *class, "{text}");
                plan
            })
            .collect();
        let before: Vec<_> = cases
            .iter()
            .map(|(text, _)| db.query(&cx, text, &params, symbols, policy()).unwrap())
            .collect();
        assert_eq!(before[0], integers("p", &[4, 7]));

        let mut txn = db.begin(&txcx).unwrap();
        txn.write(&mut db, batch(3, 11, vec![])).unwrap();
        let mut staged = Vec::new();
        for (index, (text, _)) in cases.iter().enumerate() {
            let result = txn
                .query(&db, &cx, text, &params, symbols, policy())
                .unwrap();
            assert_ne!(
                result, before[index],
                "read silently fell back to live data: {text}"
            );
            assert_eq!(
                plans[index]
                    .execute_in_transaction(&txn, &db, &cx, &params, policy())
                    .unwrap(),
                result,
                "prepared and textual transaction reads disagree: {text}"
            );
            assert_eq!(
                db.query(&cx, text, &params, symbols, policy()).unwrap(),
                before[index]
            );
            staged.push(result);
        }
        assert_eq!(staged[0], integers("p", &[4, 7, 11]));
        assert_eq!(txn.basis(), basis);
        assert_eq!(db.frontier().unwrap(), basis);
        assert!(db.vertex(VId(3)).unwrap().is_none());
        assert!(matches!(
            txn.finish(&mut db, &commit).await.unwrap(),
            EmbeddedTxnCompletion::WriteCommitted { .. }
        ));
        for (index, (text, _)) in cases.iter().enumerate() {
            assert_eq!(
                db.query(&cx, text, &params, symbols, policy()).unwrap(),
                staged[index]
            );
        }
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn ownership_and_lifecycle_precede_resolver_binding_and_zero_budget() {
    let ((), report) = run_async_under_lab(0x6e61_7402, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let other = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, batch(1, 4, vec![])).await.unwrap();
        let params = GqlParameters::new();
        let text = "MATCH (n) RETURN n.p AS p";
        let plan = PreparedNativeRead::prepare(text, &params, symbols).unwrap();
        let zero = GqlQueryPolicy::new(0, 0, 0, 0);
        let mut txn = db.begin(&txcx).unwrap();
        let calls = Cell::new(0);
        let error = txn
            .query(
                &other,
                &cx,
                text,
                &params,
                |kind: GraphSymbolKind, name: &str| {
                    calls.set(calls.get() + 1);
                    symbols(kind, name)
                },
                zero,
            )
            .unwrap_err();
        assert!(matches!(&error, QueryError::Transaction(error)
            if matches!(error.as_ref(), WriteTxnError::WrongDatabase)));
        assert!(error.source().is_some());
        assert_eq!(calls.get(), 0);
        assert!(matches!(
            plan.execute_in_transaction(&txn, &other, &cx, &params, zero),
            Err(QueryError::Transaction(error))
                if matches!(error.as_ref(), WriteTxnError::WrongDatabase)
        ));
        assert_eq!(
            txn.query(&db, &cx, text, &params, symbols, policy())
                .unwrap(),
            integers("p", &[4])
        );
        let frontier = db.frontier().unwrap();
        assert!(matches!(
            txn.finish(&mut db, &commit).await.unwrap(),
            EmbeddedTxnCompletion::ReadClosed { .. }
        ));
        assert_eq!(db.frontier().unwrap(), frontier);
        assert!(matches!(
            txn.query(&db, &cx, text, &params, |kind: GraphSymbolKind, name: &str| {
                calls.set(calls.get() + 1);
                symbols(kind, name)
            }, zero),
            Err(QueryError::Transaction(error))
                if matches!(error.as_ref(), WriteTxnError::Finished)
        ));
        assert_eq!(calls.get(), 0);
        assert!(matches!(
            plan.execute_in_transaction(&txn, &db, &cx, &params, zero),
            Err(QueryError::Transaction(error))
                if matches!(error.as_ref(), WriteTxnError::Finished)
        ));
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn reusable_native_template_rebinds_values_without_resolving_again() {
    let ((), report) = run_async_under_lab(0x6e61_7403, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, batch(1, 4, vec![])).await.unwrap();
        let mut txn = db.begin(&txcx).unwrap();
        txn.write(&mut db, batch(2, 11, vec![])).unwrap();
        let calls = Cell::new(0);
        let low = GqlParameters::new().with_int64("floor", 3).unwrap();
        let high = GqlParameters::new().with_int64("floor", 8).unwrap();
        let text = "MATCH (n) WHERE n.p > $floor RETURN n.p AS p ORDER BY p";
        let plan = PreparedNativeRead::prepare(text, &low, |kind: GraphSymbolKind, name: &str| {
            calls.set(calls.get() + 1);
            symbols(kind, name)
        })
        .unwrap();
        let resolved = calls.get();
        assert!(resolved > 0);
        assert_eq!(
            plan.execute_in_transaction(&txn, &db, &cx, &low, policy())
                .unwrap(),
            integers("p", &[4, 11])
        );
        assert_eq!(
            plan.execute_in_transaction(&txn, &db, &cx, &high, policy())
                .unwrap(),
            integers("p", &[11])
        );
        assert!(matches!(
            plan.execute_in_transaction(&txn, &db, &cx, &GqlParameters::new(), policy()),
            Err(QueryError::PatternText(_))
        ));
        assert_eq!(calls.get(), resolved);
        assert_eq!(
            plan.execute_in_transaction(&txn, &db, &cx, &low, policy())
                .unwrap(),
            integers("p", &[4, 11])
        );
        txn.finish(&mut db, &commit).await.unwrap();
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn output_refusal_is_typed_and_does_not_discard_staged_effects() {
    let ((), report) = run_async_under_lab(0x6e61_7404, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let basis = db.write(&commit, batch(1, 4, vec![])).await.unwrap();
        let mut txn = db.begin(&txcx).unwrap();
        txn.write(&mut db, batch(2, 11, vec![])).unwrap();
        let text = "MATCH (n) RETURN n.p AS p ORDER BY p";
        let params = GqlParameters::new();
        let error = txn
            .query(
                &db,
                &cx,
                text,
                &params,
                symbols,
                GqlQueryPolicy::new(10_000, 0, 1_000_000, 1_000_000),
            )
            .unwrap_err();
        assert!(matches!(&error, QueryError::TransactionPattern(error)
            if matches!(error.as_ref(), GqlQueryError::Rows(_))));
        assert!(error.source().is_some());
        assert_eq!(db.frontier().unwrap(), basis);
        assert!(db.vertex(VId(2)).unwrap().is_none());
        assert_eq!(
            txn.query(&db, &cx, text, &params, symbols, policy())
                .unwrap(),
            integers("p", &[4, 11])
        );
        txn.finish(&mut db, &commit).await.unwrap();
        assert!(db.vertex(VId(2)).unwrap().is_some());
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn empty_and_output_refused_reads_keep_matching_label_phantoms_but_not_unrelated_insertions() {
    let ((), report) = run_async_under_lab(0x6e61_7405, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        for has_person in [false, true] {
            for prepared_route in [false, true] {
                for matching in [false, true] {
                    let mut db = Database::open_memory(&commit, keys()).await.unwrap();
                    db.write(
                        &commit,
                        batch(1, 4, vec![if has_person { PERSON } else { OTHER }]),
                    )
                    .await
                    .unwrap();
                    let mut txn = db.begin(&txcx).unwrap();
                    txn.write(&mut db, batch(99, 99, vec![OTHER])).unwrap();
                    let params = GqlParameters::new();
                    let text = "MATCH (n:Person) RETURN n.p AS p";
                    let allowance = if has_person {
                        GqlQueryPolicy::new(10_000, 0, 1_000_000, 1_000_000)
                    } else {
                        policy()
                    };
                    let result = if prepared_route {
                        PreparedNativeRead::prepare(text, &params, symbols)
                            .unwrap()
                            .execute_in_transaction(&txn, &db, &cx, &params, allowance)
                    } else {
                        txn.query(&db, &cx, text, &params, symbols, allowance)
                    };
                    if has_person {
                        assert!(matches!(result, Err(QueryError::TransactionPattern(error))
                            if matches!(error.as_ref(), GqlQueryError::Rows(_))));
                    } else {
                        assert_eq!(result.unwrap(), integers("p", &[]));
                    }
                    let winner = db
                        .write(
                            &commit,
                            batch(2, 7, vec![if matching { PERSON } else { OTHER }]),
                        )
                        .await
                        .unwrap();
                    let completed = txn.finish(&mut db, &commit).await;
                    if matching {
                        assert!(matches!(
                            completed,
                            Err(WriteTxnError::Write(WriteError::FirstCommitterWins { .. }))
                        ));
                        assert!(db.vertex(VId(99)).unwrap().is_none());
                        assert_eq!(db.frontier().unwrap(), winner);
                    } else {
                        assert!(matches!(
                            completed.unwrap(),
                            EmbeddedTxnCompletion::WriteCommitted { .. }
                        ));
                        assert!(db.vertex(VId(99)).unwrap().is_some());
                    }
                    assert_eq!(txcx.outstanding_obligations(), 0);
                }
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn prepared_reads_keep_the_transaction_basis_when_the_live_database_advances() {
    let ((), report) = run_async_under_lab(0x6e61_7406, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let basis = db.write(&commit, batch(1, 4, vec![PERSON])).await.unwrap();
        let mut txn = db.begin(&txcx).unwrap();
        txn.write(&mut db, batch(2, 11, vec![PERSON])).unwrap();
        let params = GqlParameters::new();
        let text = "MATCH (n:Person) RETURN n.p AS p ORDER BY p";
        let plan = PreparedNativeRead::prepare(text, &params, symbols).unwrap();
        let advanced = db.write(&commit, batch(3, 23, vec![OTHER])).await.unwrap();
        assert_ne!(advanced, basis);
        assert_eq!(
            plan.execute_in_transaction(&txn, &db, &cx, &params, policy())
                .unwrap(),
            integers("p", &[4, 11])
        );
        assert_eq!(
            txn.query(&db, &cx, text, &params, symbols, policy())
                .unwrap(),
            integers("p", &[4, 11])
        );
        assert_eq!(
            db.query(&cx, text, &params, symbols, policy()).unwrap(),
            integers("p", &[4])
        );
        assert_eq!(db.frontier().unwrap(), advanced);
        assert_eq!(txn.basis(), basis);
        txn.finish(&mut db, &commit).await.unwrap();
        assert!(db.vertex(VId(2)).unwrap().is_some());
        assert!(db.vertex(VId(3)).unwrap().is_some());
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn every_temporal_class_reads_committed_history_without_changing_staged_effects() {
    let ((), report) = run_async_under_lab(0x6e61_7407, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let basis = db.write(&commit, batch(1, 4, vec![])).await.unwrap();
        let mut txn = db.begin(&txcx).unwrap();
        let mut staged = batch(2, 11, vec![]);
        staged.set_vertex_property(VId(1), PROPERTY, Some(CanonicalScalar::Int(99)));
        txn.write(&mut db, staged).unwrap();
        let params = GqlParameters::new();
        let cases = [
            (
                "MATCH (n) FOR SYSTEM_TIME AS OF SEQ 1 RETURN n.p AS p",
                NativeReadClass::TemporalPattern,
                integers("p", &[4]),
            ),
            (
                "MATCH (n) FOR SYSTEM_TIME AS OF SEQ 1 RETURN COUNT(*) AS total",
                NativeReadClass::TemporalAggregate,
                QueryResult::Rows {
                    columns: vec!["total".to_owned()],
                    rows: vec![vec![GraphAggregateValue::Count(1)]],
                },
            ),
            (
                "MATCH (n) FOR SYSTEM_TIME AS OF SEQ 1 RETURN n.p AS p UNION ALL MATCH (m) RETURN m.p AS p",
                NativeReadClass::TemporalSet,
                integers("p", &[4, 4]),
            ),
        ];
        for (text, class, expected) in cases {
            let prepared = PreparedNativeRead::prepare(text, &params, symbols).unwrap();
            assert_eq!(prepared.facade_class(), class);
            assert_eq!(
                prepared
                    .execute_in_transaction(&txn, &db, &cx, &params, policy())
                    .unwrap(),
                expected
            );
            assert_eq!(
                txn.query(&db, &cx, text, &params, symbols, policy())
                    .unwrap(),
                expected
            );
            assert_eq!(
                prepared
                    .execute_in_transaction_with_snapshot(&txn, &db, &cx, &params, policy())
                    .unwrap(),
                (expected, Some(basis))
            );
            assert_eq!(db.frontier().unwrap(), basis);
            assert_eq!(txn.basis(), basis);
            assert!(db.vertex(VId(2)).unwrap().is_none());
        }
        assert_eq!(
            txn.query(
                &db,
                &cx,
                "MATCH (n) RETURN n.p AS p ORDER BY p",
                &params,
                symbols,
                policy()
            )
            .unwrap(),
            integers("p", &[11, 99])
        );
        assert_eq!(
            PreparedNativeRead::prepare("MATCH (n) RETURN n.p AS p ORDER BY p", &params, symbols)
                .unwrap()
                .execute_in_transaction_with_snapshot(&txn, &db, &cx, &params, policy())
                .unwrap(),
            (integers("p", &[11, 99]), None)
        );
        txn.finish(&mut db, &commit).await.unwrap();
        assert!(db.vertex(VId(2)).unwrap().is_some());
        assert_eq!(
            db.vertex(VId(1)).unwrap().unwrap().props[0].1,
            CanonicalScalar::Int(99).into()
        );
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn temporal_parameters_rebind_below_the_original_basis_after_live_advancement() {
    let ((), report) = run_async_under_lab(0x6e61_7408, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let first = db.write(&commit, batch(1, 4, vec![])).await.unwrap();
        let mut edit = WriteBatch::new(RelationId(1));
        edit.set_vertex_property(VId(1), PROPERTY, Some(CanonicalScalar::Int(7)));
        let basis = db.write(&commit, edit).await.unwrap();
        let mut txn = db.begin(&txcx).unwrap();
        txn.write(&mut db, batch(2, 11, vec![])).unwrap();
        let mut edit = WriteBatch::new(RelationId(1));
        edit.set_vertex_property(VId(1), PROPERTY, Some(CanonicalScalar::Int(12)));
        let later = db.write(&commit, edit).await.unwrap();
        let params = |at: CommitSeq| GqlParameters::new().with_uint64("at", at.0).unwrap();
        let sum = |value| QueryResult::Rows {
            columns: vec!["total".to_owned()],
            rows: vec![vec![GraphAggregateValue::Integer(value)]],
        };
        let cases = [
            (
                "MATCH (n) FOR SYSTEM_TIME AS OF SEQ $at RETURN n.p AS p",
                integers("p", &[4]),
                integers("p", &[7]),
            ),
            (
                "MATCH (n) FOR SYSTEM_TIME AS OF SEQ $at RETURN n.p AS p UNION ALL MATCH (m) RETURN m.p AS p",
                integers("p", &[4, 4]),
                integers("p", &[7, 7]),
            ),
            (
                "MATCH (n) FOR SYSTEM_TIME AS OF SEQ $at RETURN SUM(n.p) AS total",
                sum(4),
                sum(7),
            ),
            (
                "UNWIND [1,2] AS factor MATCH (n) FOR SYSTEM_TIME AS OF SEQ $at RETURN SUM(n.p * factor) AS total",
                sum(12),
                sum(21),
            ),
        ];
        for (text, expected_first, expected_basis) in cases {
            let calls = Cell::new(0);
            let prepared = PreparedNativeRead::prepare(
                text,
                &params(first),
                |kind: GraphSymbolKind, name: &str| {
                    calls.set(calls.get() + 1);
                    symbols(kind, name)
                },
            )
            .unwrap();
            let resolved = calls.get();
            assert!(resolved > 0);
            for (at, expected) in [
                (first, &expected_first),
                (basis, &expected_basis),
                (first, &expected_first),
            ] {
                assert_eq!(
                    &prepared
                        .execute_in_transaction(&txn, &db, &cx, &params(at), policy())
                        .unwrap(),
                    expected,
                    "{text} at {at:?}"
                );
            }
            assert!(matches!(
                prepared.execute_in_transaction(&txn, &db, &cx, &params(later), policy()),
                Err(QueryError::Read(ReadError::BeyondFrontier { asked, frontier }))
                    if asked == later && frontier == basis
            ));
            // An empty output page and no work allowance cannot waive the
            // transaction's upper visibility bound.
            let empty =
                PreparedNativeRead::prepare(&format!("{text} LIMIT 0"), &params(later), symbols)
                    .unwrap();
            assert!(matches!(
                empty.execute_in_transaction(&txn, &db, &cx, &params(later), GqlQueryPolicy::new(0, 0, 0, 0)),
                Err(QueryError::Read(ReadError::BeyondFrontier { asked, frontier }))
                    if asked == later && frontier == basis
            ));
            assert!(
                prepared
                    .execute_in_transaction(&txn, &db, &cx, &GqlParameters::new(), policy())
                    .is_err()
            );
            assert_eq!(calls.get(), resolved, "rebinding re-entered the catalog");
        }
        assert_eq!(txn.basis(), basis);
        assert_eq!(db.frontier().unwrap(), later);
        // Immutable historical observations must not become current-state
        // dependencies on vertex 1, which changed after the pinned basis.
        txn.finish(&mut db, &commit).await.unwrap();
        assert!(db.vertex(VId(2)).unwrap().is_some());
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn historical_budget_and_interruption_refusals_preserve_workspace_and_current_witnesses() {
    use fgdb_types::context::SimulationCheckpointProbe;
    use std::sync::Arc;

    let ((), report) = run_async_under_lab(0x6e61_7409, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let basis = db.write(&commit, batch(1, 4, vec![])).await.unwrap();
        let mut txn = db.begin(&txcx).unwrap();
        txn.write(&mut db, batch(2, 11, vec![])).unwrap();
        let params = GqlParameters::new();
        // This CURRENT observation must survive subsequent successful and
        // refused historical statements. It remains a conflict on completion.
        assert_eq!(
            txn.query(
                &db,
                &cx,
                "MATCH (n) RETURN n.p AS p ORDER BY p",
                &params,
                symbols,
                policy()
            )
            .unwrap(),
            integers("p", &[4, 11])
        );
        for text in [
            "MATCH (n) FOR SYSTEM_TIME AS OF SEQ 1 RETURN n.p AS p",
            "MATCH (n) FOR SYSTEM_TIME AS OF SEQ 1 RETURN COUNT(*) AS total",
            "MATCH (n) FOR SYSTEM_TIME AS OF SEQ 1 RETURN n.p AS p UNION ALL MATCH (m) RETURN m.p AS p",
            "UNWIND [1,2] AS factor MATCH (n) FOR SYSTEM_TIME AS OF SEQ 1 RETURN SUM(n.p * factor) AS total",
        ] {
            let prepared = PreparedNativeRead::prepare(text, &params, symbols).unwrap();
            assert!(
                matches!(
                    prepared.execute_in_transaction(
                        &txn,
                        &db,
                        &cx,
                        &params,
                        GqlQueryPolicy::new(10_000, 0, 1_000_000, 1_000_000)
                    ),
                    Err(QueryError::Pattern(GqlQueryError::Rows(_))
                        | QueryError::Aggregate(GqlQueryError::Rows(_))
                        | QueryError::Set(GqlQueryError::Rows(_)))
                ),
                "historical output quota was bypassed: {text}"
            );
            let cancelled =
                cx.with_checkpoint_probe(Arc::new(SimulationCheckpointProbe::new(Some(1))));
            assert!(
                matches!(
                    prepared.execute_in_transaction(&txn, &db, &cancelled, &params, policy()),
                    Err(QueryError::Pattern(GqlQueryError::Interrupted(_))
                        | QueryError::Aggregate(GqlQueryError::Interrupted(_))
                        | QueryError::Set(GqlQueryError::Interrupted(_)))
                ),
                "historical cancellation was bypassed: {text}"
            );
            assert_eq!(
                prepared
                    .execute_in_transaction(&txn, &db, &cx, &params, policy())
                    .unwrap(),
                db.query(&cx, text, &params, symbols, policy()).unwrap()
            );
            assert_eq!(db.frontier().unwrap(), basis);
        }
        assert_eq!(
            txn.query(
                &db,
                &cx,
                "MATCH (n) FOR SYSTEM_TIME AS OF SEQ 0 RETURN n.p AS p",
                &params,
                symbols,
                policy()
            )
            .unwrap(),
            integers("p", &[])
        );
        assert_eq!(
            txn.query(
                &db,
                &cx,
                "MATCH (n) RETURN n.p AS p ORDER BY p",
                &params,
                symbols,
                policy()
            )
            .unwrap(),
            integers("p", &[4, 11])
        );
        let mut edit = WriteBatch::new(RelationId(1));
        edit.set_vertex_property(VId(1), PROPERTY, Some(CanonicalScalar::Int(12)));
        let winner = db.write(&commit, edit).await.unwrap();
        assert!(matches!(
            txn.finish(&mut db, &commit).await,
            Err(WriteTxnError::Write(WriteError::FirstCommitterWins { .. }))
        ));
        assert_eq!(db.frontier().unwrap(), winner);
        assert!(db.vertex(VId(2)).unwrap().is_none());
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
