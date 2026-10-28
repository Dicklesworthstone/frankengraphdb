//! Exercise real Chronicle writes and the admitted Strata generation, not a
//! graph constructed solely for the adapter. Every assertion uses public APIs.

use asupersync::{Budget, runtime::RuntimeBuilder};
use fgdb::{Database, DatabaseKeys, MemVfs, ReadCacheLimits, WriteBatch};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_prism::*;
use fgdb_types::{
    CanonicalScalar, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId,
};

/// The owner-path projection cache is process-wide and small, and these
/// tests fill it and observe reuse. Under the parallel test runner another
/// test's live snapshots can fill it or hold its lock, so a lawful rebuild
/// looked like a missing reuse (fgdb-7fqu6). Each test runs alone; a finished
/// test's entries die with its database.
static CACHE: std::sync::Mutex<()> = std::sync::Mutex::new(());
fn exclusive() -> std::sync::MutexGuard<'static, ()> {
    CACHE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x31; 32],
        DatabaseSecurityNamespaceId([0x32; 32]),
        [0x33; 32],
    )
}
fn options() -> FnxReadOptions {
    FnxReadOptions {
        as_of: None,
        selection: FnxSelection {
            vertex_label: Some(LabelId(1)),
            relation: Some(RelationId(1)),
            weight: FnxWeightSpec::Property {
                key: PropertyKeyId(1),
                missing: MissingWeightPolicy::Reject,
            },
        },
        projection: ProjectionSpec {
            directedness: Directedness::Directed,
            parallel_edges: ParallelEdgePolicy::Sum,
            self_loops: SelfLoopPolicy::Keep,
        },
        source_limits: FnxSourceLimits {
            max_work_units: 100_000,
            max_scratch_entries: 10_000,
            max_staging_bytes: 1 << 22,
        },
        projection_limits: ProjectionLimits {
            max_vertices: 100,
            max_input_edges: 1000,
            max_adjacency_entries: 2000,
            max_workspace_bytes: 1 << 22,
        },
        execution_limits: FnxExecutionLimits {
            max_iterations: 1000,
            max_result_rows: 100,
            max_estimated_work: 1 << 24,
        },
    }
}
fn fixture() -> WriteBatch {
    let mut batch = WriteBatch::new(RelationId(1));
    for vertex in [1, 2, 3] {
        batch.create_vertex(VId(vertex), vec![LabelId(1)], vec![]);
    }
    batch.create_vertex(VId(99), vec![LabelId(2)], vec![]);
    batch.add_edge(
        EId(10),
        VId(1),
        VId(2),
        vec![(PropertyKeyId(1), CanonicalScalar::Int(2))],
    );
    batch.add_edge(
        EId(11),
        VId(1),
        VId(2),
        vec![(PropertyKeyId(1), CanonicalScalar::Int(5))],
    );
    batch.add_edge(
        EId(12),
        VId(2),
        VId(1),
        vec![(PropertyKeyId(1), CanonicalScalar::Int(3))],
    );
    // Excluded by the induced vertex-label projection BEFORE weight binding.
    batch.add_edge(
        EId(13),
        VId(2),
        VId(99),
        vec![(PropertyKeyId(1), CanonicalScalar::Bool(true))],
    );
    batch
}
fn call() -> FnxCallSpec {
    FnxCallSpec::bind(
        "CALL fnx.pagerank(0.85,1000,1e-12) YIELD vertex,score",
        &FnxParameters::new(),
    )
    .unwrap()
}

#[test]
fn database_calls_select_real_multigraph_rows_without_losing_isolates() {
    let _cache = exclusive();
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let commit = contexts.commit();
        let query = contexts.query();
        let mut db = Database::<MemVfs>::open_memory(&commit, keys())
            .await
            .unwrap();
        db.write(&commit, fixture()).await.unwrap();
        let options = options();
        let view = db.read_session().unwrap();
        let projection = view
            .prism_projection_at(
                &query,
                view.frontier(),
                options.selection,
                options.projection,
                options.projection_limits,
                options.source_limits,
            )
            .unwrap();
        assert_eq!(projection.vertex_ids(), &[VId(1), VId(2), VId(3)]);
        assert_eq!(projection.projected_weight(0, 1), Some(7.0));
        assert_eq!(projection.projected_weight(1, 0), Some(3.0));
        assert_eq!(projection.neighbors_indices(2), Some(&[][..]));
        assert_eq!(projection.edge_count(), 2);
        assert_eq!(projection.input_edge_count(), 3);
        let result = db
            .call_fnx(
                &query,
                "CALL fnx.pagerank(0.85,1000,1e-12) YIELD vertex,score",
                &FnxParameters::new(),
                options,
            )
            .unwrap();
        assert_eq!(
            result.analytics,
            call()
                .execute(&projection, options.execution_limits, || query.checkpoint())
                .unwrap()
        );
        assert_eq!(
            result.analytics.certificate.snapshot.root,
            view.partition_root().0
        );
        assert_eq!(result.analytics.certificate.snapshot.as_of, view.frontier());
        assert_eq!(result.analytics.rows.len(), 3);
        assert_eq!(result.analytics.rows[2][0], FnxValue::Vertex(VId(3)));
        assert_eq!(result, view.execute_fnx(&query, &call(), options).unwrap());
        assert_eq!(result.selection, options.selection);
    });
}

