//! Source-admission laws over actual committed roots and their stored objects.
//! Corruption is injected only into this test's private MemVfs. An unrestricted
//! negative control must still detect it: quota refusal is not source admission.

use super::*;
use asupersync::fs::{OpenOptions, Vfs};
use asupersync::io::{AsyncReadExt, AsyncWriteExt};
use fgdb_strata::store::{RootReadError, RootReadLimits};
use fgdb_strata::{DeltaBlockVersion, PartitionRootVersion};
use fgdb_types::ObjectId;

async fn encoded(db: &Database<MemVfs>, cx: &QueryCx, id: ObjectId) -> Vec<u8> {
    cx.with_restriction_async(async {
        let mut file = db
            .vfs
            .open(&db.store.path(id), &OpenOptions::new().read(true))
            .await
            .unwrap();
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).await.unwrap();
        bytes
    })
    .await
}

async fn damage(db: &Database<MemVfs>, cx: &QueryCx, id: ObjectId) {
    cx.with_restriction_async(async {
        let mut file = db
            .vfs
            .open(&db.store.path(id), &OpenOptions::new().write(true))
            .await
            .unwrap();
        // Overwrite the magic in the fixture's private VFS, not the directory
        // or a root reference. The root still names its authentic original ID.
        file.write_all(&[0xff]).await.unwrap();
    })
    .await;
}

fn unweighted_fixture() -> WriteBatch {
    let mut batch = WriteBatch::new(RelationId(1));
    for vertex in 1..=4 {
        batch.create_vertex(VId(vertex), vec![LabelId(1)], vec![]);
    }
    batch.add_edge(EId(1), VId(1), VId(2), vec![]);
    batch.add_edge(EId(2), VId(2), VId(3), vec![]);
    batch
}

#[test]
fn bounded_reopen_preserves_complete_adjacency_and_properties_across_history() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let runtime_root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&runtime_root);
    runtime.block_on(async {
        let commit = contexts.commit();
        let query = contexts.query();
        let mut db = Database::<MemVfs>::open_memory(&commit, keys())
            .await
            .unwrap();
        db.write(&commit, small_fixture()).await.unwrap();
        let old = db.read_session().unwrap();
        let mut changes = WriteBatch::new(RelationId(1));
        changes.set_edge_property(EId(1), PropertyKeyId(1), Some(CanonicalScalar::Int(7)));
        changes.delete_edge(EId(2));
        changes.create_vertex(VId(4), vec![LabelId(1)], vec![]);
        db.write(&commit, changes).await.unwrap();
        let current = db.read_session().unwrap();
        for cut in [old.frontier(), current.frontier()] {
            let expected = db
                .store
                .reopen(&query, current.partition_root())
                .await
                .unwrap();
            let (root, blocks, props) = db
                .store
                .reopen_adjacency_bounded(
                    &query,
                    current.partition_root(),
                    cut,
                    RootReadLimits::default(),
                )
                .await
                .unwrap();
            assert_eq!(root, expected.0);
            assert_eq!(blocks, expected.1);
            assert_eq!(props, expected.2);
            assert!(!expected.3.is_empty()); // validated, not returned by the bounded API
            let limits = SealedLimits::default();
            let image = db
                .store
                .seal_partition_with_source_limits(
                    &query,
                    current.partition_root(),
                    cut,
                    limits,
                    RootReadLimits::default(),
                )
                .await
                .unwrap();
            let opts = FnxReadOptions {
                as_of: Some(cut),
                ..options()
            };
            let actual = current
                .execute_fnx_sealed(&query, &image, &dijkstra_call(), opts, memory())
                .unwrap();
            let decoded = current.execute_fnx(&query, &dijkstra_call(), opts).unwrap();
            assert_eq!(actual.analytics.rows, decoded.analytics.rows);
            assert_eq!(
                actual.analytics.certificate.result_digest,
                decoded.analytics.certificate.result_digest
            );
            assert_eq!(image.anchor().scope().source_root, current.partition_root());
        }
    });
}

