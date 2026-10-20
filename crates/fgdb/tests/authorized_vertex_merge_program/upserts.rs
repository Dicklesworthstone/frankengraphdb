//! All-target ON/SET through the ordinary authorized program boundary.
use super::*;
use fgdb_gql::{
    GraphMutationProgramDimension, GraphMutationProgramError, PreparedGraphVertexUpsertText,
};

fn upsert(text: &str) -> GraphWriteStatement {
    PreparedGraphVertexUpsertText::prepare(text, R, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
        .into()
}

fn value(db: &Database<MemVfs>, vertex: VId, key: PropertyKeyId) -> CanonicalScalar {
    db.vertex(vertex)
        .unwrap()
        .unwrap()
        .props
        .into_iter()
        .find_map(|(candidate, value)| (candidate == key).then_some(value))
        .map_or(CanonicalScalar::Null, |value| {
            value.as_scalar().expect("scalar property").clone()
        })
}

fn changed_key_program() -> PreparedGraphWriteProgram {
    PreparedGraphWriteProgram::prepare(vec![
        upsert(
            "MERGE (n {key:1}) ON CREATE SET n.secret=0 ON MATCH SET n.value=n.value+1 SET n.key=2",
        ),
        upsert("MERGE (n {key:2}) ON MATCH SET n.value=n.value+1"),
    ])
    .unwrap()
}

#[test]
fn all_targets_keep_actions_after_key_changes_and_later_steps_see_their_values() {
    run(async |commit, query, txn| {
        for returning in [false, true] {
            let (mut db, authority) = fixture(commit).await;
            let before = db.frontier().unwrap();
            let hidden = db.vertex(HIDDEN_ID).unwrap();
            let token = authority
                .issue_at(&grant(if returning { 4 } else { 0 }), NOW)
                .unwrap();
            let (receipt, end) = execute(
                &mut db,
                commit,
                query,
                txn,
                &authority,
                &token,
                &changed_key_program(),
                policy(),
                returning,
            )
            .await
            .unwrap();
            assert!(matches!(end, EmbeddedTxnCompletion::WriteCommitted { .. }));
            assert_eq!(db.frontier().unwrap().0, before.0 + 1);
            let stats = receipt.stats();
            assert_eq!(
                (
                    stats.completed_statements,
                    stats.created_vertices,
                    stats.mutation_effects,
                    stats.target_vertex_visits
                ),
                (2, 0, 6, 4)
            );
            assert_eq!(stats.selection.result_rows, 4);
            for (id, expected) in [(FIRST, 12), (SECOND, 22)] {
                assert_eq!(value(&db, id, KEY), CanonicalScalar::Int(2));
                assert_eq!(value(&db, id, VALUE), CanonicalScalar::Int(expected));
                assert_eq!(value(&db, id, SECRET), CanonicalScalar::Int(99));
            }
            assert_eq!(db.vertex(HIDDEN_ID).unwrap(), hidden);
            if returning {
                assert_eq!(receipt.steps().len(), 2);
                for step in receipt.steps() {
                    assert!(matches!(
                        step,
                        GraphWriteStepReceipt::VertexUpsertBatch { .. }
                    ));
                    assert_eq!(
                        step.merged_vertices(),
                        Some(
                            &[
                                GraphVertexMergeOutcome::Matched(FIRST),
                                GraphVertexMergeOutcome::Matched(SECOND),
                            ][..]
                        )
                    );
                    assert_eq!(step.created_vertices(), Some(&[][..]));
                }
            }
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn action_budget_counts_every_target_and_both_clauses_across_statements() {
    run(async |commit, query, txn| {
        for returning in [false, true] {
            for effects in [3, 4, 5, 6] {
                let (mut db, authority) = fixture(commit).await;
                let before = (
                    db.frontier().unwrap(),
                    db.vertices().unwrap(),
                    db.edges().unwrap(),
                );
                let token = authority
                    .issue_at(&grant(if returning { 4 } else { 0 }), NOW)
                    .unwrap();
                let bounded =
                    GraphWriteProgramPolicy::new(policy().mutations.query, effects, 100, 100);
                let result = execute(
                    &mut db,
                    commit,
                    query,
                    txn,
                    &authority,
                    &token,
                    &changed_key_program(),
                    bounded,
                    returning,
                )
                .await;
                if effects < 6 {
                    let expected_total = if effects < 4 { 4 } else { 6 };
                    let failed_step = usize::from(effects >= 4);
                    assert!(
                        matches!(result, Err(Fault::Program(GraphMutationProgramError::Budget {
                        dimension: GraphMutationProgramDimension::Effects, limit, observed, statement,
                    })) if limit == effects && observed == expected_total && statement == failed_step)
                    );
                    assert_eq!(
                        (
                            db.frontier().unwrap(),
                            db.vertices().unwrap(),
                            db.edges().unwrap()
                        ),
                        before
                    );
                } else {
                    assert_eq!(result.unwrap().0.stats().mutation_effects, 6);
                    assert_eq!(value(&db, SECOND, VALUE), CanonicalScalar::Int(22));
                }
                assert_eq!(txn.outstanding_obligations(), 0);
            }
        }
    });
}

#[test]
fn late_target_failure_and_forbidden_noop_erase_the_entire_unpublished_program() {
    run(async |commit, query, txn| {
        for returning in [false, true] {
            for text in [
                // FIRST changes from 10 to 20; SECOND then divides by zero.
                "MERGE (n {key:1}) ON MATCH SET n.value=200/(20-n.value)",
                // Reading a hidden property is masked; writing it is forbidden,
                // even though the RHS is masked and/or the effect normalizes away.
                "MERGE (n {key:1}) SET n.value=77, n.secret=n.secret",
            ] {
                let (mut db, authority) = fixture(commit).await;
                let before = (
                    db.frontier().unwrap(),
                    db.vertices().unwrap(),
                    db.edges().unwrap(),
                );
                let token = authority
                    .issue_at(&grant(if returning { 3 } else { 0 }), NOW)
                    .unwrap();
                let program = PreparedGraphWriteProgram::prepare(vec![
                    insert("CREATE (n:Item {key:9})"),
                    upsert(text),
                ])
                .unwrap();
                let error = execute(
                    &mut db,
                    commit,
                    query,
                    txn,
                    &authority,
                    &token,
                    &program,
                    policy(),
                    returning,
                )
                .await
                .unwrap_err();
                assert!(matches!(error, Fault::VertexUpsert { statement: 1, .. }));
                assert_eq!(
                    (
                        db.frontier().unwrap(),
                        db.vertices().unwrap(),
                        db.edges().unwrap()
                    ),
                    before
                );
                assert_eq!(txn.outstanding_obligations(), 0);
                // A failed program leaves no pin/workspace preventing a retry.
                let valid = PreparedGraphWriteProgram::prepare(vec![upsert(
                    "MERGE (n {key:1}) SET n.value=n.value+1",
                )])
                .unwrap();
                execute(
                    &mut db,
                    commit,
                    query,
                    txn,
                    &authority,
                    &token,
                    &valid,
                    policy(),
                    returning,
                )
                .await
                .unwrap();
                assert_eq!(value(&db, FIRST, VALUE), CanonicalScalar::Int(11));
                assert_eq!(value(&db, SECOND, VALUE), CanonicalScalar::Int(21));
                assert_eq!(value(&db, HIDDEN_ID, VALUE), CanonicalScalar::Int(900));
            }
        }
    });
}

#[test]
fn hidden_reads_and_actionless_branches_do_not_become_hidden_writes_or_visits() {
    run(async |commit, query, txn| {
        for returning in [false, true] {
            let (mut db, authority) = fixture(commit).await;
            let token = authority
                .issue_at(&grant(if returning { 4 } else { 0 }), NOW)
                .unwrap();
            let before = (db.frontier().unwrap(), db.vertices().unwrap());
            let actionless = PreparedGraphWriteProgram::prepare(vec![upsert(
                "MERGE (n {key:1}) ON CREATE SET n.secret=1",
            )])
            .unwrap();
            let (receipt, end) = execute(
                &mut db,
                commit,
                query,
                txn,
                &authority,
                &token,
                &actionless,
                policy(),
                returning,
            )
            .await
            .unwrap();
            assert!(matches!(end, EmbeddedTxnCompletion::ReadClosed { .. }));
            assert_eq!(receipt.stats().mutation_effects, 0);
            assert_eq!(receipt.stats().target_vertex_visits, 0);
            assert_eq!((db.frontier().unwrap(), db.vertices().unwrap()), before);
            let masked = PreparedGraphWriteProgram::prepare(vec![upsert(
                "MERGE (n {key:1}) SET n.value=coalesce(n.secret,5)",
            )])
            .unwrap();
            let (receipt, _) = execute(
                &mut db,
                commit,
                query,
                txn,
                &authority,
                &token,
                &masked,
                policy(),
                returning,
            )
            .await
            .unwrap();
            assert_eq!(receipt.stats().target_vertex_visits, 2);
            for id in [FIRST, SECOND] {
                assert_eq!(value(&db, id, VALUE), CanonicalScalar::Int(5));
                assert_eq!(value(&db, id, SECRET), CanonicalScalar::Int(99));
            }
            assert_eq!(value(&db, HIDDEN_ID, VALUE), CanonicalScalar::Int(900));
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn upsert_receipt_rows_accumulate_but_stats_only_runs_on_zero_rows() {
    run(async |commit, query, txn| {
        for rows in [0, 2, 3, 4] {
            let (mut db, authority) = fixture(commit).await;
            let before = (db.frontier().unwrap(), db.vertices().unwrap());
            let token = authority.issue_at(&grant(rows), NOW).unwrap();
            let result = execute(
                &mut db,
                commit,
                query,
                txn,
                &authority,
                &token,
                &changed_key_program(),
                policy(),
                true,
            )
            .await;
            if rows < 4 {
                assert!(result.is_err());
                assert_eq!((db.frontier().unwrap(), db.vertices().unwrap()), before);
            } else {
                assert_eq!(result.unwrap().0.steps().len(), 2);
            }
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn expiry_after_all_actions_still_precedes_the_only_publication() {
    use std::sync::atomic::{AtomicU64, Ordering};
    run(async |commit, query, txn| {
        let (mut db, authority) = fixture(commit).await;
        let token = authority.issue_at(&grant(4), NOW).unwrap();
        let program = changed_key_program();
        let calls = AtomicU64::new(0);
        db.execute_graph_write_program_returning_authorized(
            txn,
            query,
            commit,
            &authority,
            &token,
            "main",
            &program,
            policy(),
            || {
                calls.fetch_add(1, Ordering::Relaxed);
                NOW
            },
        )
        .await
        .unwrap();
        let count = calls.load(Ordering::Relaxed);
        assert!(count > 10);
        let (mut db, authority) = fixture(commit).await;
        let token = authority.issue_at(&grant(4), NOW).unwrap();
        let before = (
            db.frontier().unwrap(),
            db.vertices().unwrap(),
            db.edges().unwrap(),
        );
        let calls = AtomicU64::new(0);
        assert!(
            db.execute_graph_write_program_returning_authorized(
                txn,
                query,
                commit,
                &authority,
                &token,
                "main",
                &program,
                policy(),
                || {
                    calls.fetch_add(1, Ordering::Relaxed);
                    if calls.load(Ordering::Relaxed) < count {
                        NOW
                    } else {
                        10_000
                    }
                },
            )
            .await
            .is_err()
        );
        assert_eq!(
            calls.load(Ordering::Relaxed),
            count,
            "refusal occurs at the final live check"
        );
        assert_eq!(
            (
                db.frontier().unwrap(),
                db.vertices().unwrap(),
                db.edges().unwrap()
            ),
            before
        );
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}

#[test]
fn text_sessions_and_parameter_batches_use_the_plural_path_and_one_completion() {
    run(async |commit, query, txn| {
        for prepared in [false, true] {
            let (mut db, authority) = fixture(commit).await;
            let before = db.frontier().unwrap();
            let hidden = db.vertex(HIDDEN_ID).unwrap();
            let token = authority.issue_at(&grant(0), NOW).unwrap();
            let args = |key, increment| {
                GqlParameters::new()
                    .with_int64("key", key)
                    .unwrap()
                    .with_int64("increment", increment)
                    .unwrap()
            };
            let text = "MERGE (n {key:$key}) ON MATCH SET n.value=n.value+$increment";
            let batch = [args(1, 2), args(1, 3)];
            let mut session = db
                .authorized_write_session(
                    txn,
                    commit,
                    &authority,
                    &token,
                    "main",
                    symbols,
                    R,
                    policy(),
                    64,
                    || NOW,
                )
                .unwrap();
            let (stats, end) = if prepared {
                let template = session.prepare(query, text, &batch[0]).unwrap();
                session
                    .execute_batch_stats(query, &template, &batch)
                    .await
                    .unwrap()
            } else {
                session
                    .query_batch_stats(query, text, &batch)
                    .await
                    .unwrap()
            };
            assert_eq!(
                (
                    stats.completed_statements,
                    stats.mutation_effects,
                    stats.target_vertex_visits,
                    stats.created_vertices
                ),
                (2, 4, 4, 0)
            );
            assert!(matches!(end, EmbeddedTxnCompletion::WriteCommitted { .. }));
            assert!(!session.is_closed());
            // A subsequent text command sees the whole committed result, not
            // an earlier parameter row or a fresh subset of the original target.
            let (next, _) = session.query_stats(query, text, &args(1, 1)).await.unwrap();
            assert_eq!((next.mutation_effects, next.target_vertex_visits), (2, 2));
            drop(session);
            assert_eq!(db.frontier().unwrap().0, before.0 + 2);
            assert_eq!(value(&db, FIRST, VALUE), CanonicalScalar::Int(16));
            assert_eq!(value(&db, SECOND, VALUE), CanonicalScalar::Int(26));
            assert_eq!(db.vertex(HIDDEN_ID).unwrap(), hidden);
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn failed_second_parameter_record_closes_the_session_without_committing_the_first() {
    run(async |commit, query, txn| {
        let (mut db, authority) = fixture(commit).await;
        let before = (
            db.frontier().unwrap(),
            db.vertices().unwrap(),
            db.edges().unwrap(),
        );
        let token = authority.issue_at(&grant(0), NOW).unwrap();
        let arguments =
            [1, 0].map(|divisor| GqlParameters::new().with_int64("divisor", divisor).unwrap());
        let mut session = db
            .authorized_write_session(
                txn,
                commit,
                &authority,
                &token,
                "main",
                symbols,
                R,
                policy(),
                64,
                || NOW,
            )
            .unwrap();
        let error = session
            .query_batch_stats(
                query,
                "MERGE (n {key:1}) SET n.value=n.value/$divisor+1",
                &arguments,
            )
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            fgdb_gql::GraphWriteScriptExecutionError::BatchProgram { .. }
        ));
        assert!(session.is_closed());
        assert!(
            session
                .query_stats(query, "MERGE (n {key:1})", &GqlParameters::new())
                .await
                .is_err()
        );
        drop(session);
        assert_eq!(
            (
                db.frontier().unwrap(),
                db.vertices().unwrap(),
                db.edges().unwrap()
            ),
            before
        );
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}

#[test]
fn source_and_action_evaluator_limits_never_restart_per_target_or_step() {
    run(async |commit, query, txn| {
        for returning in [false, true] {
            let (mut db, authority) = fixture(commit).await;
            let token = authority
                .issue_at(&grant(if returning { 4 } else { 0 }), NOW)
                .unwrap();
            let program = changed_key_program();
            let (receipt, _) = execute(
                &mut db,
                commit,
                query,
                txn,
                &authority,
                &token,
                &program,
                policy(),
                returning,
            )
            .await
            .unwrap();
            let used = receipt.stats();
            let limits = [
                used.selection.snapshot_records,
                used.selection.result_rows,
                used.evaluator.work_units,
                used.evaluator.scratch_entries,
            ];
            assert!(limits.iter().all(|&limit| limit > 0));
            for reduced in 0..=4 {
                let (mut db, authority) = fixture(commit).await;
                let before = (
                    db.frontier().unwrap(),
                    db.vertices().unwrap(),
                    db.edges().unwrap(),
                );
                let token = authority
                    .issue_at(&grant(if returning { 4 } else { 0 }), NOW)
                    .unwrap();
                let mut allowed = limits;
                if reduced < 4 {
                    allowed[reduced] -= 1;
                }
                let [records, rows, work, scratch] = allowed;
                let bounded = GraphWriteProgramPolicy::new(
                    GqlQueryPolicy::new(records, rows, work, scratch),
                    6,
                    0,
                    0,
                );
                let result = execute(
                    &mut db, commit, query, txn, &authority, &token, &program, bounded, returning,
                )
                .await;
                assert_eq!(
                    result.is_ok(),
                    reduced == 4,
                    "dimension {reduced}, receipts {returning}"
                );
                if reduced < 4 {
                    assert_eq!(
                        (
                            db.frontier().unwrap(),
                            db.vertices().unwrap(),
                            db.edges().unwrap()
                        ),
                        before
                    );
                } else {
                    assert_eq!(value(&db, FIRST, VALUE), CanonicalScalar::Int(12));
                    assert_eq!(value(&db, SECOND, VALUE), CanonicalScalar::Int(22));
                }
                assert_eq!(txn.outstanding_obligations(), 0);
            }
        }
    });
}