#[test]
fn retained_view_historical_cut_and_shared_cache_survive_writer_progress_and_drop() {
    let _cache = exclusive();
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let commit = contexts.commit();
        let query = contexts.query();
        let mut db = Database::<MemVfs>::open_memory(&commit, keys())
            .await
            .unwrap();
        db.write(&commit, fixture()).await.unwrap();
        let options = options();
        let old = db.read_session().unwrap();
        let first = old.execute_fnx(&query, &call(), options).unwrap();
        let cache = old
            .prism_projection_at(
                &query,
                old.frontier(),
                options.selection,
                options.projection,
                options.projection_limits,
                options.source_limits,
            )
            .unwrap();
        let mut update = WriteBatch::new(RelationId(1));
        update.add_edge(
            EId(20),
            VId(2),
            VId(3),
            vec![(PropertyKeyId(1), CanonicalScalar::Int(1))],
        );
        db.write(&commit, update).await.unwrap();
        let latest = db.read_session().unwrap();
        assert!(latest.frontier() > old.frontier());
        assert_eq!(first, old.execute_fnx(&query, &call(), options).unwrap());
        let current = latest.execute_fnx(&query, &call(), options).unwrap();
        assert_ne!(first.analytics.rows, current.analytics.rows);
        let historical = latest
            .execute_fnx(
                &query,
                &call(),
                FnxReadOptions {
                    as_of: Some(old.frontier()),
                    ..options
                },
            )
            .unwrap();
        assert_eq!(first.analytics.rows, historical.analytics.rows);
        assert_eq!(
            historical.analytics.certificate.snapshot.as_of,
            old.frontier()
        );
        assert_ne!(
            first.analytics.certificate.snapshot.root,
            historical.analytics.certificate.snapshot.root
        );
        assert_ne!(first.digest, historical.digest);
        assert!(matches!(
            old.execute_fnx(
                &query,
                &call(),
                FnxReadOptions {
                    as_of: Some(latest.frontier()),
                    ..options
                }
            ),
            Err(FnxReadError::Read(_))
        ));
        let clone = cache.clone();
        drop(db);
        drop(old);
        drop(latest);
        assert!(cache.shares_cache_with(&clone));
        assert_eq!(
            first.analytics,
            call()
                .execute(&clone, options.execution_limits, || query.checkpoint())
                .unwrap()
        );
    });
}

#[test]
fn excluded_properties_and_explicit_discard_laws_cannot_change_answers() {
    let _cache = exclusive();
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let commit = contexts.commit();
        let query = contexts.query();
        let mut db = Database::<MemVfs>::open_memory(&commit, keys())
            .await
            .unwrap();
        db.write(&commit, fixture()).await.unwrap();
        let selected = options();
        db.execute_fnx(&query, &call(), selected).unwrap();
        let mut all = selected;
        all.selection.vertex_label = None;
        assert!(matches!(
            db.execute_fnx(&query, &call(), all),
            Err(FnxReadError::Weight {
                edge: EId(13),
                reason: FnxWeightError::NotNumeric
            })
        ));
        all.projection.parallel_edges = ParallelEdgePolicy::CollapseUnit;
        assert_eq!(
            db.execute_fnx(&query, &call(), all)
                .unwrap()
                .analytics
                .rows
                .len(),
            4
        );
        let mut loop_batch = WriteBatch::new(RelationId(1));
        loop_batch.add_edge(EId(21), VId(3), VId(3), vec![]);
        db.write(&commit, loop_batch).await.unwrap();
        assert!(matches!(
            db.execute_fnx(&query, &call(), selected),
            Err(FnxReadError::Weight {
                edge: EId(21),
                reason: FnxWeightError::Missing
            })
        ));
        let mut dropped = selected;
        dropped.projection.self_loops = SelfLoopPolicy::Drop;
        db.execute_fnx(&query, &call(), dropped).unwrap();
        dropped.projection.self_loops = SelfLoopPolicy::Reject;
        assert!(matches!(
            db.execute_fnx(&query, &call(), dropped),
            Err(FnxReadError::Projection(ProjectionError::SelfLoop(EId(21))))
        ));
    });
}

