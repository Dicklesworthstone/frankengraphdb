//! Native publication, not a model: a representation refusal must leave the
//! Chronicle frontier and graph unchanged, even after all RETURN rows exist.
use super::*;
use crate::recovery::tests::{fail_after_marker, served, settled};
use asupersync::lab::run_async_under_lab;
use fgdb_gql::GqlParameters;
use fgdb_types::CommitSeq;

fn accepted<T>(result: Result<T, Refusal>) -> T {
    result.unwrap_or_else(|error| panic!("{}: {}", error.code.name(), error.message))
}

fn prepare(db: &Served, text: &str) -> Returning {
    accepted(Returning::prepare(db, text, &GqlParameters::new())).expect("RETURN definition")
}

async fn state(cx: &Cx, db: &Served) -> (CommitSeq, usize) {
    let guard = db.db.read(cx).await.unwrap();
    (guard.frontier().unwrap(), guard.vertices().unwrap().len())
}

#[test]
fn representation_refusal_rolls_back_the_complete_native_write() {
    let ((), report) = run_async_under_lab(0xb017_1001, |root| async move {
        let (server, token, _) = served(&root, "returning-representation-refusal").await;
        let db = &server.databases["test"];
        let before = state(&root, db).await;
        let mut calls = 0;
        let result = write_returning_checked(
            &root,
            db,
            &token,
            prepare(
                db,
                "UNWIND [1,2,3] AS x CREATE (n) RETURN x AS value ORDER BY value",
            ),
            None,
            |columns, rows, _| {
                calls += 1;
                assert_eq!(columns, &[String::from("value")]);
                assert_eq!(
                    rows,
                    [
                        vec![WireValue::Int(1)],
                        vec![WireValue::Int(2)],
                        vec![WireValue::Int(3)],
                    ]
                );
                Err::<(), _>(Refusal::new(
                    ErrorCode::Execution,
                    "representation rejected",
                ))
            },
        )
        .await;
        let error = result
            .err()
            .expect("surface admission must abort the native write");
        assert_eq!(error.code, ErrorCode::Execution);
        assert_eq!(error.message, "representation rejected");
        assert_eq!(calls, 1);
        assert_eq!(state(&root, db).await, before);
        server.join_database_workers(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn chain_representation_refusal_rolls_back_all_prior_creations() {
    let ((), report) = run_async_under_lab(0xb017_1011, |root| async move {
        let (server, token, _) = served(&root, "chain-representation-refusal").await;
        let db = &server.databases["test"];
        let before = state(&root, db).await;
        let mut calls = 0;
        let result = write_returning_checked(
            &root,
            db,
            &token,
            prepare(db, "CREATE (a) WITH a CREATE (b) RETURN 7 AS value"),
            None,
            |columns, rows, _| {
                calls += 1;
                assert_eq!(columns, &[String::from("value")]);
                assert_eq!(rows, [vec![WireValue::Int(7)]]);
                Err::<(), _>(Refusal::new(ErrorCode::Execution, "chain output rejected"))
            },
        )
        .await;
        let error = result.err().expect("output admission must abort the chain");
        assert_eq!(error.message, "chain output rejected");
        assert_eq!(calls, 1);
        assert_eq!(state(&root, db).await, before);
        server.join_database_workers(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn converted_output_and_empty_output_both_precede_one_completion() {
    let ((), report) = run_async_under_lab(0xb017_1002, |root| async move {
        let (server, token, _) = served(&root, "returning-owned-representation").await;
        let db = &server.databases["test"];
        for (text, expected_rows) in [
            ("CREATE (n) RETURN 7 AS value", 1),
            ("CREATE (n) RETURN 7 AS value LIMIT 0", 0),
        ] {
            let (before_seq, before_count) = state(&root, db).await;
            let mut calls = 0;
            let answer = accepted(
                write_returning_checked(
                    &root,
                    db,
                    &token,
                    prepare(db, text),
                    None,
                    |columns, rows, work| {
                        calls += 1;
                        work(1).map_err(|error| write_refusal(&error))?;
                        assert_eq!(columns, &[String::from("value")]);
                        assert_eq!(rows.len(), expected_rows);
                        // A different owned representation proves that adapters
                        // need not convert/lookup a cell after the commit.
                        Ok(rows
                            .into_iter()
                            .map(|_| String::from("seven"))
                            .collect::<Vec<_>>())
                    },
                )
                .await,
            );
            assert_eq!(calls, 1, "empty output still requires schema admission");
            assert_eq!(answer.rows, vec![String::from("seven"); expected_rows]);
            assert_eq!(answer.columns, vec![String::from("value")]);
            assert_eq!(
                answer.outcome,
                Outcome::WriteCommitted {
                    seq: before_seq.0 + 1,
                    statements: 1
                },
            );
            assert_eq!(
                state(&root, db).await,
                (CommitSeq(before_seq.0 + 1), before_count + 1),
            );
        }
        server.join_database_workers(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn representation_work_uses_the_native_signed_allowance() {
    let ((), report) = run_async_under_lab(0xb017_1003, |root| async move {
        let (server, token, _) = served(&root, "returning-representation-work").await;
        let db = &server.databases["test"];
        let before = state(&root, db).await;
        let mut charged = false;
        let result = write_returning_checked(
            &root,
            db,
            &token,
            prepare(db, "CREATE (n) RETURN 7 AS value"),
            None,
            |_, _, work| {
                charged = true;
                work(u64::MAX).map_err(|error| write_refusal(&error))
            },
        )
        .await;
        assert!(charged);
        assert!(result.is_err());
        assert_eq!(state(&root, db).await, before);
        server.join_database_workers(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn deferred_returning_checks_its_floor_and_never_crosses_recovery_generations() {
    let ((), report) = run_async_under_lab(0xb017_1004, |root| async move {
        let (server, token, _) = served(&root, "returning-deferred-fence").await;
        let db = &server.databases["test"];
        let generation = db.db.generation().unwrap();
        let result = write_returning_checked(
            &root,
            db,
            &token,
            prepare(db, "CREATE (n) RETURN 7 AS value"),
            Some((&generation, CommitSeq(1))),
            |_, _, _| -> Result<(), Refusal> { panic!("unmet floor must not reach output") },
        )
        .await;
        assert_eq!(result.err().expect("unmet floor").code, ErrorCode::Busy);
        assert_eq!(state(&root, db).await, (CommitSeq(0), 0));
        let definition = prepare(db, "CREATE (n) RETURN 7 AS value");
        fail_after_marker(&root, db, 42).await;
        assert!(matches!(
            settled(db).await,
            crate::DatabaseStatus::Ready { generation: 2 },
        ));
        let before = state(&root, db).await;
        let result = write_returning_checked(
            &root,
            db,
            &token,
            definition,
            Some((&generation, CommitSeq(0))),
            |_, _, _| -> Result<(), Refusal> { panic!("stale generation must not execute") },
        )
        .await;
        assert_eq!(
            result.err().expect("stale generation").code,
            ErrorCode::DatabaseRecovering,
        );
        assert_eq!(state(&root, db).await, before);
        server.join_database_workers(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn retained_returning_definitions_are_not_authority_or_an_identity_bypass() {
    let ((), report) = run_async_under_lab(0xb017_1005, |root| async move {
        let (server, token, _) = served(&root, "returning-retired-definition").await;
        let db = &server.databases["test"];
        for text in ["CREATE (n) RETURN id(n)", "CREATE (n) RETURN elementId(n)"] {
            let error = Returning::prepare(db, text, &GqlParameters::new())
                .err()
                .expect("identity fence");
            assert_eq!(error.code, ErrorCode::PermissionDenied);
        }
        let definition = prepare(db, "CREATE (n) RETURN 7 AS value");
        db.authority.retire();
        let result = write_returning_checked(
            &root,
            db,
            &token,
            definition,
            None,
            |_, _, _| -> Result<(), Refusal> { panic!("retired authority must not produce rows") },
        )
        .await;
        assert!(result.is_err());
        assert_eq!(state(&root, db).await, (CommitSeq(0), 0));
        server.join_database_workers(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