#[test]
fn raw_source_counts_and_exact_encoded_bytes_are_admitted_independently() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let runtime_root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&runtime_root);
    runtime.block_on(async {
        let commit = contexts.commit();
        let query = contexts.query();
        let mut db = Database::<MemVfs>::open_memory(&commit, keys())
            .await
            .unwrap();
        db.write(&commit, unweighted_fixture()).await.unwrap();
        let view = db.read_session().unwrap();
        let (root, blocks, props, patches) = db
            .store
            .reopen(&query, view.partition_root())
            .await
            .unwrap();
        assert!(props.iter().all(Option::is_none)); // no uncounted property objects in this fixture
        let mut source_bytes = 0;
        for reference in &root.blocks {
            source_bytes += encoded(&db, &query, reference.block_id).await.len();
        }
        for reference in &root.vertex_patches {
            source_bytes += encoded(&db, &query, reference.patch_id).await.len();
        }
        let exact = RootReadLimits {
            max_root_bytes: encoded(&db, &query, view.partition_root().0).await.len(),
            max_source_bytes: source_bytes,
            max_blocks: root.blocks.len(),
            max_vertex_patches: root.vertex_patches.len(),
            max_incidences: blocks.iter().map(Vec::len).sum(),
            max_vertex_versions: patches.iter().map(|rows| rows.len()).sum(),
        };
        assert!(exact.max_incidences > 0 && exact.max_vertex_versions > 0 && source_bytes > 0);
        db.store
            .reopen_adjacency_bounded(&query, view.partition_root(), view.frontier(), exact)
            .await
            .unwrap();
        for (tight, resource) in [
            (
                RootReadLimits {
                    max_source_bytes: exact.max_source_bytes - 1,
                    ..exact
                },
                "source encoded bytes",
            ),
            (
                RootReadLimits {
                    max_blocks: exact.max_blocks - 1,
                    ..exact
                },
                "source blocks",
            ),
            (
                RootReadLimits {
                    max_vertex_patches: exact.max_vertex_patches - 1,
                    ..exact
                },
                "source vertex patches",
            ),
            (
                RootReadLimits {
                    max_incidences: exact.max_incidences - 1,
                    ..exact
                },
                "source incidences",
            ),
            (
                RootReadLimits {
                    max_vertex_versions: exact.max_vertex_versions - 1,
                    ..exact
                },
                "source vertex versions",
            ),
        ] {
            assert!(matches!(db.store.reopen_adjacency_bounded(
                &query, view.partition_root(), view.frontier(), tight,
            ).await, Err(RootReadError::Limit { resource: actual, .. }) if actual == resource));
        }
        assert!(matches!(
            db.store
                .reopen_adjacency_bounded(
                    &query,
                    view.partition_root(),
                    view.frontier(),
                    RootReadLimits {
                        max_root_bytes: exact.max_root_bytes - 1,
                        ..exact
                    },
                )
                .await,
            Err(RootReadError::Store(_))
        ));
        // None of these reads mutate the store or turn a refusal into a permit.
        db.store
            .reopen_adjacency_bounded(&query, view.partition_root(), view.frontier(), exact)
            .await
            .unwrap();
    });
}

#[test]
fn incidence_and_reference_limits_stop_before_a_corrupt_later_payload() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let runtime_root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&runtime_root);
    runtime.block_on(async {
        let commit = contexts.commit();
        let query = contexts.query();
        let mut db = Database::<MemVfs>::open_memory(&commit, keys())
            .await
            .unwrap();
        db.write(&commit, small_fixture()).await.unwrap();
        let view = db.read_session().unwrap();
        let root = db
            .store
            .get_root(&query, view.partition_root())
            .await
            .unwrap();
        assert!(root.blocks.len() >= 2);
        assert_ne!(
            root.blocks[0].block_id,
            root.blocks.last().unwrap().block_id
        );
        damage(&db, &query, root.blocks.last().unwrap().block_id).await;
        // Old eager reopen must encounter the damaged later object. This
        // negative control prevents a quota test from hiding missing admission.
        assert!(
            db.store
                .reopen(&query, view.partition_root())
                .await
                .is_err()
        );
        assert!(matches!(
            db.store
                .seal_partition(
                    &query,
                    view.partition_root(),
                    view.frontier(),
                    SealedLimits {
                        max_incidences: 0,
                        ..SealedLimits::default()
                    },
                )
                .await,
            Err(SealedError::Limit {
                resource: "source incidences",
                ..
            })
        ));
        assert!(matches!(
            db.store
                .reopen_adjacency_bounded(
                    &query,
                    view.partition_root(),
                    view.frontier(),
                    RootReadLimits {
                        max_blocks: 0,
                        ..RootReadLimits::default()
                    },
                )
                .await,
            Err(RootReadError::Limit {
                resource: "source blocks",
                ..
            })
        ));
        assert!(matches!(
            db.store
                .seal_partition(
                    &query,
                    view.partition_root(),
                    CommitSeq(view.frontier().0 + 1),
                    SealedLimits::default(),
                )
                .await,
            Err(SealedError::InvalidFloor)
        ));
        assert!(matches!(
            db.store
                .reopen_adjacency_bounded(
                    &query,
                    view.partition_root(),
                    view.frontier(),
                    RootReadLimits::default(),
                )
                .await,
            Err(RootReadError::Store(_))
        ));
    });
}