#[test]
fn relation_filter_and_selection_recipe_are_bound_even_when_rows_coincide() {
    let _cache = exclusive();
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let commit = contexts.commit();
        let query = contexts.query();
        let mut db = Database::<MemVfs>::open_memory(&commit, keys())
            .await
            .unwrap();
        db.write(&commit, fixture()).await.unwrap();
        let base = options();
        let first = db.execute_fnx(&query, &call(), base).unwrap();
        let mut other = base;
        other.selection.relation = None; // same edges now, different recipe
        let second = db.execute_fnx(&query, &call(), other).unwrap();
        assert_eq!(first.analytics, second.analytics);
        assert_ne!(first.digest, second.digest);
        other.selection.relation = Some(RelationId(9));
        let isolated = db.execute_fnx(&query, &call(), other).unwrap();
        assert_eq!(isolated.analytics.certificate.edges, 0);
        assert_eq!(isolated.analytics.certificate.vertices, 3);
        for row in &isolated.analytics.rows {
            match row[1] {
                FnxValue::Score(value) => assert!((value - 1.0 / 3.0).abs() < 1e-12),
                _ => panic!("score"),
            }
        }
    });
}

#[test]
fn source_cache_algorithm_and_frontier_limits_preserve_typed_refusals() {
    let _cache = exclusive();
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let commit = contexts.commit();
        let query = contexts.query();
        let mut db = Database::<MemVfs>::open_memory(&commit, keys())
            .await
            .unwrap();
        db.write(&commit, fixture()).await.unwrap();
        for resource in 0..3 {
            let mut budget = options();
            match resource {
                0 => budget.source_limits.max_work_units = 0,
                1 => budget.source_limits.max_scratch_entries = 0,
                _ => budget.source_limits.max_staging_bytes = 0,
            }
            assert!(matches!(
                db.execute_fnx(&query, &call(), budget),
                Err(FnxReadError::SourceLimit { .. })
            ));
        }
        let mut budget = options();
        budget.projection_limits.max_vertices = 2;
        assert!(matches!(
            db.execute_fnx(&query, &call(), budget),
            Err(FnxReadError::Projection(ProjectionError::LimitExceeded {
                resource: "vertices",
                ..
            }))
        ));
        budget = options();
        budget.execution_limits.max_result_rows = 2;
        assert!(matches!(
            db.execute_fnx(&query, &call(), budget),
            Err(FnxReadError::Execution(FnxExecutionError::LimitExceeded {
                resource: "result rows",
                ..
            }))
        ));
        budget = options();
        budget.as_of = Some(CommitSeq(u64::MAX));
        assert!(matches!(
            db.execute_fnx(&query, &call(), budget),
            Err(FnxReadError::Read(_))
        ));
        assert!(matches!(
            db.call_fnx(&query, "CALL fnx.unknown()", &FnxParameters::new(), budget),
            Err(FnxReadError::Bind(_))
        ));
        db.execute_fnx(&query, &call(), options()).unwrap(); // refusals never poison the view
    });
}

#[test]
fn property_successors_and_retirements_resolve_at_the_selected_cut() {
    let _cache = exclusive();
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let commit = contexts.commit();
        let query = contexts.query();
        let mut db = Database::<MemVfs>::open_memory(&commit, keys())
            .await
            .unwrap();
        db.write(&commit, fixture()).await.unwrap();
        let at = db.frontier().unwrap();
        let mut changes = WriteBatch::new(RelationId(1));
        changes.set_edge_property(EId(10), PropertyKeyId(1), Some(CanonicalScalar::Int(10)));
        changes.delete_edge(EId(11));
        changes.delete_vertex(VId(3)); // an isolated vertex, not a dangling-edge shortcut
        db.write(&commit, changes).await.unwrap();
        let view = db.read_session().unwrap();
        let opts = options();
        let historical = view
            .prism_projection_at(
                &query,
                at,
                opts.selection,
                opts.projection,
                opts.projection_limits,
                opts.source_limits,
            )
            .unwrap();
        let current = view
            .prism_projection_at(
                &query,
                view.frontier(),
                opts.selection,
                opts.projection,
                opts.projection_limits,
                opts.source_limits,
            )
            .unwrap();
        assert_eq!(historical.vertex_ids(), &[VId(1), VId(2), VId(3)]);
        assert_eq!(historical.projected_weight(0, 1), Some(7.0));
        assert_eq!(historical.input_edge_count(), 3);
        assert_eq!(current.vertex_ids(), &[VId(1), VId(2)]);
        assert_eq!(current.projected_weight(0, 1), Some(10.0));
        assert_eq!(current.input_edge_count(), 2);
        assert_ne!(historical.digest(), current.digest());
    });
}

