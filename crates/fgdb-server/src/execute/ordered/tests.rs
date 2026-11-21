use super::*;
use crate::recovery::tests::{fail_after_marker, served, settled, vertex};
use asupersync::lab::run_async_under_lab;
use fgdb_warden::{Grant, QueryLimits, Scope};

fn accepted<T>(result: Result<T, Refusal>) -> T {
    result.unwrap_or_else(|error| panic!("{}: {}", error.code.name(), error.message))
}

#[test]
fn ordered_reads_bind_durable_names_at_the_selected_cut_and_fence_stale_preparations() {
    use fgdb_delta_types::catalog::{CatalogSymbol, SymbolCatalog};
    use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
    use fgdb_protocol::body::{Prepare, PrepareMode};
    use fgdb_types::{CanonicalScalar, CommitSeq, VId};

    let ((), report) = run_async_under_lab(0x79a0_ca10, |root| async move {
        let (mut server, token, path) = served(&root, "ordered-durable-catalog").await;
        let directory = path.join("ordered-scratch");
        asupersync::fs::create_dir(&directory).await.unwrap();
        let mut config = crate::OrderedReadConfig::new(&directory);
        config.max_concurrent = 1;
        config.sort.run_rows = 2;
        config.sort.page_bytes = 31;
        config.sort.max_sort_work = 1_000_000;
        Arc::get_mut(server.databases.get_mut("test").unwrap())
            .unwrap()
            .ordered_reads = Some(
            crate::ordered::OrderedReads::open(&root, config)
                .await
                .unwrap(),
        );
        let db = &server.databases["test"];
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let query = contexts.query();
        {
            let mut writer = db.db.write(&root).await.unwrap();
            let mut catalog = SymbolCatalog::default();
            catalog
                .bind_all([
                    ("Person", CatalogSymbol::Label(LabelId(1))),
                    ("score", CatalogSymbol::Property(PropertyKeyId(1))),
                ])
                .unwrap();
            assert_eq!(
                writer
                    .register_symbols(&contexts.commit(), &catalog)
                    .await
                    .unwrap(),
                CommitSeq(1)
            );
            let mut graph = fgdb::WriteBatch::new(RelationId(1));
            for (id, score) in [(1, 30), (2, 10), (3, 20)] {
                graph.create_vertex(
                    VId(id),
                    vec![LabelId(1)],
                    vec![(PropertyKeyId(1), CanonicalScalar::Int(score))],
                );
            }
            assert_eq!(
                writer.write(&contexts.commit(), graph).await.unwrap(),
                CommitSeq(2)
            );
        }
        let historical_text =
            "MATCH (n:Person) FOR SYSTEM_TIME AS OF SEQ $at RETURN n.score AS value ORDER BY value";
        let historical_arguments = vec![("at".to_owned(), WireValue::Count(2))];
        let prepared = accepted(
            crate::execute::prepare_read(
                &root,
                db,
                &token,
                &Prepare {
                    mode: PrepareMode::Read,
                    statement: historical_text.into(),
                    parameters: historical_arguments.clone(),
                },
            )
            .await,
        );
        {
            let mut writer = db.db.write(&root).await.unwrap();
            let mut later = SymbolCatalog::default();
            later
                .bind_all([
                    ("Later", CatalogSymbol::Label(LabelId(9))),
                    ("later", CatalogSymbol::Property(PropertyKeyId(9))),
                ])
                .unwrap();
            assert_eq!(
                writer
                    .register_symbols(&contexts.commit(), &later)
                    .await
                    .unwrap(),
                CommitSeq(3)
            );
            let mut graph = fgdb::WriteBatch::new(RelationId(1));
            for (id, score) in [(4, 60), (5, 40), (6, 50)] {
                graph.create_vertex(
                    VId(id),
                    vec![LabelId(9)],
                    vec![(PropertyKeyId(9), CanonicalScalar::Int(score))],
                );
            }
            assert_eq!(
                writer.write(&contexts.commit(), graph).await.unwrap(),
                CommitSeq(4)
            );
        }
        let generation = accepted(ordered_generation(db, &token));
        let historical_parameters =
            crate::convert::parameters(&historical_arguments, None).unwrap();
        {
            let (mut session, template) = accepted(
                read_ordered_session_for(
                    &root,
                    db,
                    &token,
                    Some(&prepared),
                    generation.clone(),
                    (historical_text, &historical_parameters),
                )
                .await,
            );
            let template = template.expect("the cached historical template is attached");
            let mut cursor = accepted(
                accepted(session.start(&query, &template, &historical_parameters))
                    .finish()
                    .await,
            );
            assert_eq!(cursor.snapshot_seq(), 2);
            for expected in [10, 20, 30] {
                assert_eq!(
                    accepted(cursor.next().await),
                    Some(vec![WireValue::Int(expected)])
                );
            }
            assert!(accepted(cursor.next().await).is_none());
            assert!(cursor.exhausted());
        }
        let empty = fgdb_gql::GqlParameters::new();
        {
            let missing = "MATCH (n:Later) FOR SYSTEM_TIME AS OF SEQ 2 RETURN n.later AS value ORDER BY value";
            let (mut session, _) = accepted(
                read_ordered_session_for(
                    &root,
                    db,
                    &token,
                    None,
                    generation.clone(),
                    (missing, &empty),
                )
                .await,
            );
            let refusal = session
                .prepare(&query, missing, &empty)
                .err()
                .expect("later names cannot bind against an earlier catalog");
            assert_eq!(refusal.code, ErrorCode::Statement);
        }
        let newer =
            crate::convert::parameters(&[("at".into(), WireValue::Count(4))], None).unwrap();
        let stale = read_ordered_session_for(
            &root,
            db,
            &token,
            Some(&prepared),
            generation.clone(),
            (historical_text, &newer),
        )
        .await
        .err()
        .expect("rebinding a prepared cut must recheck its name map");
        assert_eq!(stale.code, ErrorCode::Conflict);
        // A refused prepared map releases its sole scratch lease. A fresh
        // request discovers the current names and performs a new native sort.
        {
            let current = "MATCH (n:Later) RETURN n.later AS value ORDER BY value";
            let (mut session, _) = accepted(
                read_ordered_session_for(&root, db, &token, None, generation, (current, &empty))
                    .await,
            );
            let template = accepted(session.prepare(&query, current, &empty));
            let mut cursor = accepted(
                accepted(session.start(&query, &template, &empty))
                    .finish()
                    .await,
            );
            assert_eq!(cursor.snapshot_seq(), 4);
            for expected in [40, 50, 60] {
                assert_eq!(
                    accepted(cursor.next().await),
                    Some(vec![WireValue::Int(expected)])
                );
            }
            assert!(accepted(cursor.next().await).is_none());
            assert!(cursor.exhausted());
        }
        server.finish_shutdown(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn ordered_admission_is_authenticated_bounded_and_old_cursors_fail_after_recovery() {
    let ((), report) = run_async_under_lab(0x79a0_0602, |root| async move {
        let (mut server, token, path) = served(&root, "ordered-recovery").await;
        let db = server.databases.get_mut("test").unwrap();
        let generation = accepted(ordered_generation(db, &token));
        let expired = db
            .authority
            .issue_at(
                &Grant::read_only(
                    crate::TRUNK,
                    1,
                    QueryLimits {
                        max_nodes: 100,
                        max_work: 100,
                        max_rows: 100,
                    },
                ),
                0,
            )
            .unwrap();
        assert_eq!(
            ordered_generation(db, &expired).err().unwrap().code,
            ErrorCode::PermissionDenied
        );
        assert_eq!(
            read_ordered_session(&root, db, &expired, None, generation.clone())
                .await
                .err()
                .unwrap()
                .code,
            ErrorCode::PermissionDenied
        );
        assert_eq!(
            read_ordered_session(&root, db, &token, None, generation.clone())
                .await
                .err()
                .unwrap()
                .code,
            ErrorCode::Statement
        );
        let directory = path.join("ordered-scratch");
        asupersync::fs::create_dir(&directory).await.unwrap();
        let mut config = crate::OrderedReadConfig::new(&directory);
        config.max_concurrent = 1;
        config.sort.run_rows = 2;
        config.sort.page_bytes = 31;
        config.sort.max_sort_work = 1_000_000;
        Arc::get_mut(db).unwrap().ordered_reads = Some(
            crate::ordered::OrderedReads::open(&root, config)
                .await
                .unwrap(),
        );
        let db = &server.databases["test"];
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let query = contexts.query();
        let mut batch = vertex(1);
        batch.create_vertex(fgdb_types::VId(2), vec![], vec![]);
        batch.create_vertex(fgdb_types::VId(3), vec![], vec![]);
        db.db
            .write(&root)
            .await
            .unwrap()
            .write(&contexts.commit(), batch)
            .await
            .unwrap();
        // External-sort admission also pins the published view without
        // waiting for an otherwise unchanged writer to release its lock.
        let held = db.db.write(&root).await.unwrap();
        let (mut session, _) =
            accepted(read_ordered_session(&root, db, &token, None, generation.clone()).await);
        drop(held);
        assert_eq!(
            read_ordered_session(&root, db, &token, None, generation.clone())
                .await
                .err()
                .unwrap()
                .code,
            ErrorCode::Budget
        );
        let params = fgdb_gql::GqlParameters::new();
        let prepared = accepted(session.prepare(
            &query,
            "MATCH (n) RETURN 1 AS value ORDER BY value",
            &params,
        ));
        let mut cursor = accepted(
            accepted(session.start(&query, &prepared, &params))
                .finish()
                .await,
        );
        assert_eq!(cursor.snapshot_seq(), 1);
        assert_eq!(accepted(cursor.next().await), Some(vec![WireValue::Int(1)]));
        fail_after_marker(&root, db, 4).await;
        assert!(matches!(
            settled(db).await,
            crate::DatabaseStatus::Ready { .. }
        ));
        assert_eq!(
            cursor.next().await.err().unwrap().code,
            ErrorCode::DatabaseRecovering
        );
        drop(cursor);
        drop(session);
        assert_eq!(
            read_ordered_session(&root, db, &token, None, generation)
                .await
                .err()
                .unwrap()
                .code,
            ErrorCode::DatabaseRecovering
        );
        let generation = accepted(ordered_generation(db, &token));
        let (mut fresh, _) =
            accepted(read_ordered_session(&root, db, &token, None, generation).await);
        let prepared = accepted(fresh.prepare(
            &query,
            "MATCH (n) RETURN 1 AS value ORDER BY value LIMIT 1",
            &params,
        ));
        let mut cursor = accepted(
            accepted(fresh.start(&query, &prepared, &params))
                .finish()
                .await,
        );
        assert_eq!(cursor.snapshot_seq(), 2);
        assert_eq!(accepted(cursor.next().await), Some(vec![WireValue::Int(1)]));
        drop(cursor);
        drop(fresh);
        server.join_database_workers(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn a_pending_order_does_not_hold_a_recovery_operation() {
    let ((), report) = run_async_under_lab(0x79a0_0603, |root| async move {
        let (server, token, _) = served(&root, "ordered-pending-recovery").await;
        let db = &server.databases["test"];
        let generation = db.db.read(&root).await.unwrap().generation();
        let mut future = Box::pin(in_generation(
            &generation,
            core::future::pending::<Result<(), Refusal>>(),
        ));
        poll_fn(|context| {
            assert!(future.as_mut().poll(context).is_pending());
            Poll::Ready(())
        })
        .await;
        fail_after_marker(&root, db, 1).await;
        assert!(matches!(
            settled(db).await,
            crate::DatabaseStatus::Ready { .. }
        ));
        assert_eq!(
            future.await.err().unwrap().code,
            ErrorCode::DatabaseRecovering
        );
        assert!(db.admits(&token));
        server.join_database_workers(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn ordered_difference_preserves_signed_multiplicity_and_refuses_stale_delivery() {
    let ((), report) = run_async_under_lab(0xd1ff_9002, |root| async move {
        let (mut server, _, path) = served(&root, "ordered-difference-recovery").await;
        let db = server.databases.get_mut("test").unwrap();
        let directory = path.join("difference-scratch");
        asupersync::fs::create_dir(&directory).await.unwrap();
        let mut config = crate::OrderedReadConfig::new(&directory);
        config.max_concurrent = 1;
        config.sort.run_rows = 2;
        config.sort.page_bytes = 31;
        config.sort.max_sort_work = 1_000_000;
        Arc::get_mut(db).unwrap().ordered_reads = Some(
            crate::ordered::OrderedReads::open(&root, config)
                .await
                .unwrap(),
        );
        let db = &server.databases["test"];
        let mut grant = Grant::read_only(
            TRUNK,
            u64::MAX / 2,
            QueryLimits {
                max_nodes: 100_000,
                max_work: 100_000_000,
                max_rows: 1,
            },
        );
        let denied = db.authority.issue_at(&grant, unix_millis()).unwrap();
        // read_only starts with no visible objects. This query needs vertex
        // visibility; relation and property scopes remain empty.
        grant.labels = Scope::All;
        let token = db.authority.issue_at(&grant, unix_millis()).unwrap();
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let query = contexts.query();
        let mut initial = vertex(1);
        initial.create_vertex(fgdb_types::VId(2), vec![], vec![]);
        initial.create_vertex(fgdb_types::VId(3), vec![], vec![]);
        db.db
            .write(&root)
            .await
            .unwrap()
            .write(&contexts.commit(), initial)
            .await
            .unwrap();
        let mut update = fgdb::WriteBatch::new(fgdb_delta_types::RelationId(1));
        update.delete_vertex(fgdb_types::VId(1));
        update.delete_vertex(fgdb_types::VId(2));
        db.db
            .write(&root)
            .await
            .unwrap()
            .write(&contexts.commit(), update)
            .await
            .unwrap();

        let generation = accepted(ordered_generation(db, &token));
        {
            let (mut hidden, _) =
                accepted(read_ordered_session(&root, db, &denied, None, generation.clone()).await);
            let hidden_plan = accepted(hidden.prepare(
                &query,
                "MATCH (n) RETURN 1 AS value",
                &fgdb_gql::GqlParameters::new(),
            ));
            let parameters = fgdb_gql::GqlParameters::new();
            let execution =
                accepted(hidden.start_difference(&query, &hidden_plan, &parameters, 1, 2));
            let mut cursor = accepted(execution.finish().await);
            assert!(accepted(cursor.next().await).is_none());
            assert!(cursor.exhausted());
        }
        let (mut session, _) =
            accepted(read_ordered_session(&root, db, &token, None, generation.clone()).await);
        let params = fgdb_gql::GqlParameters::new();
        let prepared = accepted(session.prepare(&query, "MATCH (n) RETURN 1 AS value", &params));
        for (before, after, weight) in [(1, 2, Some(-2)), (2, 1, Some(2)), (2, 2, None)] {
            let execution =
                accepted(session.start_difference(&query, &prepared, &params, before, after));
            assert_eq!(execution.columns(), ["value"]);
            let mut cursor = accepted(execution.finish().await);
            assert_eq!((cursor.before(), cursor.after()), (before, after));
            assert_eq!(
                accepted(cursor.next().await),
                weight.map(|weight| (weight, vec![WireValue::Int(1)]))
            );
            assert!(accepted(cursor.next().await).is_none());
            assert!(cursor.exhausted());
        }
        let aggregate =
            accepted(session.prepare(&query, "MATCH (n) RETURN COUNT(*) AS count", &params));
        assert_eq!(
            session
                .start_difference(&query, &aggregate, &params, 1, 2)
                .err()
                .unwrap()
                .code,
            ErrorCode::Statement
        );
        let mut cursor = accepted(
            accepted(session.start_difference(&query, &prepared, &params, 1, 2))
                .finish()
                .await,
        );
        assert!(!cursor.exhausted());
        fail_after_marker(&root, db, 4).await;
        assert!(matches!(
            settled(db).await,
            crate::DatabaseStatus::Ready { .. }
        ));
        assert_eq!(
            cursor.next().await.err().unwrap().code,
            ErrorCode::DatabaseRecovering
        );
        assert!(!cursor.exhausted());
        assert!(accepted(cursor.next().await).is_none());
        assert!(!cursor.exhausted());
        drop(cursor);
        drop(session);
        assert_eq!(
            read_ordered_session(&root, db, &token, None, generation)
                .await
                .err()
                .unwrap()
                .code,
            ErrorCode::DatabaseRecovering
        );
        server.join_database_workers(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn ordered_aggregate_cursors_fence_recovery_and_live_authority_without_claiming_exhaustion() {
    let ((), report) = run_async_under_lab(0x79a0_a601, |root| async move {
        let (mut server, token, path) = served(&root, "ordered-aggregate-fences").await;
        let directory = path.join("ordered-scratch");
        asupersync::fs::create_dir(&directory).await.unwrap();
        let mut config = crate::OrderedReadConfig::new(&directory);
        config.max_concurrent = 1;
        config.sort.run_rows = 2;
        config.sort.page_bytes = 31;
        config.sort.max_sort_work = 1_000_000;
        Arc::get_mut(server.databases.get_mut("test").unwrap())
            .unwrap()
            .ordered_reads = Some(
            crate::ordered::OrderedReads::open(&root, config)
                .await
                .unwrap(),
        );
        let db = &server.databases["test"];
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let query = contexts.query();
        let mut initial = vertex(1);
        initial.create_vertex(fgdb_types::VId(2), vec![], vec![]);
        initial.create_vertex(fgdb_types::VId(3), vec![], vec![]);
        db.db
            .write(&root)
            .await
            .unwrap()
            .write(&contexts.commit(), initial)
            .await
            .unwrap();
        let parameters = fgdb_gql::GqlParameters::new();
        let text = "MATCH (n) RETURN n AS node, COUNT(*) AS rows GROUP BY n ORDER BY node";
        let generation = accepted(ordered_generation(db, &token));
        let (mut session, _) =
            accepted(read_ordered_session(&root, db, &token, None, generation.clone()).await);
        let prepared = accepted(session.prepare(&query, text, &parameters));
        let execution = accepted(session.start(&query, &prepared, &parameters));
        assert_eq!(execution.columns(), ["node", "rows"]);
        let mut cursor = accepted(execution.finish().await);
        assert_eq!(cursor.snapshot_seq(), 1);
        assert_eq!(
            accepted(cursor.next().await),
            Some(vec![WireValue::Vertex(1), WireValue::Count(1)])
        );
        assert!(!cursor.exhausted());
        fail_after_marker(&root, db, 4).await;
        assert!(matches!(
            settled(db).await,
            crate::DatabaseStatus::Ready { .. }
        ));
        assert_eq!(
            cursor.next().await.err().unwrap().code,
            ErrorCode::DatabaseRecovering
        );
        assert!(!cursor.exhausted());
        assert!(accepted(cursor.next().await).is_none());
        assert!(!cursor.exhausted());
        drop(cursor);
        drop(session);

        // Releasing a failed aggregate cursor returns the one scratch slot.
        // A fresh generation can run, but retirement remains live during pulls.
        let generation = accepted(ordered_generation(db, &token));
        let (mut fresh, _) =
            accepted(read_ordered_session(&root, db, &token, None, generation).await);
        let prepared = accepted(fresh.prepare(&query, text, &parameters));
        let mut cursor = accepted(
            accepted(fresh.start(&query, &prepared, &parameters))
                .finish()
                .await,
        );
        assert_eq!(cursor.snapshot_seq(), 2);
        assert_eq!(
            accepted(cursor.next().await),
            Some(vec![WireValue::Vertex(1), WireValue::Count(1)])
        );
        assert!(db.authority.retire());
        assert_eq!(
            cursor.next().await.err().unwrap().code,
            ErrorCode::PermissionDenied
        );
        assert!(!cursor.exhausted());
        assert!(accepted(cursor.next().await).is_none());
        assert!(!cursor.exhausted());
        drop(cursor);
        drop(fresh);
        server.finish_shutdown(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