#[test]
fn hosted_property_bytes_are_charged_before_their_decode() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let runtime_root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&runtime_root);
    runtime.block_on(async {
        let commit = contexts.commit();
        let query = contexts.query();
        let mut db = Database::<MemVfs>::open_memory(&commit, keys())
            .await
            .unwrap();
        db.write(&commit, small_fixture()).await.unwrap();
        let view = db.read_session().unwrap();
        let (root, _, props, _) = db
            .store
            .reopen(&query, view.partition_root())
            .await
            .unwrap();
        assert!(props[0].is_some());
        let first_bytes = db
            .store
            .get_bytes(&query, DeltaBlockVersion(root.blocks[0].block_id))
            .await
            .unwrap()
            .len();
        // At this exact boundary ObjectStart rejects BEFORE loading the hosted
        // patch; charging only block bytes would wrongly reach a later object.
        assert!(matches!(db.store.reopen_adjacency_bounded(
            &query, view.partition_root(), view.frontier(),
            RootReadLimits { max_source_bytes: first_bytes, ..RootReadLimits::default() },
        ).await, Err(RootReadError::Limit {
            resource: "source encoded bytes", requested, limit,
        }) if requested == first_bytes + 1 && limit == first_bytes));
    });
}

#[test]
fn adjacency_only_reopen_does_not_skip_vertex_history_validation() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let runtime_root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&runtime_root);
    runtime.block_on(async {
        let commit = contexts.commit();
        let query = contexts.query();
        let mut db = Database::<MemVfs>::open_memory(&commit, keys())
            .await
            .unwrap();
        let mut batch = WriteBatch::new(RelationId(1));
        batch.create_vertex(VId(u128::MAX), vec![], vec![]);
        db.write(&commit, batch).await.unwrap();
        let view = db.read_session().unwrap();
        let root = db
            .store
            .get_root(&query, view.partition_root())
            .await
            .unwrap();
        assert!(root.blocks.is_empty());
        assert!(!root.vertex_patches.is_empty());
        assert!(matches!(
            db.store
                .reopen_adjacency_bounded(
                    &query,
                    view.partition_root(),
                    view.frontier(),
                    RootReadLimits {
                        max_vertex_versions: 0,
                        ..RootReadLimits::default()
                    },
                )
                .await,
            Err(RootReadError::Limit {
                resource: "source vertex versions",
                ..
            })
        ));
        damage(&db, &query, root.vertex_patches[0].patch_id).await;
        assert!(matches!(
            db.store
                .reopen_adjacency_bounded(
                    &query,
                    view.partition_root(),
                    view.frontier(),
                    RootReadLimits::default(),
                )
                .await,
            Err(RootReadError::Store(_))
        ));
        assert!(matches!(
            db.store
                .seal_partition(
                    &query,
                    view.partition_root(),
                    view.frontier(),
                    SealedLimits::default(),
                )
                .await,
            Err(SealedError::Store(_))
        ));
    });
}