#[test]
fn selected_relation_calls_bound_winner_scratch_without_skipping_raw_source_work() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let commit = contexts.commit();
        let query = contexts.query();
        // Creation builds the adjacency index for publication, and every later
        // commit would extend it. Reopening the written checkpoint leaves it
        // unbuilt, as a cold reader sees it, so this stays the raw-source
        // control rather than the indexed path.
        let mut opened = Vec::new();
        for _ in 0..2 {
            let vfs = MemVfs::new().unwrap();
            let path = vfs.database_dir();
            let mut created = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
                .await
                .unwrap();
            created.write(&commit, fixture()).await.unwrap();
            drop(created);
            let database = Database::open_with_vfs(&commit, vfs, &path, keys())
                .await
                .unwrap();
            assert_eq!(database.index_maintenance_work().unwrap().0, 0);
            database
                .set_read_cache_limits(
                    &query,
                    ReadCacheLimits {
                        projection_bytes: 0,
                        ..ReadCacheLimits::default()
                    },
                )
                .unwrap();
            opened.push(database);
        }
        let mut oracle = opened.pop().unwrap();
        let mut db = opened.pop().unwrap();
        // Same vertices, many unrelated edges: their properties would fail
        // numeric binding if the target relation were not selected first.
        let mut other = WriteBatch::new(RelationId(2));
        for id in 1000..1512 {
            other.add_edge(
                EId(id),
                VId(1),
                VId(2),
                vec![(PropertyKeyId(1), CanonicalScalar::Bool(true))],
            );
        }
        db.write(&commit, other).await.unwrap();
        let old = [db.frontier().unwrap(), oracle.frontier().unwrap()];
        for database in [&mut db, &mut oracle] {
            let mut changes = WriteBatch::new(RelationId(1));
            changes.set_edge_property(EId(10), PropertyKeyId(1), Some(CanonicalScalar::Int(10)));
            changes.delete_edge(EId(11));
            database.write(&commit, changes).await.unwrap();
        }
        let now = [db.frontier().unwrap(), oracle.frontier().unwrap()];
        for database in [&db, &oracle] {
            assert_eq!(
                database.index_maintenance_work().unwrap().0,
                0,
                "batch property updates and retirements must leave this raw-source control cold",
            );
        }
        let calls = [
            (call(), Directedness::Directed),
            (
                FnxCallSpec::single_source_shortest_path_length(VId(1), None),
                Directedness::Directed,
            ),
            (
                FnxCallSpec::bind(
                    "CALL fnx.single_source_dijkstra_path_length(1)",
                    &FnxParameters::new(),
                )
                .unwrap(),
                Directedness::Directed,
            ),
            (
                FnxCallSpec::bind("CALL fnx.connected_components()", &FnxParameters::new())
                    .unwrap(),
                Directedness::Undirected,
            ),
        ];
        for cuts in [old, now] {
            for (call, direction) in &calls {
                let mut selected = options();
                selected.as_of = Some(cuts[0]);
                selected.projection.directedness = *direction;
                // Four selected EIds and the small vertex-merge heap fit;
                // the old all-relation merge needed 512 additional winners.
                selected.source_limits.max_scratch_entries = 32;
                let actual = db.execute_fnx(&query, call, selected).unwrap();
                let expected = oracle
                    .execute_fnx(
                        &query,
                        call,
                        FnxReadOptions {
                            as_of: Some(cuts[1]),
                            ..selected
                        },
                    )
                    .unwrap();
                assert_eq!(actual.analytics.rows, expected.analytics.rows);
                assert_eq!(
                    actual.analytics.certificate.result_digest,
                    expected.analytics.certificate.result_digest
                );
            }
            let mut limited = options();
            limited.as_of = Some(cuts[0]);
            limited.source_limits.max_scratch_entries = 32;
            limited.selection.relation = None;
            assert!(matches!(
                db.execute_fnx(&query, &call(), limited),
                Err(FnxReadError::SourceLimit {
                    resource: "scratch entries",
                    ..
                })
            ));
            limited.selection.relation = Some(RelationId(1));
            limited.source_limits.max_work_units = 128;
            assert!(matches!(
                db.execute_fnx(&query, &call(), limited),
                Err(FnxReadError::SourceLimit {
                    resource: "work units",
                    ..
                })
            ));
            // The physically smaller oracle does fit this raw-source budget.
            limited.as_of = Some(cuts[1]);
            oracle.execute_fnx(&query, &call(), limited).unwrap();
        }
        assert_eq!(db.read_cache_status(&query).unwrap().projections.entries, 0);
        assert_eq!(contexts.outstanding_obligations(), 0);
    });
}

