//! Standing aggregates consume committed deltas, not repeated graph scans.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, WriteBatch};
use fgdb_delta_types::{PropertyKeyId, RelationId, ZWeight};
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind, PreparedGraphAggregateText,
};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, PurposeContexts, VId};

const P: PropertyKeyId = PropertyKeyId(1);
const N: PropertyKeyId = PropertyKeyId(2);
const TEXT: &str = "MATCH (n) WHERE n.selected = 1 RETURN COUNT(*) AS count,SUM(n.amount) AS total";

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Property, "selected") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "amount") => Some(GraphSymbol::Property(N)),
        _ => None,
    }
}

fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000)
}

#[test]
fn committed_insert_updates_registered_equality_count_and_sum() {
    let ((), report) = run_async_under_lab(0x005c_3501, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query_cx = contexts.query();
        let keys = DatabaseKeys::new(
            [0x35; 32],
            DatabaseSecurityNamespaceId([0x36; 32]),
            [0x37; 32],
        );
        let mut db = Database::open_memory(&commit, keys).await.unwrap();
        let query = PreparedGraphAggregateText::prepare(TEXT, symbols)
            .unwrap()
            .bind_parameters(&GqlParameters::new())
            .unwrap();
        let handle = db
            .register_standing_query(&query_cx, query.clone(), policy())
            .unwrap();
        let mut batch = WriteBatch::new(RelationId(1));
        for (id, selected, amount) in [(1, 1, 7), (2, 0, 90)] {
            batch.create_vertex(
                VId(id),
                vec![],
                vec![
                    (P, CanonicalScalar::Int(selected)),
                    (N, CanonicalScalar::Int(amount)),
                ],
            );
        }
        let at = db.write(&commit, batch).await.unwrap();
        let maintained = db.standing_query(&query_cx, &handle).unwrap();
        assert_eq!(maintained.frontier(), at);
        let full = db
            .execute_graph_aggregate_governed(&query_cx, &query, policy())
            .unwrap();
        let expected = &full.value[0];
        assert_eq!(expected.get(0).unwrap().as_count(), Some(1));
        assert_eq!(expected.get(1).unwrap().as_integer(), Some(7));
        assert_eq!(maintained.rows().len(), 1);
        assert_eq!(
            maintained.rows().weight(expected),
            Some(&ZWeight::from_i128(1))
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn seeded_commits_retract_restore_and_survive_compaction() {
    for seed in [0x005c_3511, 0x005c_3522, 0x005c_3533] {
        let ((), report) = run_async_under_lab(seed, move |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let cx = contexts.query();
            let keys = || {
                DatabaseKeys::new(
                    [0x35; 32],
                    DatabaseSecurityNamespaceId([0x36; 32]),
                    [0x37; 32],
                )
            };
            let vfs = fgdb::MemVfs::new().unwrap();
            let path = vfs.database_dir();
            let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
                .await
                .unwrap();
            let query = PreparedGraphAggregateText::prepare(TEXT, symbols)
                .unwrap()
                .bind_parameters(&GqlParameters::new())
                .unwrap();
            let handle = db
                .register_standing_query(&cx, query.clone(), policy())
                .unwrap();
            let amount = (seed % 97 + 1) as i64;
            let mut previous = db
                .execute_graph_aggregate_governed(&cx, &query, policy())
                .unwrap()
                .value[0]
                .clone();
            let mut selected_summary = None;
            for step in 0..8 {
                let mut batch = WriteBatch::new(RelationId(1));
                match step {
                    0 => {
                        batch.create_vertex(
                            VId(1),
                            vec![],
                            vec![
                                (P, CanonicalScalar::Int(1)),
                                (N, CanonicalScalar::Int(amount)),
                            ],
                        );
                        batch.create_vertex(
                            VId(2),
                            vec![],
                            vec![(P, CanonicalScalar::Int(0)), (N, CanonicalScalar::Int(900))],
                        );
                    }
                    1 => {
                        batch.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(0)));
                    }
                    2 => {
                        batch.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(1)));
                    }
                    3 => {
                        batch.set_vertex_property(
                            VId(1),
                            N,
                            Some(CanonicalScalar::Int(amount + 1)),
                        );
                    }
                    4 => {
                        batch.add_edge(fgdb_types::EId(10), VId(1), VId(2), vec![]);
                    }
                    5 => {
                        batch.delete_edge(fgdb_types::EId(10));
                    }
                    6 => {
                        batch.delete_vertex(VId(1));
                    }
                    7 => {
                        // Spent identities cannot resurrect; restore the same
                        // logical contribution using a fresh vertex identity.
                        batch.create_vertex(
                            VId(3),
                            vec![],
                            vec![
                                (P, CanonicalScalar::Int(1)),
                                (N, CanonicalScalar::Int(amount)),
                            ],
                        );
                    }
                    _ => unreachable!(),
                }
                let at = db.write(&commit, batch).await.unwrap();
                let full = db
                    .execute_graph_aggregate_governed(&cx, &query, policy())
                    .unwrap();
                let maintained = db.standing_query(&cx, &handle).unwrap();
                assert_eq!(maintained.frontier(), at);
                assert_eq!(
                    maintained.rows().len(),
                    full.value.len(),
                    "seed={seed} step={step}"
                );
                for row in &full.value {
                    assert_eq!(
                        maintained.rows().weight(row),
                        Some(&ZWeight::ONE),
                        "seed={seed} step={step}"
                    );
                }
                let expected_count = if matches!(step, 1 | 6) { 0 } else { 1 };
                assert_eq!(
                    full.value[0].get(0).unwrap().as_count(),
                    Some(expected_count)
                );
                if expected_count == 0 {
                    assert!(full.value[0].get(1).unwrap().is_null());
                }
                if previous != full.value[0] {
                    assert_eq!(
                        maintained.rows().weight(&previous),
                        None,
                        "obsolete aggregate row must retract at seed={seed} step={step}"
                    );
                }
                if step == 0 {
                    selected_summary = Some(full.value[0].clone());
                }
                if matches!(step, 1 | 6) {
                    assert_eq!(
                        maintained.rows().weight(selected_summary.as_ref().unwrap()),
                        None
                    );
                }
                if matches!(step, 2 | 7) {
                    assert_eq!(
                        maintained.rows().weight(selected_summary.as_ref().unwrap()),
                        Some(&ZWeight::ONE)
                    );
                }
                previous = full.value[0].clone();
                db.compact(&commit).await.unwrap();
                let after_compaction = db.standing_query(&cx, &handle).unwrap();
                let compacted_full = db
                    .execute_graph_aggregate_governed(&cx, &query, policy())
                    .unwrap();
                assert_eq!(after_compaction.rows().len(), compacted_full.value.len());
                for row in &compacted_full.value {
                    assert_eq!(after_compaction.rows().weight(row), Some(&ZWeight::ONE));
                }
            }
            // Each seed also varies nullable payloads and predicate transitions,
            // rather than merely replaying the same history under a new scheduler.
            let mut random = seed;
            for _ in 0..24 {
                random ^= random << 13;
                random ^= random >> 7;
                random ^= random << 17;
                let mut batch = WriteBatch::new(RelationId(1));
                batch.set_vertex_property(
                    VId(3),
                    P,
                    Some(CanonicalScalar::Int((random & 1) as i64)),
                );
                batch.set_vertex_property(
                    VId(3),
                    N,
                    (random & 2 != 0).then_some(CanonicalScalar::Int((random % 101) as i64 - 50)),
                );
                let at = db.write(&commit, batch).await.unwrap();
                let full = db
                    .execute_graph_aggregate_governed(&cx, &query, policy())
                    .unwrap();
                let maintained = db.standing_query(&cx, &handle).unwrap();
                assert_eq!(maintained.frontier(), at);
                assert_eq!(maintained.rows().len(), full.value.len());
                for row in &full.value {
                    assert_eq!(maintained.rows().weight(row), Some(&ZWeight::ONE));
                }
            }
            let before_reopen = db
                .execute_graph_aggregate_governed(&cx, &query, policy())
                .unwrap()
                .value;
            drop(db);
            let mut reopened = Database::open_with_vfs(&commit, vfs, &path, keys())
                .await
                .unwrap();
            assert!(matches!(
                reopened.standing_query(&cx, &handle),
                Err(fgdb::StandingQueryError::ForeignHandle)
            ));
            let fresh = reopened
                .register_standing_query(&cx, query.clone(), policy())
                .unwrap();
            let restored = reopened.standing_query(&cx, &fresh).unwrap();
            let full = reopened
                .execute_graph_aggregate_governed(&cx, &query, policy())
                .unwrap()
                .value;
            assert_eq!(full, before_reopen);
            assert_eq!(restored.rows().len(), full.len());
            for row in &full {
                assert_eq!(restored.rows().weight(row), Some(&ZWeight::ONE));
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }
}

#[test]
fn reopen_refuses_old_handle_and_registration_rebuilds_exactly() {
    let ((), report) = run_async_under_lab(0x005c_3544, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let keys = || {
            DatabaseKeys::new(
                [0x35; 32],
                DatabaseSecurityNamespaceId([0x36; 32]),
                [0x37; 32],
            )
        };
        let vfs = fgdb::MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let query = PreparedGraphAggregateText::prepare(TEXT, symbols)
            .unwrap()
            .bind_parameters(&GqlParameters::new())
            .unwrap();
        let handle = db
            .register_standing_query(&cx, query.clone(), policy())
            .unwrap();
        let mut batch = WriteBatch::new(RelationId(1));
        batch.create_vertex(
            VId(1),
            vec![],
            vec![(P, CanonicalScalar::Int(1)), (N, CanonicalScalar::Int(73))],
        );
        let at = db.write(&commit, batch).await.unwrap();
        let before = db
            .execute_graph_aggregate_governed(&cx, &query, policy())
            .unwrap()
            .value;
        assert_eq!(
            db.standing_query(&cx, &handle)
                .unwrap()
                .rows()
                .weight(&before[0]),
            Some(&ZWeight::ONE)
        );
        drop(db);
        let mut reopened = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        assert!(matches!(
            reopened.standing_query(&cx, &handle),
            Err(fgdb::StandingQueryError::ForeignHandle)
        ));
        let fresh = reopened
            .register_standing_query(&cx, query.clone(), policy())
            .unwrap();
        let restored = reopened.standing_query(&cx, &fresh).unwrap();
        assert_eq!(restored.frontier(), at);
        let full = reopened
            .execute_graph_aggregate_governed(&cx, &query, policy())
            .unwrap()
            .value;
        assert_eq!(full, before);
        assert_eq!(restored.rows().len(), full.len());
        for row in &full {
            assert_eq!(restored.rows().weight(row), Some(&ZWeight::ONE));
        }
        let mut deletion = WriteBatch::new(RelationId(1));
        deletion.delete_vertex(VId(1));
        reopened.write(&commit, deletion).await.unwrap();
        let empty = reopened
            .execute_graph_aggregate_governed(&cx, &query, policy())
            .unwrap()
            .value;
        let maintained = reopened.standing_query(&cx, &fresh).unwrap();
        assert_eq!(maintained.rows().weight(&before[0]), None);
        assert_eq!(maintained.rows().weight(&empty[0]), Some(&ZWeight::ONE));
        assert_eq!(empty[0].get(0).unwrap().as_count(), Some(0));
        assert!(empty[0].get(1).unwrap().is_null());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn constant_commit_delta_has_bounded_maintenance_on_growing_graph() {
    let ((), report) = run_async_under_lab(0x005c_3555, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let keys = DatabaseKeys::new(
            [0x35; 32],
            DatabaseSecurityNamespaceId([0x36; 32]),
            [0x37; 32],
        );
        let mut db = Database::open_memory(&commit, keys).await.unwrap();
        let query = PreparedGraphAggregateText::prepare(TEXT, symbols)
            .unwrap()
            .bind_parameters(&GqlParameters::new())
            .unwrap();
        let handle = db
            .register_standing_query(&cx, query.clone(), policy())
            .unwrap();
        let mut tenth = None;
        for id in 1..=400 {
            let mut batch = WriteBatch::new(RelationId(1));
            batch.create_vertex(
                VId(id),
                vec![],
                vec![(P, CanonicalScalar::Int(1)), (N, CanonicalScalar::Int(7))],
            );
            let at = db.write(&commit, batch).await.unwrap();
            let maintained = db.standing_query(&cx, &handle).unwrap();
            assert_eq!(maintained.frontier(), at);
            let stats = maintained.last_maintenance();
            assert_eq!(stats.affected_vertices, 1);
            assert!(stats.work_units > 0, "maintenance must charge its visits");
            let full = db
                .execute_graph_aggregate_governed(&cx, &query, policy())
                .unwrap();
            assert_eq!(maintained.rows().len(), full.value.len());
            for row in &full.value {
                assert_eq!(maintained.rows().weight(row), Some(&ZWeight::ONE));
            }
            assert_eq!(
                full.value[0].get(0).unwrap().as_count(),
                Some(u64::try_from(id).unwrap())
            );
            assert_eq!(
                full.value[0].get(1).unwrap().as_integer(),
                Some((i128::from(u64::try_from(id).unwrap())) * 7)
            );
            if id == 10 {
                tenth = Some((stats.delta_rows, stats.work_units, stats.scratch_entries));
            }
            if id == 400 {
                let (rows, work, scratch) = tenth.unwrap();
                assert_eq!(stats.delta_rows, rows);
                // Same one-row delta, same existing aggregate group, same
                // integer widths. No graph/history visit is needed at either
                // frontier; constant slack permits fixed publication work.
                assert!(
                    stats.work_units <= work + 16,
                    "commit 400 work={} versus commit 10={work}",
                    stats.work_units
                );
                assert!(stats.scratch_entries <= scratch + 4);
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