#[test]
fn cancellation_at_every_source_and_sealing_checkpoint_never_returns_a_prefix() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let runtime_root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&runtime_root);
    runtime.block_on(async {
        let commit = contexts.commit();
        let query = contexts.query();
        let mut db = Database::<MemVfs>::open_memory(&commit, keys())
            .await
            .unwrap();
        db.write(&commit, small_fixture()).await.unwrap();
        let view = db.read_session().unwrap();
        let probe = Arc::new(SimulationCheckpointProbe::new(None));
        let controlled = query.with_checkpoint_probe(Arc::clone(&probe));
        let baseline = db
            .store
            .reopen_adjacency_bounded(
                &controlled,
                view.partition_root(),
                view.frontier(),
                RootReadLimits::default(),
            )
            .await
            .unwrap();
        let source_checkpoints = probe.calls();
        assert!(source_checkpoints > 4);
        for stop in 1..=source_checkpoints {
            let probe = Arc::new(SimulationCheckpointProbe::new(Some(stop)));
            let controlled = query.with_checkpoint_probe(Arc::clone(&probe));
            assert!(matches!(
                db.store
                    .reopen_adjacency_bounded(
                        &controlled,
                        view.partition_root(),
                        view.frontier(),
                        RootReadLimits::default(),
                    )
                    .await,
                Err(RootReadError::Interrupted(_))
            ));
            assert_eq!(probe.calls(), stop);
        }
        let probe = Arc::new(SimulationCheckpointProbe::new(None));
        let controlled = query.with_checkpoint_probe(Arc::clone(&probe));
        let baseline_image = db
            .store
            .seal_partition(
                &controlled,
                view.partition_root(),
                view.frontier(),
                SealedLimits::default(),
            )
            .await
            .unwrap();
        let sealing_checkpoints = probe.calls();
        assert!(sealing_checkpoints > source_checkpoints);
        for stop in 1..=sealing_checkpoints {
            let probe = Arc::new(SimulationCheckpointProbe::new(Some(stop)));
            let controlled = query.with_checkpoint_probe(Arc::clone(&probe));
            assert!(matches!(
                db.store
                    .seal_partition(
                        &controlled,
                        view.partition_root(),
                        view.frontier(),
                        SealedLimits::default(),
                    )
                    .await,
                Err(SealedError::Interrupted(_))
            ));
            assert_eq!(probe.calls(), stop);
        }
        let after = db
            .store
            .reopen_adjacency_bounded(
                &query,
                view.partition_root(),
                view.frontier(),
                RootReadLimits::default(),
            )
            .await
            .unwrap();
        assert_eq!(after, baseline);
        let image = db
            .store
            .seal_partition(
                &query,
                view.partition_root(),
                view.frontier(),
                SealedLimits::default(),
            )
            .await
            .unwrap();
        assert_eq!(image.anchor(), baseline_image.anchor());
        assert_eq!(contexts.outstanding_obligations(), 0);
    });
}

#[test]
fn explicit_source_profile_cannot_bypass_the_legacy_incidence_ceiling() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let runtime_root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&runtime_root);
    runtime.block_on(async {
        let commit = contexts.commit();
        let query = contexts.query();
        let mut db = Database::<MemVfs>::open_memory(&commit, keys())
            .await
            .unwrap();
        db.write(&commit, small_fixture()).await.unwrap();
        let view = db.read_session().unwrap();
        for (source, output) in [(0, usize::MAX), (usize::MAX, 0)] {
            assert!(matches!(
                db.store
                    .seal_partition_with_source_limits(
                        &query,
                        view.partition_root(),
                        view.frontier(),
                        SealedLimits {
                            max_incidences: output,
                            ..SealedLimits::default()
                        },
                        RootReadLimits {
                            max_incidences: source,
                            ..RootReadLimits::default()
                        },
                    )
                    .await,
                Err(SealedError::Limit {
                    resource: "source incidences",
                    ..
                })
            ));
        }
        let empty = Database::<MemVfs>::open_memory(&commit, keys())
            .await
            .unwrap();
        let empty_view = empty.read_session().unwrap();
        let source = RootReadLimits {
            max_source_bytes: 0,
            max_blocks: 0,
            max_vertex_patches: 0,
            max_incidences: 0,
            max_vertex_versions: 0,
            ..RootReadLimits::default()
        };
        let image = empty
            .store
            .seal_partition_with_source_limits(
                &query,
                empty_view.partition_root(),
                empty_view.frontier(),
                SealedLimits::default(),
                source,
            )
            .await
            .unwrap();
        assert_eq!(image.stats().incidences, 0);
        // Still authenticate the empty root. A random identity is no permit.
        assert!(
            empty
                .store
                .reopen_adjacency_bounded(
                    &query,
                    PartitionRootVersion(ObjectId([0xff; 32])),
                    empty_view.frontier(),
                    source,
                )
                .await
                .is_err()
        );
    });
}

fn without_vertex_index(view: &EmbeddedReadView) -> EmbeddedReadView {
    // Only discard this test copy's derived index. Its already admitted rows,
    // source root, historical cut and adjacency index remain exactly the same.
    let mut snapshot = (*view.snapshot).clone();
    snapshot.property_index = std::sync::OnceLock::new();
    let mut raw = view.clone();
    raw.snapshot = Arc::new(snapshot);
    raw
}