#[test]
fn retained_adjacency_projections_equal_cold_reads_across_history_and_reductions() {
    let _cache = exclusive();
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let commit = contexts.commit();
        let query = contexts.query();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        db.write(&commit, fixture()).await.unwrap();
        let mut parallel = WriteBatch::new(RelationId(1));
        parallel.add_edge(
            EId(14),
            VId(2),
            VId(2),
            vec![(PropertyKeyId(1), CanonicalScalar::Int(4))],
        );
        for id in 1000..1512 {
            parallel.add_edge(
                EId(id),
                VId(1),
                VId(2),
                vec![(PropertyKeyId(1), CanonicalScalar::Int(1))],
            );
        }
        db.write(&commit, parallel).await.unwrap();
        let mut other = WriteBatch::new(RelationId(2));
        other.add_edge(
            EId(2000),
            VId(2),
            VId(1),
            vec![(PropertyKeyId(1), CanonicalScalar::Int(8))],
        );
        let first = db.write(&commit, other).await.unwrap();
        // Creation built the index empty and each commit extended it; reopen
        // so the ordinary read below builds it in full, as the comment needs.
        drop(db);
        let mut db = Database::open_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        assert_eq!(db.frontier().unwrap(), first);
        assert_eq!(db.index_maintenance_work().unwrap().0, 0);
        // An ordinary graph read retains the flat adjacency index. Subsequent
        // updates extend it, exercising the persistent overlay as well.
        assert_eq!(db.neighbours(VId(1), RelationId(1)).unwrap(), [VId(2)]);
        assert!(db.index_maintenance_work().unwrap().0 > 0);
        let mut changes = WriteBatch::new(RelationId(1));
        changes.set_edge_property(EId(10), PropertyKeyId(1), Some(CanonicalScalar::Int(10)));
        changes.delete_edge(EId(11));
        let latest = db.write(&commit, changes).await.unwrap();
        assert!(db.index_maintenance_work().unwrap().0 > 0);
        let warm = db.read_session().unwrap();
        // A separate read-only open reconstructs the same admitted root with
        // no retained adjacency. Neither projection may reuse decoded output.
        let cold = Database::open_read_view_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        assert_eq!(cold.partition_root(), warm.partition_root());
        assert_eq!(cold.frontier(), warm.frontier());
        for view in [&warm, &cold] {
            view.set_read_cache_limits(
                &query,
                ReadCacheLimits {
                    projection_bytes: 0,
                    ..ReadCacheLimits::default()
                },
            )
            .unwrap();
        }
        let base = options();
        for at in [first, latest] {
            for relation in [None, Some(RelationId(1)), Some(RelationId(2))] {
                let selection = FnxSelection {
                    relation,
                    ..base.selection
                };
                for directedness in [
                    Directedness::Directed,
                    Directedness::Reversed,
                    Directedness::Undirected,
                ] {
                    for parallel_edges in [
                        ParallelEdgePolicy::Sum,
                        ParallelEdgePolicy::Minimum,
                        ParallelEdgePolicy::CollapseUnit,
                    ] {
                        for self_loops in [SelfLoopPolicy::Keep, SelfLoopPolicy::Drop] {
                            let spec = ProjectionSpec {
                                directedness,
                                parallel_edges,
                                self_loops,
                            };
                            let project = |view: &fgdb::EmbeddedReadView, limits| {
                                view.prism_projection_at(
                                    &query,
                                    at,
                                    selection,
                                    spec,
                                    base.projection_limits,
                                    limits,
                                )
                            };
                            let expected = project(&cold, base.source_limits).unwrap();
                            let actual = project(&warm, base.source_limits).unwrap();
                            assert_eq!(actual.vertex_ids(), &[VId(1), VId(2), VId(3)]);
                            assert_eq!(actual.binding(), expected.binding());
                            assert_eq!(actual.digest(), expected.digest());
                            assert_eq!(actual.input_edge_count(), expected.input_edge_count());
                            assert_eq!(actual.edge_count(), expected.edge_count());
                            for ordinal in 0..actual.vertex_ids().len() {
                                assert_eq!(
                                    actual.projected_row(ordinal),
                                    expected.projected_row(ordinal)
                                );
                            }
                            assert_eq!(actual.incoming_face(), expected.incoming_face());
                            if relation == Some(RelationId(1))
                                && directedness == Directedness::Directed
                                && parallel_edges == ParallelEdgePolicy::Sum
                            {
                                assert_eq!(
                                    actual.projected_weight(0, 1),
                                    Some(if at == first { 519.0 } else { 522.0 }),
                                );
                            }
                            if relation != Some(RelationId(2)) {
                                let tight = FnxSourceLimits {
                                    max_scratch_entries: 64,
                                    ..base.source_limits
                                };
                                assert_eq!(
                                    project(&warm, tight).unwrap().digest(),
                                    expected.digest()
                                );
                                assert!(matches!(
                                    project(&cold, tight),
                                    Err(FnxReadError::SourceLimit {
                                        resource: "scratch entries",
                                        ..
                                    })
                                ));
                            }
                        }
                    }
                }
            }
        }
        assert_eq!(
            warm.read_cache_status(&query).unwrap().projections.entries,
            0
        );
        assert_eq!(
            cold.read_cache_status(&query).unwrap().projections.entries,
            0
        );
        assert_eq!(contexts.outstanding_obligations(), 0);
    });
}

