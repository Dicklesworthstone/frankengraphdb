//! Real native preparation and execution; the cache owns no database pin.

use super::*;
use crate::execute::{prepare_read, read_prepared};
use crate::recovery::tests::{fail_after_marker, served, settled, vertex};
use asupersync::lab::run_async_under_lab;
use fgdb_protocol::body::PrepareMode;
use fgdb_types::{CommitSeq, PurposeContexts};

fn ok<T>(result: Result<T, Refusal>) -> T {
    result.unwrap_or_else(|error| panic!("{}: {}", error.code.name(), error.message))
}

#[test]
fn prepared_map_fields_rebind_across_native_delivery_modes_and_recovery() {
    let ((), report) = run_async_under_lab(0x79a0_0441, |root| async move {
        let (mut server, token, path) = served(&root, "prepared-map-fields").await;
        let directory = path.join("map-fields-scratch");
        asupersync::fs::create_dir(&directory).await.unwrap();
        let config = crate::OrderedReadConfig::new(&directory);
        let ordered = crate::ordered::OrderedReads::open(&root, config)
            .await
            .unwrap();
        let db = std::sync::Arc::get_mut(server.databases.get_mut("test").unwrap()).unwrap();
        db.symbols
            .bind(fgdb_gql::GraphSymbolKind::Property, "p", 1)
            .unwrap();
        db.ordered_reads = Some(ordered);
        let db = &server.databases["test"];
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let query = contexts.query();
        let key = fgdb_delta_types::PropertyKeyId(1);
        let mut batch = fgdb::WriteBatch::new(fgdb_delta_types::RelationId(1));
        for (id, value) in [(1, 10), (2, 30)] {
            batch.create_vertex(
                fgdb_types::VId(id),
                vec![],
                vec![(key, fgdb_types::CanonicalScalar::Int(value))],
            );
        }
        db.db
            .write(&root)
            .await
            .unwrap()
            .write(&contexts.commit(), batch)
            .await
            .unwrap();
        let arguments = |value: Option<i64>| {
            vec![(
                "m".into(),
                WireValue::Map(
                    value
                        .map(|value| vec![("key".into(), WireValue::Int(value))])
                        .unwrap_or_default(),
                ),
            )]
        };
        let text = "MATCH (n {p: $m.key}) RETURN n.p AS p";
        let definition = ok(prepare_read(
            &root,
            db,
            &token,
            &Prepare {
                mode: PrepareMode::Read,
                statement: text.into(),
                parameters: arguments(Some(10)),
            },
        )
        .await);
        for value in [Some(30), Some(10), None] {
            let parameters = arguments(value);
            let expected = value
                .map(|value| vec![vec![WireValue::Int(value)]])
                .unwrap_or_default();
            assert_eq!(
                ok(read_prepared(&root, db, &token, &definition, &parameters).await).rows,
                expected,
            );
            let parameters = crate::convert::parameters(&parameters, None).unwrap();
            let (mut session, template) =
                ok(crate::execute::read_prepared_session(&root, db, &token, &definition).await);
            let parameters = ok(definition.arguments(&query, db, &token, &parameters));
            let mut cursor = ok(session.stream_prepared(&query, &template, &parameters));
            let mut rows = Vec::new();
            while let Some(row) = ok(cursor.next().await) {
                rows.push(row);
            }
            assert_eq!(rows, expected);
        }
        assert_eq!(
            read_prepared(&root, db, &token, &definition, &[])
                .await
                .err()
                .unwrap()
                .code,
            ErrorCode::Statement,
        );
        let diff = ok(crate::execute::read_prepared_difference(
            &root,
            db,
            &token,
            &definition,
            &arguments(Some(30)),
            0,
            1,
        )
        .await);
        assert_eq!(diff.entries, [(1, vec![WireValue::Int(30)])]);

        let ordered = ok(prepare_read(
            &root,
            db,
            &token,
            &Prepare {
                mode: PrepareMode::Read,
                statement: format!("{text} ORDER BY p"),
                parameters: arguments(Some(10)),
            },
        )
        .await);
        let generation = ok(crate::execute::ordered::ordered_generation(db, &token));
        let (mut session, template) = ok(crate::execute::ordered::read_ordered_session(
            &root,
            db,
            &token,
            Some(&ordered),
            generation,
        )
        .await);
        let template = template.unwrap();
        let rebound = crate::convert::parameters(&arguments(Some(30)), None).unwrap();
        let rebound = ok(ordered.arguments(&query, db, &token, &rebound));
        {
            let execution = ok(session.start(&query, &template, &rebound));
            let mut cursor = ok(execution.finish().await);
            assert_eq!(ok(cursor.next().await), Some(vec![WireValue::Int(30)]));
            assert!(ok(cursor.next().await).is_none());
        }
        {
            let execution = ok(session.start_difference(&query, &template, &rebound, 0, 1));
            let mut cursor = ok(execution.finish().await);
            assert_eq!(ok(cursor.next().await), Some((1, vec![WireValue::Int(30)])));
            assert!(ok(cursor.next().await).is_none());
        }
        drop(session);

        let mut update = fgdb::WriteBatch::new(fgdb_delta_types::RelationId(1));
        update.set_vertex_property(
            fgdb_types::VId(2),
            key,
            Some(fgdb_types::CanonicalScalar::Int(300)),
        );
        db.db
            .write(&root)
            .await
            .unwrap()
            .write(&contexts.commit(), update)
            .await
            .unwrap();
        let answer = ok(read_prepared(&root, db, &token, &definition, &arguments(Some(300))).await);
        assert_eq!(answer.outcome, Outcome::Rows { seq: 2 });
        assert_eq!(answer.rows, [vec![WireValue::Int(300)]]);
        fail_after_marker(&root, db, 99).await;
        assert_eq!(
            settled(db).await,
            crate::DatabaseStatus::Ready { generation: 2 }
        );
        assert_eq!(
            read_prepared(&root, db, &token, &definition, &arguments(Some(300)))
                .await
                .err()
                .unwrap()
                .code,
            ErrorCode::DatabaseRecovering,
        );
        assert!(db.authority.retire());
        assert_eq!(
            definition
                .arguments(&query, db, &token, &fgdb_gql::GqlParameters::new())
                .err()
                .unwrap()
                .code,
            ErrorCode::PermissionDenied,
            "retired authority wins over map binding and old generation",
        );
        server.join_database_workers(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn a_prepared_read_rebinds_operands_and_observes_commits_after_preparation() {
    let ((), report) = run_async_under_lab(0x79a0_0401, |root| async move {
        let (server, token, _) = served(&root, "prepared-live-frontier").await;
        let db = &server.databases["test"];
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let mut cache = PreparedStatements::default();
        let definition = ok(prepare_read(
            &root,
            db,
            &token,
            &Prepare {
                mode: PrepareMode::Read,
                statement: "MATCH (n) RETURN $value AS value".into(),
                parameters: vec![("value".into(), WireValue::Int(7))],
            },
        )
        .await);
        let (handle, _) = ok(cache.insert(&root, definition));
        let definition = ok(cache.get(handle));
        let definition = definition.read().unwrap();
        let first = ok(read_prepared(
            &root,
            db,
            &token,
            definition,
            &[("value".into(), WireValue::Int(11))],
        )
        .await);
        assert_eq!(first.outcome, Outcome::Rows { seq: 0 });
        assert!(first.rows.is_empty());
        let mut batch = vertex(1);
        batch.create_vertex(fgdb_types::VId(2), vec![], vec![]);
        assert_eq!(
            db.db
                .write(&root)
                .await
                .unwrap()
                .write(&contexts.commit(), batch)
                .await
                .unwrap(),
            CommitSeq(1)
        );
        let second = ok(read_prepared(
            &root,
            db,
            &token,
            definition,
            &[("value".into(), WireValue::Int(19))],
        )
        .await);
        assert_eq!(second.columns, ["value"]);
        assert_eq!(
            second.rows,
            [vec![WireValue::Int(19)], vec![WireValue::Int(19)]]
        );
        assert_eq!(second.outcome, Outcome::Rows { seq: 1 });
        assert_eq!(
            read_prepared(&root, db, &token, definition, &[])
                .await
                .err()
                .unwrap()
                .code,
            ErrorCode::Statement,
            "representative values are not defaults"
        );
        let after_refusal = ok(read_prepared(
            &root,
            db,
            &token,
            definition,
            &[("value".into(), WireValue::Int(23))],
        )
        .await);
        assert_eq!(
            after_refusal.rows,
            [vec![WireValue::Int(23)], vec![WireValue::Int(23)]]
        );
        assert!(
            prepare_read(
                &root,
                db,
                &token,
                &Prepare {
                    mode: PrepareMode::Read,
                    statement: "CREATE ()".into(),
                    parameters: vec![]
                },
            )
            .await
            .is_err()
        );
        assert_eq!(
            db.db.read(&root).await.unwrap().frontier().unwrap(),
            CommitSeq(1)
        );
        server.join_database_workers(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn prepared_cache_is_bounded_and_foreign_release_cannot_remove_another_owner() {
    let ((), report) = run_async_under_lab(0x79a0_0402, |root| async move {
        let (server, token, _) = served(&root, "prepared-cache-budget").await;
        let db = &server.databases["test"];
        let request = Prepare {
            mode: PrepareMode::Read,
            statement: "MATCH (n) RETURN n".into(),
            parameters: vec![],
        };
        let mut cache = PreparedStatements::default();
        let mut handles = Vec::new();
        for _ in 0..MAX_PREPARED_STATEMENTS {
            let definition = ok(prepare_read(&root, db, &token, &request).await);
            handles.push(ok(cache.insert(&root, definition)).0);
        }
        assert_eq!(cache.entries.len(), MAX_PREPARED_STATEMENTS);
        assert_eq!(
            cache.source_bytes,
            MAX_PREPARED_STATEMENTS * request.statement.len()
        );
        let overflow = ok(prepare_read(&root, db, &token, &request).await);
        assert_eq!(
            cache.insert(&root, overflow).err().unwrap().code,
            ErrorCode::Budget
        );
        let mut foreign = PreparedStatements::default();
        let foreign_error = foreign.get(handles[0]).err().unwrap();
        foreign.release(handles[0]);
        assert!(cache.get(handles[0]).is_ok());
        cache.release(handles[0]);
        cache.release(handles[0]);
        let released_error = cache.get(handles[0]).err().unwrap();
        let unknown_error = cache.get(PreparedHandle([0x7f; 16])).err().unwrap();
        assert_eq!(foreign_error.code, released_error.code);
        assert_eq!(foreign_error.message, released_error.message);
        assert_eq!(unknown_error.message, released_error.message);
        assert_eq!(cache.entries.len(), MAX_PREPARED_STATEMENTS - 1);
        assert_eq!(
            cache.source_bytes,
            (MAX_PREPARED_STATEMENTS - 1) * request.statement.len()
        );
        let replacement = ok(prepare_read(&root, db, &token, &request).await);
        let replacement = ok(cache.insert(&root, replacement)).0;
        assert_ne!(replacement, handles[0]);
        assert_eq!(cache.entries.len(), MAX_PREPARED_STATEMENTS);
        server.join_database_workers(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn prepared_handles_and_borrowed_templates_never_revive_after_recovery() {
    let ((), report) = run_async_under_lab(0x79a0_0403, |root| async move {
        let (server, token, _) = served(&root, "prepared-recovery-fence").await;
        let db = &server.databases["test"];
        let request = Prepare {
            mode: PrepareMode::Read,
            statement: "MATCH (n) RETURN n".into(),
            parameters: vec![],
        };
        let mut cache = PreparedStatements::default();
        let definition = ok(prepare_read(&root, db, &token, &request).await);
        let (old_handle, _) = ok(cache.insert(&root, definition));
        let retained = ok(cache.get(old_handle));
        let retained = retained.read().unwrap();
        fail_after_marker(&root, db, 99).await;
        assert_eq!(
            settled(db).await,
            crate::DatabaseStatus::Ready { generation: 2 }
        );
        assert_eq!(
            read_prepared(&root, db, &token, retained, &[])
                .await
                .err()
                .unwrap()
                .code,
            ErrorCode::DatabaseRecovering
        );
        assert!(cache.get(old_handle).is_err());
        assert!(cache.entries.is_empty());
        assert_eq!(cache.source_bytes, 0);
        let definition = ok(prepare_read(&root, db, &token, &request).await);
        let (new_handle, _) = ok(cache.insert(&root, definition));
        let definition = ok(cache.get(new_handle));
        let definition = definition.read().unwrap();
        let answer = ok(read_prepared(&root, db, &token, definition, &[]).await);
        assert_eq!(answer.rows, [vec![WireValue::Vertex(99)]]);
        assert!(cache.get(old_handle).is_err());
        assert!(db.authority.retire());
        assert!(
            read_prepared(&root, db, &token, definition, &[])
                .await
                .is_err()
        );
        server.join_database_workers(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn prepared_write_templates_cannot_outlive_recovery_or_issuer_retirement() {
    let ((), report) = run_async_under_lab(0x79a0_0421, |root| async move {
        let (server, token, _) = served(&root, "prepared-write-recovery").await;
        let db = &server.databases["test"];
        let request = Prepare {
            mode: PrepareMode::Write,
            statement: "CREATE ()".into(),
            parameters: vec![],
        };
        let argument_sets = [vec![], vec![]];
        let mut cache = PreparedStatements::default();
        let definition = ok(prepare_statement(&root, db, &token, &request).await);
        let (handle, _) = ok(cache.insert(&root, definition));
        let retained = ok(cache.get(handle));
        assert!(retained.read().is_none());
        assert_eq!(
            db.db.read(&root).await.unwrap().frontier().unwrap(),
            CommitSeq(0)
        );
        fail_after_marker(&root, db, 99).await;
        assert_eq!(
            settled(db).await,
            crate::DatabaseStatus::Ready { generation: 2 }
        );
        assert_eq!(
            run_prepared(
                &root,
                db,
                &token,
                &retained,
                PreparedExecutionMode::Write,
                &[]
            )
            .await
            .err()
            .unwrap()
            .code,
            ErrorCode::DatabaseRecovering
        );
        assert_eq!(
            run_prepared_batch(&root, db, &token, &retained, &argument_sets)
                .await
                .err()
                .unwrap()
                .code,
            ErrorCode::DatabaseRecovering,
            "every record retains the prepared handle's original generation fence"
        );
        assert_eq!(
            db.db.read(&root).await.unwrap().frontier().unwrap(),
            CommitSeq(1),
            "the stale template publishes no single write or batch prefix"
        );
        assert!(cache.get(handle).is_err());
        assert_eq!(cache.source_bytes, 0);
        let definition = ok(prepare_statement(&root, db, &token, &request).await);
        assert!(db.authority.retire());
        assert_eq!(
            run_prepared(
                &root,
                db,
                &token,
                &definition,
                PreparedExecutionMode::Write,
                &[]
            )
            .await
            .err()
            .unwrap()
            .code,
            ErrorCode::PermissionDenied
        );
        assert_eq!(
            run_prepared_batch(&root, db, &token, &definition, &argument_sets)
                .await
                .err()
                .unwrap()
                .code,
            ErrorCode::PermissionDenied,
            "a prepared batch must acquire fresh authority for this invocation"
        );
        assert_eq!(
            db.db.read(&root).await.unwrap().frontier().unwrap(),
            CommitSeq(1)
        );
        server.join_database_workers(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn prepared_sources_rebind_without_reparsing_and_pin_one_execution_frontier() {
    let ((), report) = run_async_under_lab(0x79a0_0411, |root| async move {
        let (server, token, _) = served(&root, "prepared-source-frontier").await;
        let db = &server.databases["test"];
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let mut batch = vertex(1);
        batch.create_vertex(fgdb_types::VId(2), vec![], vec![]);
        db.db
            .write(&root)
            .await
            .unwrap()
            .write(&contexts.commit(), batch)
            .await
            .unwrap();
        let definition = ok(prepare_read(
            &root,
            db,
            &token,
            &Prepare {
                mode: PrepareMode::Read,
                statement: "MATCH (n) RETURN n AS id, $value AS value".into(),
                parameters: vec![("value".into(), WireValue::Int(7))],
            },
        )
        .await);
        let query = contexts.query();
        let first_parameters = fgdb_gql::GqlParameters::new()
            .with_int64("value", 11)
            .unwrap();
        let (mut session, prepared) =
            ok(crate::execute::read_prepared_session(&root, db, &token, &definition).await);
        let mut source = ok(session.stream_prepared(&query, &prepared, &first_parameters));
        assert_eq!(source.snapshot_seq(), 1);
        assert_eq!(
            ok(source.next().await),
            Some(vec![WireValue::Vertex(1), WireValue::Int(11)])
        );
        // The cursor has no writer lock and remains on its first snapshot.
        db.db
            .write(&root)
            .await
            .unwrap()
            .write(&contexts.commit(), vertex(3))
            .await
            .unwrap();
        assert_eq!(
            ok(source.next().await),
            Some(vec![WireValue::Vertex(2), WireValue::Int(11)])
        );
        assert!(ok(source.next().await).is_none());
        drop(source);
        drop(session);

        let (mut session, prepared) =
            ok(crate::execute::read_prepared_session(&root, db, &token, &definition).await);
        let second_parameters = fgdb_gql::GqlParameters::new()
            .with_int64("value", 23)
            .unwrap();
        let mut source = ok(session.stream_prepared(&query, &prepared, &second_parameters));
        assert_eq!(source.snapshot_seq(), 2);
        for id in 1..=3 {
            assert_eq!(
                ok(source.next().await),
                Some(vec![WireValue::Vertex(id), WireValue::Int(23)])
            );
        }
        assert!(ok(source.next().await).is_none());
        drop(source);
        drop(session);
        server.join_database_workers(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn prepared_source_cursors_and_templates_keep_recovery_and_live_authority_fences() {
    let ((), report) = run_async_under_lab(0x79a0_0412, |root| async move {
        let (server, token, _) = served(&root, "prepared-source-fences").await;
        let db = &server.databases["test"];
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let mut batch = vertex(1);
        batch.create_vertex(fgdb_types::VId(2), vec![], vec![]);
        db.db
            .write(&root)
            .await
            .unwrap()
            .write(&contexts.commit(), batch)
            .await
            .unwrap();
        let request = Prepare {
            mode: PrepareMode::Read,
            statement: "MATCH (n) RETURN n AS key, count(*) AS n GROUP BY n".into(),
            parameters: vec![],
        };
        let definition = ok(prepare_read(&root, db, &token, &request).await);
        let query = contexts.query();
        let parameters = fgdb_gql::GqlParameters::new();
        let (mut session, prepared) =
            ok(crate::execute::read_prepared_session(&root, db, &token, &definition).await);
        let mut source = ok(session.stream_prepared(&query, &prepared, &parameters));
        assert_eq!(
            ok(source.next().await),
            Some(vec![WireValue::Vertex(1), WireValue::Count(1)])
        );
        fail_after_marker(&root, db, 99).await;
        assert_eq!(
            settled(db).await,
            crate::DatabaseStatus::Ready { generation: 2 }
        );
        assert_eq!(
            source.next().await.err().unwrap().code,
            ErrorCode::DatabaseRecovering
        );
        drop(source);
        drop(session);
        assert_eq!(
            crate::execute::read_prepared_session(&root, db, &token, &definition)
                .await
                .err()
                .unwrap()
                .code,
            ErrorCode::DatabaseRecovering
        );

        let definition = ok(prepare_read(&root, db, &token, &request).await);
        let (mut session, prepared) =
            ok(crate::execute::read_prepared_session(&root, db, &token, &definition).await);
        let mut source = ok(session.stream_prepared(&query, &prepared, &parameters));
        assert_eq!(source.snapshot_seq(), 2);
        assert!(ok(source.next().await).is_some());
        assert!(db.authority.retire());
        assert_eq!(
            source.next().await.err().unwrap().code,
            ErrorCode::PermissionDenied
        );
        assert!(ok(source.next().await).is_none());
        drop(source);
        drop(session);
        server.join_database_workers(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn prepared_returning_rebinds_without_defaults_and_never_changes_execution_class() {
    let ((), report) = run_async_under_lab(0x79a0_0431, |root| async move {
        let (server, token, _) = served(&root, "prepared-returning-rebind").await;
        let db = &server.databases["test"];
        let definition = ok(prepare_statement(
            &root,
            db,
            &token,
            &Prepare {
                mode: PrepareMode::WriteReturning,
                statement: "CREATE (n) RETURN $value AS value".into(),
                parameters: vec![("value".into(), WireValue::Int(99))],
            },
        )
        .await);
        let mut cache = PreparedStatements::default();
        let (handle, _) = ok(cache.insert(&root, definition));
        let definition = ok(cache.get(handle));
        assert!(definition.read().is_none());
        assert_eq!(
            db.db.read(&root).await.unwrap().frontier().unwrap(),
            CommitSeq(0)
        );
        for mode in [
            PreparedExecutionMode::Buffered,
            PreparedExecutionMode::SourceStream,
            PreparedExecutionMode::OrderedStream,
            PreparedExecutionMode::Write,
        ] {
            assert_eq!(
                run_prepared(&root, db, &token, &definition, mode, &[])
                    .await
                    .err()
                    .unwrap()
                    .code,
                ErrorCode::Statement
            );
        }
        assert_eq!(
            run_prepared_batch(&root, db, &token, &definition, &[vec![]])
                .await
                .err()
                .unwrap()
                .code,
            ErrorCode::Statement,
            "a returning handle cannot be used by the stats-only batch owner"
        );
        for (seq, value) in [(1, 11), (2, 23)] {
            let answer = ok(run_prepared(
                &root,
                db,
                &token,
                &definition,
                PreparedExecutionMode::WriteReturning,
                &[("value".into(), WireValue::Int(value))],
            )
            .await);
            assert_eq!(answer.columns, ["value"]);
            assert_eq!(answer.rows, [vec![WireValue::Int(value)]]);
            assert_eq!(
                answer.outcome,
                Outcome::WriteCommitted { seq, statements: 1 }
            );
        }
        for parameters in [
            vec![],
            vec![("value".into(), WireValue::Text("wrong type".into()))],
        ] {
            assert_eq!(
                run_prepared(
                    &root,
                    db,
                    &token,
                    &definition,
                    PreparedExecutionMode::WriteReturning,
                    &parameters,
                )
                .await
                .err()
                .unwrap()
                .code,
                ErrorCode::Statement
            );
        }
        assert_eq!(
            db.db.read(&root).await.unwrap().frontier().unwrap(),
            CommitSeq(2)
        );
        server.join_database_workers(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn prepared_returning_retains_recovery_and_live_authority_fences() {
    let ((), report) = run_async_under_lab(0x79a0_0432, |root| async move {
        let (server, token, _) = served(&root, "prepared-returning-fences").await;
        let db = &server.databases["test"];
        let request = Prepare {
            mode: PrepareMode::WriteReturning,
            statement: "CREATE (n) RETURN 7 AS value".into(),
            parameters: vec![],
        };
        let definition = ok(prepare_statement(&root, db, &token, &request).await);
        let mut cache = PreparedStatements::default();
        let (handle, _) = ok(cache.insert(&root, definition));
        let retained = ok(cache.get(handle));
        fail_after_marker(&root, db, 99).await;
        assert_eq!(
            settled(db).await,
            crate::DatabaseStatus::Ready { generation: 2 }
        );
        assert_eq!(
            run_prepared(
                &root,
                db,
                &token,
                &retained,
                PreparedExecutionMode::WriteReturning,
                &[],
            )
            .await
            .err()
            .unwrap()
            .code,
            ErrorCode::DatabaseRecovering
        );
        assert!(cache.get(handle).is_err());
        assert_eq!(cache.source_bytes, 0);
        let current = ok(prepare_statement(&root, db, &token, &request).await);
        assert!(db.authority.retire());
        assert_eq!(
            run_prepared(
                &root,
                db,
                &token,
                &current,
                PreparedExecutionMode::WriteReturning,
                &[],
            )
            .await
            .err()
            .unwrap()
            .code,
            ErrorCode::PermissionDenied
        );
        assert_eq!(
            db.db.read(&root).await.unwrap().frontier().unwrap(),
            CommitSeq(1)
        );
        server.join_database_workers(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn prepared_returning_failure_before_output_admission_never_publishes() {
    let ((), report) = run_async_under_lab(0x79a0_0433, |root| async move {
        let (server, token, _) = served(&root, "prepared-returning-rollback").await;
        let db = &server.databases["test"];
        for text in [
            "MATCH (n) RETURN n",
            "CREATE (n)",
            "CREATE (a) WITH a CREATE (b)",
        ] {
            assert_eq!(
                prepare_statement(
                    &root,
                    db,
                    &token,
                    &Prepare {
                        mode: PrepareMode::WriteReturning,
                        statement: text.into(),
                        parameters: vec![]
                    },
                )
                .await
                .err()
                .unwrap()
                .code,
                ErrorCode::Statement
            );
        }
        assert_eq!(
            prepare_statement(
                &root,
                db,
                &token,
                &Prepare {
                    mode: PrepareMode::WriteReturning,
                    statement: "CREATE (n) RETURN id(n)".into(),
                    parameters: vec![],
                },
            )
            .await
            .err()
            .unwrap()
            .code,
            ErrorCode::PermissionDenied
        );
        let definition = ok(prepare_statement(
            &root,
            db,
            &token,
            &Prepare {
                mode: PrepareMode::WriteReturning,
                statement: "CREATE (n) RETURN 1/$divisor AS value".into(),
                parameters: vec![("divisor".into(), WireValue::Int(1))],
            },
        )
        .await);
        assert!(
            run_prepared(
                &root,
                db,
                &token,
                &definition,
                PreparedExecutionMode::WriteReturning,
                &[("divisor".into(), WireValue::Int(0))],
            )
            .await
            .is_err()
        );
        let limited = db
            .authority
            .issue_at(
                &fgdb_warden::Grant {
                    branch: crate::TRUNK.into(),
                    labels: fgdb_warden::Scope::All,
                    relations: fgdb_warden::Scope::All,
                    properties: fgdb_warden::Scope::All,
                    rights: fgdb_warden::Rights::ReadWrite,
                    limits: fgdb_warden::QueryLimits {
                        max_nodes: 1_000_000,
                        max_work: 100_000_000,
                        max_rows: 0,
                    },
                    expires_at_ms: u64::MAX / 2,
                },
                crate::unix_millis(),
            )
            .unwrap();
        assert_eq!(
            run_prepared(
                &root,
                db,
                &limited,
                &definition,
                PreparedExecutionMode::WriteReturning,
                &[("divisor".into(), WireValue::Int(1))],
            )
            .await
            .err()
            .unwrap()
            .code,
            ErrorCode::Budget,
            "signed output admission runs while the write can still roll back"
        );
        assert_eq!(
            db.db.read(&root).await.unwrap().frontier().unwrap(),
            CommitSeq(0)
        );
        server.join_database_workers(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn prepared_returning_binding_and_native_execution_share_one_signed_work_ceiling() {
    let ((), report) = run_async_under_lab(0x79a0_0434, |root| async move {
        let (server, token, _) = served(&root, "prepared-returning-work-debit").await;
        let db = &server.databases["test"];
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        db.db
            .write(&root)
            .await
            .unwrap()
            .write(&contexts.commit(), vertex(1))
            .await
            .unwrap();
        let statement = "MERGE (n) RETURN 7 AS value";
        let definition = ok(prepare_statement(
            &root,
            db,
            &token,
            &Prepare {
                mode: PrepareMode::WriteReturning,
                statement: statement.into(),
                parameters: vec![],
            },
        )
        .await);

        // Measure the existing native checked owner's exact admission
        // threshold on an unchanged graph. Every accepted probe is a read
        // close, so the source, row and representation work remain identical.
        let mut low = 0;
        let mut high = 1_000_000;
        while low + 1 < high {
            let work = low + (high - low) / 2;
            let scoped = token
                .attenuate(fgdb_warden::Restriction::MaxWork(work))
                .unwrap();
            let bound = ok(crate::execute::Returning::prepare(
                db,
                statement,
                &fgdb_gql::GqlParameters::new(),
            ))
            .unwrap();
            match crate::execute::write_returning_checked(
                &root,
                db,
                &scoped,
                bound,
                None,
                |_, rows, _| Ok(rows),
            )
            .await
            {
                Ok(answer) => {
                    assert_eq!(
                        answer.outcome,
                        Outcome::ReadClosed {
                            seq: 1,
                            statements: 1
                        }
                    );
                    high = work;
                }
                Err(error) => {
                    assert_eq!(error.code, ErrorCode::Budget, "{}", error.message);
                    low = work;
                }
            }
        }
        for (work, succeeds) in [(high, false), (high + 1, true)] {
            let scoped = token
                .attenuate(fgdb_warden::Restriction::MaxWork(work))
                .unwrap();
            let result = run_prepared(
                &root,
                db,
                &scoped,
                &definition,
                PreparedExecutionMode::WriteReturning,
                &[],
            )
            .await;
            if succeeds {
                let answer = ok(result);
                assert_eq!(answer.rows, [vec![WireValue::Int(7)]]);
                assert_eq!(
                    answer.outcome,
                    Outcome::ReadClosed {
                        seq: 1,
                        statements: 1
                    }
                );
            } else {
                assert_eq!(
                    result.err().unwrap().code,
                    ErrorCode::Budget,
                    "one binding unit cannot disappear when the native owner begins"
                );
            }
        }
        let creating = ok(prepare_statement(
            &root,
            db,
            &token,
            &Prepare {
                mode: PrepareMode::WriteReturning,
                statement: "CREATE (n) RETURN 7 AS value".into(),
                parameters: vec![],
            },
        )
        .await);
        let mut full = token.clone();
        for _ in 0..fgdb_warden::MAX_CAVEATS {
            match full.attenuate(fgdb_warden::Restriction::MaxWork(u64::MAX)) {
                Ok(next) => full = next,
                Err(fgdb_warden::Error::TooLarge) => break,
                Err(error) => panic!("{error}"),
            }
        }
        assert!(matches!(
            full.attenuate(fgdb_warden::Restriction::MaxWork(u64::MAX)),
            Err(fgdb_warden::Error::TooLarge)
        ));
        assert_eq!(
            run_prepared(
                &root,
                db,
                &full,
                &creating,
                PreparedExecutionMode::WriteReturning,
                &[],
            )
            .await
            .err()
            .unwrap()
            .code,
            ErrorCode::Budget,
            "a full attenuation envelope refuses before creating a vertex"
        );
        assert_eq!(
            db.db.read(&root).await.unwrap().frontier().unwrap(),
            CommitSeq(1)
        );
        server.join_database_workers(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn prepared_no_return_chains_keep_write_mode_and_fresh_arguments() {
    let ((), report) = run_async_under_lab(0x79a0_0442, |root| async move {
        let (server, token, _) = served(&root, "prepared-rowless-chain").await;
        let db = &server.databases["test"];
        let arguments = |values: &[i64]| -> Vec<(String, WireValue)> {
            vec![(
                "rows".into(),
                WireValue::List(values.iter().copied().map(WireValue::Int).collect()),
            )]
        };
        let statement = "UNWIND $rows AS row CREATE (a) WITH a, row CREATE (b)";
        let definition = ok(prepare_statement(
            &root,
            db,
            &token,
            &Prepare {
                mode: PrepareMode::Write,
                statement: statement.into(),
                parameters: arguments(&[99]),
            },
        )
        .await);
        let mut cache = PreparedStatements::default();
        let (handle, _) = ok(cache.insert(&root, definition));
        let definition = ok(cache.get(handle));
        assert!(definition.read().is_none());
        for mode in [
            PreparedExecutionMode::Buffered,
            PreparedExecutionMode::WriteReturning,
        ] {
            assert_eq!(
                run_prepared(&root, db, &token, &definition, mode, &arguments(&[1]))
                    .await
                    .err()
                    .unwrap()
                    .code,
                ErrorCode::Statement
            );
        }
        assert_eq!(
            run_prepared_batch(&root, db, &token, &definition, &[arguments(&[1])])
                .await
                .err()
                .unwrap()
                .code,
            ErrorCode::Statement,
            "the script-batch owner cannot independently commit chain argument sets"
        );
        assert_eq!(
            prepare_statement(
                &root,
                db,
                &token,
                &Prepare {
                    mode: PrepareMode::Write,
                    statement: format!("{statement} RETURN 1 AS value"),
                    parameters: arguments(&[1]),
                },
            )
            .await
            .err()
            .unwrap()
            .code,
            ErrorCode::Statement
        );
        assert_eq!(
            db.db.read(&root).await.unwrap().frontier().unwrap(),
            CommitSeq(0)
        );
        let answer = ok(run_prepared(
            &root,
            db,
            &token,
            &definition,
            PreparedExecutionMode::Write,
            &arguments(&[1, 2]),
        )
        .await);
        assert!(answer.columns.is_empty());
        assert!(answer.rows.is_empty());
        assert_eq!(
            answer.outcome,
            Outcome::WriteCommitted {
                seq: 1,
                statements: 1
            }
        );
        assert_eq!(
            db.db.read(&root).await.unwrap().vertices().unwrap().len(),
            4
        );
        assert_eq!(
            run_prepared(
                &root,
                db,
                &token,
                &definition,
                PreparedExecutionMode::Write,
                &[],
            )
            .await
            .err()
            .unwrap()
            .code,
            ErrorCode::Statement,
            "PREPARE arguments are not implicit execution defaults"
        );
        let empty = ok(run_prepared(
            &root,
            db,
            &token,
            &definition,
            PreparedExecutionMode::Write,
            &arguments(&[]),
        )
        .await);
        assert!(empty.columns.is_empty());
        assert!(empty.rows.is_empty());
        assert_eq!(
            empty.outcome,
            Outcome::ReadClosed {
                seq: 1,
                statements: 1
            }
        );
        assert_eq!(
            db.db.read(&root).await.unwrap().vertices().unwrap().len(),
            4
        );
        server.join_database_workers(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