fn assert_indexed_vertex_projection(
    view: &EmbeddedReadView,
    query: &QueryCx,
    options: FnxReadOptions,
) -> SnapshotGraphView {
    let raw = without_vertex_index(view);
    let build = |source: &EmbeddedReadView| {
        source
            .build_prism_projection_at(
                query,
                options.as_of.unwrap_or(source.frontier()),
                options.selection,
                options.projection,
                options.projection_limits,
                options.source_limits,
            )
            .unwrap()
    };
    let expected = build(&raw);
    assert!(raw.snapshot.property_index.get().is_none());
    let graph = build(view);
    assert_eq!(graph.binding(), expected.binding());
    assert_eq!(graph.spec(), expected.spec());
    assert_eq!(graph.digest(), expected.digest());
    assert_eq!(graph.vertex_ids(), expected.vertex_ids());
    assert_eq!(graph.input_edge_count(), expected.input_edge_count());
    assert_eq!(graph.incoming_face(), expected.incoming_face());
    for row in 0..graph.vertex_ids().len() {
        assert_eq!(graph.projected_row(row), expected.projected_row(row));
    }
    let statement = if options.projection.directedness == Directedness::Undirected {
        "CALL fnx.connected_components() YIELD vertex,component"
    } else {
        "CALL fnx.weakly_connected_components() YIELD vertex,component"
    };
    let call = FnxCallSpec::bind(statement, &FnxParameters::new()).unwrap();
    assert_eq!(
        call.execute(&graph, options.execution_limits, || query.checkpoint())
            .unwrap(),
        call.execute(&expected, options.execution_limits, || query.checkpoint())
            .unwrap(),
        "complete rows, witnesses and result/projection digests must agree"
    );
    graph
}

#[test]
fn indexed_vertex_projection_preserves_labels_history_and_pinned_generations() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let runtime_root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&runtime_root);
    runtime.block_on(async {
        let commit = contexts.commit();
        let query = contexts.query();
        let mut db = Database::<MemVfs>::open_memory(&commit, keys())
            .await
            .unwrap();
        db.write(&commit, fixture()).await.unwrap();
        let pinned = db.read_session().unwrap();
        let mut opt = options();
        opt.selection.weight = FnxWeightSpec::Unit;
        opt.projection.parallel_edges = ParallelEdgePolicy::CollapseUnit;
        let cold = without_vertex_index(&pinned);
        assert_indexed_vertex_projection(&cold, &query, opt);
        assert!(
            cold.snapshot.property_index.get().is_none(),
            "selection must not build an index"
        );
        let old_index = Arc::clone(pinned.snapshot.property_index());
        let before = assert_indexed_vertex_projection(&pinned, &query, opt);

        let mut changed = WriteBatch::new(RelationId(1));
        changed.set_vertex_property(VId(1), PropertyKeyId(2), Some(CanonicalScalar::Int(7)));
        changed.set_vertex_label(VId(3), LabelId(1), false);
        changed.set_vertex_label(VId(3), LabelId(2), true);
        changed.delete_vertex(VId(12)); // isolated vertex retirement
        changed.create_vertex(VId(0), vec![LabelId(2)], vec![]);
        db.write(&commit, changed).await.unwrap();
        let updated = db.read_session().unwrap();
        assert!(updated.snapshot.property_index.get().is_some());
        assert!(!Arc::ptr_eq(updated.snapshot.property_index(), &old_index));
        for view in [&pinned, &updated] {
            for cut in [CommitSeq(0), pinned.frontier(), view.frontier()] {
                for label in [None, Some(LabelId(1)), Some(LabelId(2)), Some(LabelId(404))] {
                    for direction in [
                        Directedness::Directed,
                        Directedness::Reversed,
                        Directedness::Undirected,
                    ] {
                        let mut selected = opt;
                        selected.as_of = Some(cut);
                        selected.selection.vertex_label = label;
                        selected.projection.directedness = direction;
                        let graph = assert_indexed_vertex_projection(view, &query, selected);
                        if cut == CommitSeq(0) || label == Some(LabelId(404)) {
                            assert!(graph.vertex_ids().is_empty());
                        }
                    }
                }
            }
        }
        let current = assert_indexed_vertex_projection(&updated, &query, opt);
        assert!(!current.vertex_ids().contains(&VId(3)));
        assert!(!current.vertex_ids().contains(&VId(12)));
        assert!(before.vertex_ids().contains(&VId(3)));
        assert!(before.vertex_ids().contains(&VId(12)));
        assert!(current.vertex_ids().contains(&VId(u128::MAX)));
        db.compact(&commit).await.unwrap();
        let compacted = db.read_session().unwrap();
        assert!(compacted.snapshot.property_index.get().is_some());
        assert_indexed_vertex_projection(&compacted, &query, opt);
        assert_eq!(
            assert_indexed_vertex_projection(&pinned, &query, opt).digest(),
            before.digest(),
            "updates and compaction cannot replace a pinned generation's history"
        );
        assert!(Arc::ptr_eq(pinned.snapshot.property_index(), &old_index));
        assert_eq!(contexts.outstanding_obligations(), 0);
    });
}

