//! OPTIONAL changes a returning CALL's empty-result behavior, not its effects
//! or authority. Every invocation remains in the surrounding atomic owner.

use super::*;

fn null() -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Null)
}

async fn execute(
    db: &mut Database<MemVfs>,
    contexts: &PurposeContexts,
    text: &str,
    columns: &[&str],
    rows: u64,
    allocated: &mut u128,
) -> Vec<Vec<GraphValue>> {
    let result = db
        .query_write(
            &contexts.txn(),
            &contexts.query(),
            &contexts.commit(),
            text,
            &GqlParameters::new(),
            symbols,
            R,
            policy(rows),
            |request| {
                assert!(matches!(request.request, GraphInsertRequest::Vertex { .. }));
                *allocated += 1;
                Ok::<_, Infallible>(ElementId::Vertex(VId(100 + *allocated)))
            },
        )
        .await
        .unwrap_or_else(|error| panic!("{text}: {error:?}"));
    values(result, columns)
}

#[test]
fn optional_write_calls_preserve_duplicate_and_null_inputs_and_empty_body_effects() {
    let ((), report) = run_async_under_lab(0xca11_2001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&contexts.commit(), vfs.clone(), &path, keys())
            .await
            .unwrap();
        let basis = db.frontier().unwrap();
        let pinned = db.read_session().unwrap();
        let mut allocated = 0;
        let mut rows = execute(
            &mut db,
            &contexts,
            "UNWIND [1,1,2,3,null] AS x OPTIONAL CALL(x) { \
             CREATE (created:Visible {p:coalesce(x,0)}) WITH x,created \
             UNWIND CASE WHEN x=1 THEN [10,20] WHEN x=2 THEN [30] ELSE [] END AS y \
             RETURN created AS made,y AS bump } \
             CREATE (tail:Visible {q:coalesce(bump,-1)}) \
             RETURN x,made.p AS p,bump,tail.q AS q",
            &["x", "p", "bump", "q"],
            7,
            &mut allocated,
        )
        .await;
        let mut expected = vec![
            vec![int(1), int(1), int(10), int(10)],
            vec![int(1), int(1), int(20), int(20)],
            vec![int(1), int(1), int(10), int(10)],
            vec![int(1), int(1), int(20), int(20)],
            vec![int(2), int(2), int(30), int(30)],
            vec![int(3), null(), null(), int(-1)],
            vec![null(), null(), null(), int(-1)],
        ];
        rows.sort();
        expected.sort();
        assert_eq!(rows, expected);
        assert_eq!(
            allocated, 12,
            "five body writes and seven downstream writes"
        );
        let vertices = db.vertices().unwrap();
        assert_eq!(vertices.len(), 12);
        assert_eq!(db.frontier().unwrap().0, basis.0 + 1);
        assert_eq!(db.delta_since(basis).unwrap().count(), 1);
        assert!(pinned.vertices().unwrap().is_empty());
        drop(db);
        let reopened = Database::open_with_vfs(&contexts.commit(), vfs, &path, keys())
            .await
            .unwrap();
        assert_eq!(reopened.vertices().unwrap(), vertices);
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn optional_union_extends_absence_once_after_all_arms_and_keeps_their_effects() {
    let ((), report) = run_async_under_lab(0xca11_2002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        for (quantifier, second_filter, expected) in [
            (
                "ALL",
                "WHERE x=2",
                vec![
                    vec![int(1), null()],
                    vec![int(1), null()],
                    vec![int(2), int(2)],
                    vec![int(2), int(2)],
                ],
            ),
            (
                "DISTINCT",
                "WHERE x=2",
                vec![
                    vec![int(1), null()],
                    vec![int(1), null()],
                    vec![int(2), int(2)],
                ],
            ),
            (
                "ALL",
                "",
                vec![
                    vec![int(1), int(1)],
                    vec![int(1), int(1)],
                    vec![int(2), int(2)],
                    vec![int(2), int(2)],
                ],
            ),
        ] {
            let mut db = Database::open_memory(&contexts.commit(), keys())
                .await
                .unwrap();
            let mut allocated = 0;
            let mut rows = execute(
                &mut db,
                &contexts,
                &format!(
                    "UNWIND [1,1,2] AS x OPTIONAL CALL(x) {{ \
                     CREATE (:Visible {{p:x}}) WITH x WHERE x=2 RETURN x AS value \
                     UNION {quantifier} CREATE (:Visible {{p:x+10}}) WITH x {second_filter} \
                     RETURN x AS value }} RETURN x,value"
                ),
                &["x", "value"],
                expected.len() as u64,
                &mut allocated,
            )
            .await;
            rows.sort();
            let mut expected = expected;
            expected.sort();
            assert_eq!(rows, expected, "{quantifier} {second_filter}");
            assert_eq!(allocated, 6, "every arm runs for every outer occurrence");
            let mut properties: Vec<_> = db
                .vertices()
                .unwrap()
                .into_iter()
                .map(|row| row.props)
                .collect();
            properties.sort();
            assert_eq!(
                properties,
                [1, 1, 2, 11, 11, 12]
                    .into_iter()
                    .map(|value| vec![(P, CanonicalScalar::Int(value).into())])
                    .collect::<Vec<_>>()
            );
        }
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn optional_unit_nested_calls_and_empty_global_aggregates_keep_their_own_semantics() {
    let ((), report) = run_async_under_lab(0xca11_2003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let mut db = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        let mut allocated = 0;
        let rows = execute(
            &mut db,
            &contexts,
            "UNWIND [1,1,2] AS x OPTIONAL CALL(x) { \
             OPTIONAL CALL(x) { CREATE (:Visible {p:x}) WITH x WHERE false RETURN x AS absent } \
             CREATE (:Visible {q:coalesce(absent,9)}) } RETURN x",
            &["x"],
            3,
            &mut allocated,
        )
        .await;
        assert_eq!(rows, vec![vec![int(1)], vec![int(1)], vec![int(2)]]);
        assert_eq!(allocated, 6);
        assert_eq!(db.vertices().unwrap().len(), 6);

        let basis = db.frontier().unwrap();
        let rows = execute(
            &mut db,
            &contexts,
            "UNWIND [1,2] AS x OPTIONAL CALL(x) { MATCH (n:Hidden) SET n.p=x \
             WITH n MATCH (m:Hidden) RETURN count(*) AS seen } RETURN x,seen",
            &["x", "seen"],
            2,
            &mut allocated,
        )
        .await;
        assert_eq!(rows, vec![vec![int(1), int(0)], vec![int(2), int(0)]]);
        assert_eq!(allocated, 6);
        assert_eq!(db.frontier().unwrap(), basis);
        let rows = execute(
            &mut db,
            &contexts,
            "UNWIND [] AS x OPTIONAL CALL(x) { CREATE (:Visible) RETURN 1 AS value } RETURN value",
            &["value"],
            0,
            &mut allocated,
        )
        .await;
        assert!(
            rows.is_empty(),
            "an empty outer relation invokes no optional body"
        );
        assert_eq!(allocated, 6);
        assert_eq!(db.frontier().unwrap(), basis);

        let rows = execute(
            &mut db,
            &contexts,
            "CREATE (first:Visible {p:7}) OPTIONAL CALL(first) { \
             WITH first WHERE false CREATE (other:Visible {p:8}) RETURN other AS absent } \
             SET absent.q=1 RETURN first.p AS p,absent.q AS q",
            &["p", "q"],
            1,
            &mut allocated,
        )
        .await;
        assert_eq!(rows, vec![vec![int(7), null()]]);
        assert_eq!(allocated, 7, "a write to the absent export has no target");
        assert_eq!(db.vertices().unwrap().len(), 7);
        assert_eq!(db.frontier().unwrap().0, basis.0 + 1);
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn wildcard_read_calls_feed_optional_write_union_arms_under_one_atomic_owner() {
    let ((), report) = run_async_under_lab(0xca11_2007, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let text = "UNWIND [1,1,2] AS x OPTIONAL CALL (*) { \
                    CALL (*) { RETURN x+10 AS offset } \
                    CREATE (created:Visible {p:offset}) WITH x,created \
                    WHERE x=2 RETURN created AS made \
                    UNION ALL CALL (*) { RETURN x+20 AS offset } \
                    CREATE (created:Visible {p:offset}) WITH x,created \
                    WHERE false RETURN created AS made } RETURN x,made.p AS p";
        let expected = vec![
            vec![int(1), null()],
            vec![int(1), null()],
            vec![int(2), int(12)],
        ];
        let mut db = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        let basis = db.frontier().unwrap();
        let pinned = db.read_session().unwrap();
        let mut allocated = 0;
        assert_eq!(
            execute(&mut db, &contexts, text, &["x", "p"], 3, &mut allocated).await,
            expected
        );
        assert_eq!(allocated, 6, "both arms retain every outer occurrence");
        assert_eq!(db.frontier().unwrap().0, basis.0 + 1);
        assert!(pinned.vertices().unwrap().is_empty());
        let properties = |db: &Database<MemVfs>| {
            let mut properties: Vec<_> = db
                .vertices()
                .unwrap()
                .into_iter()
                .map(|vertex| vertex.props)
                .collect();
            properties.sort();
            properties
        };
        let expected_properties: Vec<_> = [11, 11, 12, 21, 21, 22]
            .into_iter()
            .map(|value| vec![(P, CanonicalScalar::Int(value).into())])
            .collect();
        assert_eq!(properties(&db), expected_properties);

        let issuer = authority();
        for allowed_rows in [3, 2] {
            let mut authorized = Database::open_memory(&contexts.commit(), keys())
                .await
                .unwrap();
            let basis = authorized.frontier().unwrap();
            let token = issuer
                .issue_at(&grant(Rights::ReadWrite, allowed_rows), NOW)
                .unwrap();
            let result = authorized
                .execute_graph_write_chain_authorized(
                    &contexts.txn(),
                    &contexts.query(),
                    &contexts.commit(),
                    &issuer,
                    &token,
                    "main",
                    &program(text),
                    insert_policy(3),
                    || NOW,
                )
                .await;
            if allowed_rows == 3 {
                let (_, output, _) = result.unwrap();
                let actual: Vec<_> = output
                    .value
                    .iter()
                    .map(|row| row.values().to_vec())
                    .collect();
                assert_eq!(actual, expected);
                assert_eq!(properties(&authorized), expected_properties);
                assert_eq!(authorized.frontier().unwrap().0, basis.0 + 1);
            } else {
                result.expect_err("wildcard imports cannot renew the output allowance");
                assert_eq!(authorized.frontier().unwrap(), basis);
                assert!(authorized.vertices().unwrap().is_empty());
            }
        }
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn optional_absence_cannot_hide_static_denials_or_mint_result_and_creation_budgets() {
    let ((), report) = run_async_under_lab(0xca11_2004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let issuer = authority();
        for (text, rights, denial) in [
            (
                "UNWIND [] AS x OPTIONAL CALL(x) { CREATE (n:Visible) RETURN n.secret AS leaked } RETURN leaked",
                Rights::ReadWrite,
                Error::ScopeDenied,
            ),
            (
                "UNWIND [] AS x OPTIONAL CALL(x) { MATCH (n:Visible) SET n.p=1 RETURN n AS seen } RETURN seen",
                Rights::Write,
                Error::PermissionDenied,
            ),
        ] {
            let mut db = Database::open_memory(&contexts.commit(), keys())
                .await
                .unwrap();
            let basis = db.frontier().unwrap();
            let token = issuer.issue_at(&grant(rights, 0), NOW).unwrap();
            let error = db
                .execute_graph_write_chain_authorized(
                    &contexts.txn(),
                    &contexts.query(),
                    &contexts.commit(),
                    &issuer,
                    &token,
                    "main",
                    &program(text),
                    insert_policy(0),
                    || NOW,
                )
                .await
                .unwrap_err();
            assert_eq!(authorization(&error), Some(denial), "{text}: {error:?}");
            assert_eq!(db.frontier().unwrap(), basis);
            assert!(db.vertices().unwrap().is_empty());
        }

        let call = program(
            "OPTIONAL CALL() { CREATE (:Visible) WITH 1 AS value WHERE false RETURN value }",
        );
        for (local_rows, signed_rows, succeeds) in [(1, 1, true), (0, 1, false), (1, 0, false)] {
            let mut db = Database::open_memory(&contexts.commit(), keys())
                .await
                .unwrap();
            let basis = db.frontier().unwrap();
            let token = issuer
                .issue_at(&grant(Rights::ReadWrite, signed_rows), NOW)
                .unwrap();
            let result = db
                .execute_graph_write_chain_authorized(
                    &contexts.txn(),
                    &contexts.query(),
                    &contexts.commit(),
                    &issuer,
                    &token,
                    "main",
                    &call,
                    insert_policy(local_rows),
                    || NOW,
                )
                .await;
            if succeeds {
                let (_, result, _) = result.unwrap();
                assert_eq!(result.value.len(), 1);
                assert_eq!(result.value[0].values(), &[null()]);
                assert_eq!(db.vertices().unwrap().len(), 1);
            } else {
                result.expect_err("the optional NULL row crosses the public result boundary");
                assert_eq!(db.frontier().unwrap(), basis);
                assert!(db.vertices().unwrap().is_empty());
            }
        }

        let mut db = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        let basis = db.frontier().unwrap();
        let token = issuer.issue_at(&grant(Rights::ReadWrite, 3), NOW).unwrap();
        db.execute_graph_write_chain_authorized(
            &contexts.txn(),
            &contexts.query(),
            &contexts.commit(),
            &issuer,
            &token,
            "main",
            &program("UNWIND [1,2,3] AS x OPTIONAL CALL(x) { CREATE (:Visible {p:x}) WITH x WHERE false RETURN x AS value } RETURN x,value"),
            GraphInsertPolicy::new(insert_policy(3).query, 2, 0),
            || NOW,
        )
        .await
        .expect_err("empty returns do not reset the cumulative creation ceiling");
        assert_eq!(db.frontier().unwrap(), basis);
        assert!(db.vertices().unwrap().is_empty());
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn optional_call_errors_roll_back_earlier_null_extended_invocations_and_keep_the_entry_prefix() {
    let ((), report) = run_async_under_lab(0xca11_2005, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let mut db = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        let basis = db.frontier().unwrap();
        let mut transaction = db.begin(&contexts.txn()).unwrap();
        let mut prefix = WriteBatch::new(R);
        prefix.create_vertex(VId(7), vec![VISIBLE], vec![(P, CanonicalScalar::Int(99))]);
        transaction.write(&mut db, prefix).unwrap();
        let before = transaction.staged_effect_digest().unwrap();
        let mut allocated = 0;
        transaction
            .execute_graph_write_chain_governed(
                &mut db,
                &contexts.query(),
                &program("UNWIND [1,2] AS x OPTIONAL CALL(x) { CREATE (:Visible {p:x}) WITH x WHERE x>1 RETURN 10/(2-x) AS value } RETURN x,value"),
                insert_policy(2),
                |_| {
                    allocated += 1;
                    Ok::<_, Infallible>(ElementId::Vertex(VId(100 + allocated)))
                },
            )
            .expect_err("an arithmetic failure is not an empty optional invocation");
        assert_eq!(
            allocated, 2,
            "the failure follows both bodies' CREATE clauses"
        );
        assert_eq!(transaction.staged_effect_digest().unwrap(), before);
        assert_eq!(transaction.vertices(&db).unwrap().len(), 1);
        assert_eq!(
            transaction.vertex(&db, VId(7)).unwrap().unwrap().props,
            vec![(P, CanonicalScalar::Int(99).into())]
        );
        assert_eq!(db.frontier().unwrap(), basis);
        assert!(db.vertices().unwrap().is_empty());
        transaction
            .finish(&mut db, &contexts.commit())
            .await
            .unwrap();
        assert_eq!(db.vertices().unwrap().len(), 1);
        assert_eq!(contexts.txn().outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn optional_null_output_remains_cancellable_and_rechecks_authority_before_commit() {
    use fgdb_types::context::SimulationCheckpointProbe;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    let ((), report) = run_async_under_lab(0xca11_2006, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, txn, query) = (contexts.commit(), contexts.txn(), contexts.query());
        let issuer = authority();
        let token = issuer.issue_at(&grant(Rights::ReadWrite, 2), NOW).unwrap();
        let call = program(
            "UNWIND [1,2] AS x OPTIONAL CALL(x) { CREATE (:Visible {p:x}) WITH x WHERE false RETURN x AS value } RETURN x,value",
        );
        let observed = Arc::new(SimulationCheckpointProbe::new(None));
        let mut baseline = Database::open_memory(&commit, keys()).await.unwrap();
        baseline
            .execute_graph_write_chain_authorized(
                &txn,
                &query.with_checkpoint_probe(Arc::clone(&observed)),
                &commit,
                &issuer,
                &token,
                "main",
                &call,
                insert_policy(2),
                || NOW,
            )
            .await
            .unwrap();
        let calls = observed.calls();
        assert!(calls > 2);
        for stop in [calls / 2, calls - 1] {
            let probe = Arc::new(SimulationCheckpointProbe::new(Some(stop)));
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let basis = db.frontier().unwrap();
            db.execute_graph_write_chain_authorized(
                &txn,
                &query.with_checkpoint_probe(Arc::clone(&probe)),
                &commit,
                &issuer,
                &token,
                "main",
                &call,
                insert_policy(2),
                || NOW,
            )
            .await
            .expect_err("optional absence must not suppress cancellation");
            assert_eq!(db.frontier().unwrap(), basis);
            assert!(db.vertices().unwrap().is_empty());
        }

        let expired = Arc::new(AtomicBool::new(false));
        let clock = Arc::clone(&expired);
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let basis = db.frontier().unwrap();
        let error = db
            .execute_graph_write_chain_authorized_checked(
                &txn,
                &query,
                &commit,
                &issuer,
                &token,
                "main",
                &call,
                insert_policy(2),
                move || {
                    if clock.load(Ordering::Relaxed) {
                        10_000
                    } else {
                        NOW
                    }
                },
                |rows, _, _| {
                    assert_eq!(rows.len(), 2);
                    assert!(rows.iter().all(|row| row.values()[1].is_null()));
                    expired.store(true, Ordering::Relaxed);
                    Ok(())
                },
            )
            .await
            .unwrap_err();
        assert!(expired.load(Ordering::Relaxed));
        assert_eq!(authorization(&error), Some(Error::Expired));
        assert_eq!(db.frontier().unwrap(), basis);
        assert!(db.vertices().unwrap().is_empty());
        assert_eq!(txn.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