#[test]
fn every_source_and_adapter_checkpoint_can_cancel_without_publishing_results() {
    let _cache = exclusive();
    use fgdb_types::context::SimulationCheckpointProbe;
    use std::sync::Arc;
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let commit = contexts.commit();
        let query = contexts.query();
        // A generation's first call builds its projection and later calls
        // reuse it (fgdb-0tbrz), so the two paths have different checkpoints.
        // Count each on its own path, and stop each at every one of them.
        let counted = |view: &fgdb::EmbeddedReadView| {
            let probe = Arc::new(SimulationCheckpointProbe::new(None));
            let observed = query.with_checkpoint_probe(Arc::clone(&probe));
            let rows = view.execute_fnx(&observed, &call(), options()).unwrap();
            (rows, probe.calls())
        };
        let stopped = |view: &fgdb::EmbeddedReadView, stop| {
            let probe = Arc::new(SimulationCheckpointProbe::new(Some(stop)));
            let controlled = query.with_checkpoint_probe(probe);
            matches!(
                view.execute_fnx(&controlled, &call(), options()),
                Err(FnxReadError::Cancelled(_))
                    | Err(FnxReadError::Execution(FnxExecutionError::Cancelled(_)))
            )
        };
        let mut db = Database::<MemVfs>::open_memory(&commit, keys())
            .await
            .unwrap();
        db.write(&commit, fixture()).await.unwrap();
        let view = db.read_session().unwrap();
        let (expected, building) = counted(&view);
        let (again, reusing) = counted(&view);
        assert_eq!(again, expected);
        assert!(building > 10);
        assert!(
            reusing < building,
            "a reused projection skips the build's checkpoints"
        );
        for stop in 1..=reusing {
            assert!(stopped(&view, stop), "reusing, stop={stop}");
        }
        assert_eq!(counted(&view), (expected.clone(), reusing));
        // A building call needs a generation nothing has projected yet. An
        // interrupted one publishes nothing: the next call still answers.
        for stop in 1..=building {
            let mut fresh = Database::<MemVfs>::open_memory(&commit, keys())
                .await
                .unwrap();
            fresh.write(&commit, fixture()).await.unwrap();
            let view = fresh.read_session().unwrap();
            assert!(stopped(&view, stop), "building, stop={stop}");
            assert_eq!(counted(&view).0, expected, "after stop={stop}");
        }
        assert_eq!(contexts.outstanding_obligations(), 0);
        // The pinned upstream fnx iteration loop has no internal checkpoint;
        // this covers the actual available source and adapter boundaries only.
    });
}