#[test]
fn indexed_vertex_projection_enforces_source_limits_and_every_cancellation() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let runtime_root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&runtime_root);
    runtime.block_on(async {
        let commit = contexts.commit();
        let query = contexts.query();
        let mut db = Database::<MemVfs>::open_memory(&commit, keys())
            .await
            .unwrap();
        db.write(&commit, small_fixture()).await.unwrap();
        let mut changed = WriteBatch::new(RelationId(1));
        changed.set_vertex_label(VId(3), LabelId(1), false);
        changed.set_vertex_property(VId(1), PropertyKeyId(2), Some(CanonicalScalar::Int(7)));
        db.write(&commit, changed).await.unwrap();
        let view = db.read_session().unwrap();
        view.snapshot.property_index();
        let opt = options();
        let (mut work, mut scratch) = (0, 0);
        let expected = view
            .select_prism_vertices(
                view.frontier(),
                opt.selection,
                opt.projection_limits,
                opt.source_limits,
                &mut |event| {
                    match event {
                        SourceEvent::Work | SourceEvent::SnapshotRecord => work += 1,
                        SourceEvent::ScratchEntry => scratch += 1,
                    }
                    Ok(())
                },
                &mut 0,
            )
            .unwrap();
        assert_eq!(expected, [VId(1), VId(2)]);
        assert!(work > 0 && scratch > 0);
        let exact = FnxSourceLimits {
            max_work_units: work,
            max_scratch_entries: scratch,
            ..opt.source_limits
        };
        let select = |limits| {
            view.select_prism_vertices(
                view.frontier(),
                opt.selection,
                opt.projection_limits,
                limits,
                &mut source_control(&query, limits),
                &mut 0,
            )
        };
        assert_eq!(select(exact).unwrap(), expected);
        for (tight, resource) in [
            (
                FnxSourceLimits {
                    max_work_units: work - 1,
                    ..exact
                },
                "work units",
            ),
            (
                FnxSourceLimits {
                    max_scratch_entries: scratch - 1,
                    ..exact
                },
                "scratch entries",
            ),
            (
                FnxSourceLimits {
                    max_staging_bytes: 0,
                    ..exact
                },
                "staging bytes",
            ),
        ] {
            assert!(matches!(select(tight), Err(FnxReadError::SourceLimit {
                resource: actual, ..
            }) if actual == resource));
        }

        let build = |cx: &QueryCx| {
            view.build_prism_projection_at(
                cx,
                view.frontier(),
                opt.selection,
                opt.projection,
                opt.projection_limits,
                opt.source_limits,
            )
        };
        let probe = Arc::new(SimulationCheckpointProbe::new(None));
        let controlled = query.with_checkpoint_probe(Arc::clone(&probe));
        let complete = build(&controlled).unwrap();
        let checkpoints = probe.calls();
        assert!(u64::try_from(checkpoints).unwrap() > work);
        for stop in 1..=checkpoints {
            let probe = Arc::new(SimulationCheckpointProbe::new(Some(stop)));
            let controlled = query.with_checkpoint_probe(Arc::clone(&probe));
            assert!(matches!(
                build(&controlled),
                Err(FnxReadError::Cancelled(_))
            ));
            assert_eq!(probe.calls(), stop);
        }
        assert_eq!(build(&query).unwrap().digest(), complete.digest());
        assert_eq!(select(exact).unwrap(), expected);
        assert_eq!(contexts.outstanding_obligations(), 0);
    });
}
