//! Relation-scoped preparation against the real committed source and the
//! incumbent full-partition sealer. Selection must reduce retained work without
//! turning unselected objects into unchecked source history.

use super::*;
use asupersync::fs::{OpenOptions, Vfs};
use asupersync::io::AsyncWriteExt;
use fgdb_prism::SealedProjectionMask;
use fgdb_strata::edge_props::EdgePropertyRow;
use fgdb_strata::root::{BlockRef, PartitionRoot, RootError};
use fgdb_strata::store::StoreError;
use fgdb_strata::tiered::sealed::{IncomingIndexLimits, SealedIncomingIndex};
use fgdb_strata::{AdjacencyEntry, DeltaBlockVersion, PartitionRootVersion};

fn unrelated(count: usize) -> WriteBatch {
    let mut batch = WriteBatch::new(RelationId(2));
    for at in 0..count {
        batch.add_edge(
            EId(10_000 + at as u128),
            VId(1 + (at % 3) as u128),
            VId(2 + (at % 3) as u128),
            // Valid source properties which would be invalid numeric weights
            // if selection accidentally let them into the target projection.
            vec![(
                PropertyKeyId(1),
                CanonicalScalar::bytes(vec![0x5a; 512]).unwrap(),
            )],
        );
    }
    batch
}

fn outgoing(
    image: &SealedPartition,
    cx: &QueryCx,
    vertex: VId,
    cut: CommitSeq,
) -> Vec<(AdjacencyEntry, EdgePropertyRow)> {
    let mut rows = Vec::new();
    let mut cursor = image.row(cx, vertex, RelationId(1), cut).unwrap();
    while let Some(edge) = cursor.next(cx).unwrap() {
        rows.push((edge.entry, edge.properties.to_vec()));
    }
    rows
}

fn incoming(
    index: &SealedIncomingIndex,
    cx: &QueryCx,
    vertex: VId,
    cut: CommitSeq,
) -> Vec<(AdjacencyEntry, EdgePropertyRow)> {
    let mut rows = Vec::new();
    let mut cursor = index.row(cx, vertex, RelationId(1), cut).unwrap();
    while let Some(edge) = cursor.next(cx).unwrap() {
        rows.push((edge.entry, edge.properties.to_vec()));
    }
    rows
}

#[test]
fn relation_scoped_sealing_preserves_history_properties_directions_and_answers() {
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
        db.write(&commit, unrelated(64)).await.unwrap();
        let first = db.read_session().unwrap().frontier();
        let mut changes = WriteBatch::new(RelationId(1));
        changes.set_edge_property(EId(100), PropertyKeyId(1), Some(CanonicalScalar::Int(3)));
        changes.delete_edge(EId(300));
        db.write(&commit, changes).await.unwrap();
        let view = db.read_session().unwrap();
        let full = db
            .store
            .seal_partition(
                &query,
                view.partition_root(),
                first,
                SealedLimits::default(),
            )
            .await
            .unwrap();
        let scoped = db
            .store
            .seal_relation(
                &query,
                view.partition_root(),
                first,
                RelationId(1),
                SealedLimits::default(),
            )
            .await
            .unwrap();
        assert_eq!(scoped.anchor().scope(), full.anchor().scope());
        assert_eq!(scoped.anchor().relation(), Some(RelationId(1)));
        assert_eq!(full.anchor().relation(), None);
        assert_eq!(full.stats().incidences, scoped.stats().incidences + 64);
        assert_eq!(
            full.stats().property_rows,
            scoped.stats().property_rows + 64
        );
        assert!(scoped.stats().encoded_bytes < full.stats().encoded_bytes);
        let restored = SealedPartition::reload(
            &query,
            scoped.anchor(),
            &scoped.encode(&query, SealedLimits::default()).unwrap(),
            SealedLimits::default(),
            None,
        )
        .unwrap();
        let full_incoming = full
            .incoming_index(&query, IncomingIndexLimits::default())
            .unwrap();
        let scoped_incoming = scoped
            .incoming_index(&query, IncomingIndexLimits::default())
            .unwrap();
        assert_eq!(
            scoped_incoming.stats().incidences,
            scoped.stats().incidences
        );
        assert!(
            scoped_incoming.stats().charged_workspace_bytes
                < full_incoming.stats().charged_workspace_bytes
        );
        for cut in [first, view.frontier()] {
            for vertex in (1..=12).map(VId).chain([VId(99), VId(u128::MAX)]) {
                assert_eq!(
                    outgoing(&scoped, &query, vertex, cut),
                    outgoing(&full, &query, vertex, cut)
                );
                assert_eq!(
                    outgoing(&restored, &query, vertex, cut),
                    outgoing(&full, &query, vertex, cut)
                );
                assert_eq!(
                    incoming(&scoped_incoming, &query, vertex, cut),
                    incoming(&full_incoming, &query, vertex, cut)
                );
            }
            for direction in [
                Directedness::Directed,
                Directedness::Reversed,
                Directedness::Undirected,
            ] {
                let mut opts = options();
                opts.as_of = Some(cut);
                opts.projection.directedness = direction;
                let prepare = |image: &SealedPartition| {
                    view.prism_sealed_projection_at(
                        &query,
                        image,
                        cut,
                        opts.selection,
                        opts.projection,
                        opts.projection_limits,
                        opts.source_limits,
                    )
                    .unwrap()
                };
                let whole = prepare(&full);
                let selected = prepare(&scoped);
                assert_eq!(selected.binding(), whole.binding());
                assert_eq!(selected.digest(), whole.digest());
                let call = dijkstra_call();
                let expected = view.execute_fnx(&query, &call, opts).unwrap().analytics;
                for image in [&full, &scoped, &restored] {
                    let actual = call
                        .execute_sealed(&query, &prepare(image), opts.execution_limits, memory())
                        .unwrap();
                    assert_eq!(actual.rows, expected.rows, "{direction:?} at {cut:?}");
                    assert_eq!(
                        actual.certificate.result_digest,
                        expected.certificate.result_digest
                    );
                }
            }
        }
    });
}

#[test]
fn selected_image_and_raw_source_have_independent_exact_limits() {
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
        db.write(&commit, unrelated(128)).await.unwrap();
        let view = db.read_session().unwrap();
        let (_, blocks, _, _) = db
            .store
            .reopen(&query, view.partition_root())
            .await
            .unwrap();
        let raw = blocks.iter().map(Vec::len).sum();
        let baseline = db
            .store
            .seal_relation(
                &query,
                view.partition_root(),
                view.frontier(),
                RelationId(1),
                SealedLimits::default(),
            )
            .await
            .unwrap();
        let stats = baseline.stats();
        assert!(raw > stats.incidences);
        let sealing = SealedLimits {
            max_rows: stats.rows,
            max_incidences: stats.incidences,
            max_image_bytes: stats.encoded_bytes,
            max_property_bytes: 512,
        };
        let source = RootReadLimits {
            max_incidences: raw,
            ..RootReadLimits::default()
        };
        // The full-partition API keeps its historical contract: the image
        // allowance also caps raw input even if the explicit source cap is larger.
        assert!(matches!(
            db.store
                .seal_partition_with_source_limits(
                    &query,
                    view.partition_root(),
                    view.frontier(),
                    sealing,
                    source,
                )
                .await,
            Err(SealedError::Limit {
                resource: "source incidences",
                ..
            })
        ));
        for direction in [
            Directedness::Directed,
            Directedness::Reversed,
            Directedness::Undirected,
        ] {
            let mut opts = options();
            opts.projection.directedness = direction;
            opts.projection_limits.max_input_edges = stats.incidences;
            let graph = db
                .prism_sealed_projection_with_source_limits(
                    &query,
                    None,
                    opts.selection,
                    opts.projection,
                    opts.projection_limits,
                    opts.source_limits,
                    sealing,
                    source,
                )
                .await
                .unwrap();
            assert_eq!(graph.scan_incidence_bound(), stats.incidences);
            if let Some(incoming) = graph.incoming_index_stats() {
                assert_eq!(incoming.incidences, stats.incidences);
            }
            // The ordinary host entrypoint also selects before sealing. Its
            // default raw budget must not shrink to this small image budget.
            let call = dijkstra_call();
            let actual = db
                .execute_fnx_sealed(&query, &call, opts, memory(), sealing)
                .await
                .unwrap();
            let expected = call
                .execute_sealed(&query, &graph, opts.execution_limits, memory())
                .unwrap();
            assert_eq!(actual.analytics, expected);
        }
        assert!(matches!(
            db.store
                .seal_relation_with_source_limits(
                    &query,
                    view.partition_root(),
                    view.frontier(),
                    RelationId(1),
                    sealing,
                    RootReadLimits {
                        max_incidences: raw - 1,
                        ..source
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
                .seal_relation_with_source_limits(
                    &query,
                    view.partition_root(),
                    view.frontier(),
                    RelationId(1),
                    SealedLimits {
                        max_incidences: stats.incidences - 1,
                        ..sealing
                    },
                    source,
                )
                .await,
            Err(SealedError::Limit {
                resource: "incidences",
                ..
            })
        ));
        let empty_limits = SealedLimits {
            max_rows: 0,
            max_incidences: 0,
            max_property_bytes: 0,
            ..SealedLimits::default()
        };
        let empty = db
            .store
            .seal_relation_with_source_limits(
                &query,
                view.partition_root(),
                view.frontier(),
                RelationId(99),
                empty_limits,
                source,
            )
            .await
            .unwrap();
        assert_eq!(empty.anchor().relation(), Some(RelationId(99)));
        assert_eq!(empty.stats().incidences, 0);
        let mut opts = options();
        opts.selection.relation = Some(RelationId(99));
        opts.projection_limits.max_input_edges = 0;
        let graph = db
            .prism_sealed_projection_with_source_limits(
                &query,
                None,
                opts.selection,
                opts.projection,
                opts.projection_limits,
                opts.source_limits,
                empty_limits,
                source,
            )
            .await
            .unwrap();
        assert_eq!(graph.input_edge_count(), 0);
        assert_eq!(graph.scan_incidence_bound(), 0);
        assert_eq!(graph.node_count(), 13); // isolated selected vertices survive
    });
}

#[test]
fn wrong_relation_is_refused_before_empty_directory_or_hidden_mask_shortcuts() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let commit = contexts.commit();
        let query = contexts.query();
        let mut db = Database::<MemVfs>::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, small_fixture()).await.unwrap();
        let view = db.read_session().unwrap();
        for retained in [RelationId(1), RelationId(99)] {
            let image = db.store.seal_relation(
                &query, view.partition_root(), view.frontier(), retained, SealedLimits::default(),
            ).await.unwrap();
            let mut opts = options();
            opts.selection.relation = Some(RelationId(2));
            for vertices in [&[][..], &[VId(1), VId(2)][..]] {
                for visible in [false, true] {
                    let result = SealedGraphView::build_with_checkpoint(
                        &query, &image, vertices,
                        SealedProjectionSpec { as_of: view.frontier(), selection: opts.selection, projection: opts.projection },
                        SealedProjectionMask { relation_visible: visible, weight_property_visible: visible },
                        opts.projection_limits, || Ok(()),
                    );
                    assert!(matches!(result, Err(SealedProjectionError::Read(
                        SealedError::RelationOutsideAnchor { requested: RelationId(2), retained: actual },
                    )) if actual == retained));
                }
            }
            opts.selection.vertex_label = Some(LabelId(999));
            assert!(matches!(view.execute_fnx_sealed(&query, &image, &dijkstra_call(), opts, memory()),
                Err(SealedReadError::Projection(SealedProjectionError::Read(
                    SealedError::RelationOutsideAnchor { .. },
                )))
            ));
        }
    });
}

#[test]
fn unselected_blocks_properties_and_vertex_patches_still_require_integrity() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let commit = contexts.commit();
        let query = contexts.query();
        for damaged_kind in 0..3 {
            let mut db = Database::<MemVfs>::open_memory(&commit, keys())
                .await
                .unwrap();
            db.write(&commit, fixture()).await.unwrap();
            db.write(&commit, unrelated(8)).await.unwrap();
            let view = db.read_session().unwrap();
            let (root, blocks, _, _) = db
                .store
                .reopen(&query, view.partition_root())
                .await
                .unwrap();
            let unselected = blocks
                .iter()
                .position(|block| block[0].relation == RelationId(2))
                .unwrap();
            let block_id = root.blocks[unselected].block_id;
            let target = match damaged_kind {
                0 => block_id,
                1 => {
                    let bytes = db
                        .store
                        .get_bytes(&query, DeltaBlockVersion(block_id))
                        .await
                        .unwrap();
                    fgdb_strata::decode_block_with_properties(&bytes)
                        .unwrap()
                        .1
                        .unwrap()
                        .0
                }
                _ => root.vertex_patches[0].patch_id,
            };
            // These small commits are below the pack threshold. Damage only
            // this private MemVfs payload, leaving the authentic root untouched.
            query
                .with_restriction_async(async {
                    let mut file = db
                        .vfs
                        .open(&db.store.path(target), &OpenOptions::new().write(true))
                        .await
                        .unwrap();
                    file.write_all(&[0xff]).await.unwrap();
                })
                .await;
            assert!(
                db.store
                    .reopen(&query, view.partition_root())
                    .await
                    .is_err()
            );
            for relation in [RelationId(1), RelationId(99)] {
                assert!(matches!(
                    db.store
                        .seal_relation(
                            &query,
                            view.partition_root(),
                            view.frontier(),
                            relation,
                            SealedLimits::default(),
                        )
                        .await,
                    Err(SealedError::Store(_))
                ));
            }
            let opts = options();
            assert!(matches!(
                db.prism_sealed_projection_at(
                    &query,
                    None,
                    opts.selection,
                    opts.projection,
                    opts.projection_limits,
                    opts.source_limits,
                    SealedLimits::default(),
                )
                .await,
                Err(SealedReadError::Seal(SealedError::Store(_)))
            ));
        }
    });
}

#[test]
fn cross_relation_eid_reuse_cannot_hide_behind_source_selection() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let commit = contexts.commit();
        let query = contexts.query();
        let db = Database::<MemVfs>::open_memory(&commit, keys())
            .await
            .unwrap();
        let mut root = PartitionRoot {
            graph: crate::GRAPH,
            branch: crate::BRANCH,
            partition: crate::PARTITION,
            published_at: CommitSeq(1),
            blocks: Vec::new(),
            vertex_patches: Vec::new(),
        };
        for relation in [RelationId(1), RelationId(2)] {
            let bytes = fgdb_strata::encode_block(
                crate::PARTITION,
                None,
                &[AdjacencyEntry {
                    src: VId(1),
                    relation,
                    dst: VId(2),
                    eid: EId(7),
                    created_at: CommitSeq(1),
                    retired_at: None,
                }],
            )
            .unwrap();
            let block = db.store.put(&commit, &bytes).await.unwrap();
            root.blocks.push(BlockRef {
                block_id: block.0,
                first_seq: CommitSeq(1),
                last_seq: CommitSeq(1),
            });
        }
        // Each family is independently lawful. Together they reuse one EId
        // for two immutable births, which normal publication must reject.
        let mut lawful = root.clone();
        lawful.blocks.truncate(1);
        let admitted = db.store.put_root(&commit, &lawful).await.unwrap();
        let control = db
            .store
            .seal_relation(
                &query,
                admitted,
                CommitSeq(1),
                RelationId(1),
                SealedLimits::default(),
            )
            .await
            .unwrap();
        assert_eq!(control.stats().incidences, 1);
        assert!(matches!(
            db.store.put_root(&commit, &root).await,
            Err(StoreError::MalformedRoot(
                RootError::EdgeIdentityMismatch { .. }
            ))
        ));
        // Inject the independently encoded, correctly addressed malformed root
        // only into the private test VFS. No public constructor fabricates an
        // image anchor, and the database manifest never publishes this root.
        let bytes = fgdb_strata::root::encode_root(&root).unwrap();
        let id = PartitionRootVersion(fgdb_strata::root::root_id(
            keys().k_oid(),
            keys().namespace,
            &bytes,
        ));
        commit
            .with_restriction_async(db.vfs.write(&db.store.path(id.0), &bytes))
            .await
            .unwrap();
        for relation in [RelationId(1), RelationId(2), RelationId(99)] {
            assert!(matches!(db.store.seal_relation(
                &query, id, CommitSeq(1), relation, SealedLimits::default(),
            ).await, Err(SealedError::Store(error)) if matches!(*error,
                StoreError::MalformedRoot(RootError::EdgeIdentityMismatch { .. }))));
        }
    });
}

#[test]
fn scoped_source_cancellation_never_returns_an_image_or_receipt_prefix() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let commit = contexts.commit();
        let query = contexts.query();
        let mut db = Database::<MemVfs>::open_memory(&commit, keys())
            .await
            .unwrap();
        db.write(&commit, small_fixture()).await.unwrap();
        let mut extra = WriteBatch::new(RelationId(2));
        extra.add_edge(EId(500), VId(1), VId(2), vec![]);
        db.write(&commit, extra).await.unwrap();
        let view = db.read_session().unwrap();
        let probe = Arc::new(SimulationCheckpointProbe::new(None));
        let controlled = query.with_checkpoint_probe(Arc::clone(&probe));
        let expected = db
            .store
            .seal_relation(
                &controlled,
                view.partition_root(),
                view.frontier(),
                RelationId(1),
                SealedLimits::default(),
            )
            .await
            .unwrap();
        let checkpoints = probe.calls();
        assert!(checkpoints > 10);
        for stop in 1..=checkpoints {
            let probe = Arc::new(SimulationCheckpointProbe::new(Some(stop)));
            let controlled = query.with_checkpoint_probe(Arc::clone(&probe));
            assert!(matches!(
                db.store
                    .seal_relation(
                        &controlled,
                        view.partition_root(),
                        view.frontier(),
                        RelationId(1),
                        SealedLimits::default(),
                    )
                    .await,
                Err(SealedError::Interrupted(_))
            ));
            assert_eq!(probe.calls(), stop);
        }
        let actual = db
            .store
            .seal_relation(
                &query,
                view.partition_root(),
                view.frontier(),
                RelationId(1),
                SealedLimits::default(),
            )
            .await
            .unwrap();
        assert_eq!(actual.anchor(), expected.anchor());
        assert_eq!(contexts.outstanding_obligations(), 0);
    });
}

/// Same-invocation incumbent and candidate over real committed roots. Timing
/// includes source admission, compaction, encoding, vertex selection, incoming
/// indexing and a consumed Dijkstra result. This synthetic selective workload
/// uses warm MemVfs storage; it makes no cold-disk or external-memory claim.
#[test]
#[ignore = "manual paired source-preparation measurement; use --ignored --nocapture"]
fn paired_relation_source_preparation_benchmark() {
    use std::time::Instant;

    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let commit = contexts.commit();
        let query = contexts.query();
        let call = dijkstra_call();
        for unrelated_edges in [0, 8_192] {
            let mut db = Database::<MemVfs>::open_memory(&commit, keys())
                .await
                .unwrap();
            db.write(&commit, fixture()).await.unwrap();
            if unrelated_edges != 0 {
                db.write(&commit, unrelated(unrelated_edges)).await.unwrap();
            }
            let view = db.read_session().unwrap();
            let full = db
                .store
                .seal_partition(
                    &query,
                    view.partition_root(),
                    view.frontier(),
                    SealedLimits::default(),
                )
                .await
                .unwrap();
            let scoped = db
                .store
                .seal_relation(
                    &query,
                    view.partition_root(),
                    view.frontier(),
                    RelationId(1),
                    SealedLimits::default(),
                )
                .await
                .unwrap();
            let full_stats = full.stats();
            let scoped_stats = scoped.stats();
            assert_eq!(full_stats.incidences, scoped_stats.incidences + unrelated_edges);
            drop(full);
            drop(scoped);
            for direction in [
                Directedness::Directed,
                Directedness::Reversed,
                Directedness::Undirected,
            ] {
                let mut opts = options();
                opts.projection.directedness = direction;
                opts.projection_limits.max_input_edges = full_stats.incidences;
                let expected = view.execute_fnx(&query, &call, opts).unwrap().analytics;
                let mut full_times = [0u128; 5];
                let mut scoped_times = [0u128; 5];
                let mut incoming_workspace = [0usize; 2];
                // Alternate which path runs first so an always-second cache
                // advantage cannot silently choose the winner.
                for trial in 0..full_times.len() {
                    for candidate in if trial % 2 == 0 { [false, true] } else { [true, false] } {
                        let start = Instant::now();
                        let graph = if candidate {
                            db.prism_sealed_projection_at(
                                &query,
                                None,
                                opts.selection,
                                opts.projection,
                                opts.projection_limits,
                                opts.source_limits,
                                SealedLimits::default(),
                            )
                            .await
                            .unwrap()
                        } else {
                            // The incumbent uses the existing full-root sealer
                            // and the same host vertex/projector/kernel path.
                            let view = db.read_session().unwrap();
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
                            view.prism_sealed_projection_at(
                                &query,
                                &image,
                                view.frontier(),
                                opts.selection,
                                opts.projection,
                                opts.projection_limits,
                                opts.source_limits,
                            )
                            .unwrap()
                        };
                        let result = call
                            .execute_sealed(&query, &graph, opts.execution_limits, memory())
                            .unwrap();
                        std::hint::black_box(&result.rows);
                        let nanos = start.elapsed().as_nanos();
                        assert_eq!(result.rows, expected.rows);
                        assert_eq!(result.certificate.result_digest, expected.certificate.result_digest);
                        incoming_workspace[usize::from(candidate)] = graph
                            .incoming_index_stats()
                            .map_or(0, |stats| stats.charged_workspace_bytes);
                        if candidate {
                            scoped_times[trial] = nanos;
                            assert_eq!(graph.scan_incidence_bound(), scoped_stats.incidences);
                        } else {
                            full_times[trial] = nanos;
                            assert_eq!(graph.scan_incidence_bound(), full_stats.incidences);
                        }
                    }
                }
                full_times.sort_unstable();
                scoped_times.sort_unstable();
                eprintln!(
                    "relation-preparation-pair storage=warm-memvfs debug_assertions={} unrelated_edges={} direction={direction:?} trials=5 full_median_us={} scoped_median_us={} full_incidences={} scoped_incidences={} full_image_bytes={} scoped_image_bytes={} full_incoming_workspace={} scoped_incoming_workspace={}",
                    cfg!(debug_assertions), unrelated_edges, full_times[2] / 1_000,
                    scoped_times[2] / 1_000, full_stats.incidences, scoped_stats.incidences,
                    full_stats.encoded_bytes, scoped_stats.encoded_bytes,
                    incoming_workspace[0], incoming_workspace[1],
                );
            }
        }
    });
}

fn decoded_projection(
    view: &EmbeddedReadView,
    cx: &QueryCx,
    cut: CommitSeq,
    options: FnxReadOptions,
) -> Result<SnapshotGraphView, FnxReadError<crate::ReadError, Box<asupersync::error::Error>>> {
    view.build_prism_projection_at(
        cx,
        cut,
        options.selection,
        options.projection,
        options.projection_limits,
        options.source_limits,
    )
}

#[test]
fn indexed_decoded_projection_preserves_historical_properties_and_complete_certificates() {
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
        let first = db.read_session().unwrap().frontier();
        db.write(&commit, unrelated(64)).await.unwrap();
        let mut changes = WriteBatch::new(RelationId(1));
        changes.set_edge_property(EId(100), PropertyKeyId(1), Some(CanonicalScalar::Int(3)));
        changes.delete_edge(EId(300));
        db.write(&commit, changes).await.unwrap();
        let view = db.read_session().unwrap();
        assert!(view.snapshot.adjacency.get().is_none());
        let call = dijkstra_call();
        let mut expected = Vec::new();
        for cut in [first, view.frontier()] {
            for direction in [
                Directedness::Directed,
                Directedness::Reversed,
                Directedness::Undirected,
            ] {
                let mut opts = options();
                opts.projection.directedness = direction;
                let graph = decoded_projection(&view, &query, cut, opts).unwrap();
                let result = call
                    .execute(&graph, opts.execution_limits, || query.checkpoint())
                    .unwrap();
                expected.push((cut, opts, graph, result));
            }
        }
        assert!(
            view.snapshot.adjacency.get().is_none(),
            "a cold projection must not build adjacency"
        );

        // A real OLTP traversal creates the index once. Analytics can reuse
        // it, without making that larger allocation part of a cold CALL.
        view.neighbours(VId(1), RelationId(1)).unwrap();
        assert!(view.snapshot.adjacency.get().is_some());
        for (cut, opts, before, expected) in &expected {
            let after = decoded_projection(&view, &query, *cut, *opts).unwrap();
            assert_eq!(after.binding(), before.binding());
            assert_eq!(after.digest(), before.digest());
            assert_eq!(after.vertex_ids(), before.vertex_ids());
            assert_eq!(after.input_edge_count(), before.input_edge_count());
            assert_eq!(
                after.charged_workspace_bytes(),
                before.charged_workspace_bytes()
            );
            let actual = call
                .execute(&after, opts.execution_limits, || query.checkpoint())
                .unwrap();
            assert_eq!(
                actual, *expected,
                "cut {cut:?}, direction {:?}",
                opts.projection.directedness
            );
        }

        // Extending that index publishes a new overlay without changing the
        // old view. Re-reading its prior cut through the successor keeps the
        // same results even though the projection binds a newer root.
        let mut changes = WriteBatch::new(RelationId(1));
        changes.set_edge_property(EId(100), PropertyKeyId(1), Some(CanonicalScalar::Int(11)));
        changes.delete_edge(EId(101));
        db.write(&commit, changes).await.unwrap();
        let successor = db.read_session().unwrap();
        assert!(successor.snapshot.adjacency.get().is_some());
        for (cut, opts, _, expected) in &expected {
            let graph = decoded_projection(&successor, &query, *cut, *opts).unwrap();
            let actual = call
                .execute(&graph, opts.execution_limits, || query.checkpoint())
                .unwrap();
            assert_eq!(actual.rows, expected.rows);
            assert_eq!(
                actual.certificate.result_digest,
                expected.certificate.result_digest
            );
        }
        assert_eq!(contexts.outstanding_obligations(), 0);
    });
}

#[test]
fn indexed_decoded_projection_limits_charge_cursor_scratch_and_all_selected_edges() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let commit = contexts.commit();
        let query = contexts.query();
        let mut db = Database::<MemVfs>::open_memory(&commit, keys())
            .await
            .unwrap();
        let mut batch = WriteBatch::new(RelationId(1));
        for vertex in 1..=129 {
            batch.create_vertex(VId(vertex), vec![LabelId(1)], vec![]);
            if vertex > 1 {
                batch.add_edge(EId(vertex), VId(vertex - 1), VId(vertex), vec![]);
            }
        }
        db.write(&commit, batch).await.unwrap();
        let view = db.read_session().unwrap();
        let mut opts = FnxReadOptions::whole_graph(Some(view.frontier()));
        let expected = decoded_projection(&view, &query, view.frontier(), opts).unwrap();
        opts.source_limits.max_scratch_entries = 32;
        assert!(matches!(
            decoded_projection(&view, &query, view.frontier(), opts),
            Err(FnxReadError::SourceLimit {
                resource: "scratch entries",
                limit: 32,
                requested: 33
            })
        ));
        assert!(view.snapshot.adjacency.get().is_none());

        view.neighbours(VId(1), RelationId(1)).unwrap();
        let actual = decoded_projection(&view, &query, view.frontier(), opts).unwrap();
        assert_eq!(actual.digest(), expected.digest());
        assert_eq!(actual.input_edge_count(), 128);

        // The flat identity directory needs no scratch. Only the single
        // vertex patch cursor is charged, so exactly one entry suffices.
        let mut tight = opts;
        tight.source_limits.max_scratch_entries = 1;
        assert_eq!(
            decoded_projection(&view, &query, view.frontier(), tight)
                .unwrap()
                .digest(),
            expected.digest()
        );
        tight.source_limits.max_scratch_entries = 0;
        assert!(matches!(
            decoded_projection(&view, &query, view.frontier(), tight),
            Err(FnxReadError::SourceLimit {
                resource: "scratch entries",
                limit: 0,
                requested: 1
            })
        ));
        let mut tight = opts;
        tight.projection_limits.max_input_edges = 127;
        assert!(matches!(
            decoded_projection(&view, &query, view.frontier(), tight),
            Err(FnxReadError::Projection(ProjectionError::LimitExceeded {
                resource: "input edges",
                limit: 127,
                observed: 128
            }))
        ));
        assert_eq!(
            decoded_projection(&view, &query, view.frontier(), opts)
                .unwrap()
                .digest(),
            expected.digest()
        );
    });
}

#[test]
fn indexed_decoded_projection_cancels_during_source_and_assembly_without_a_partial_graph() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let commit = contexts.commit();
        let query = contexts.query();
        let mut db = Database::<MemVfs>::open_memory(&commit, keys())
            .await
            .unwrap();
        db.write(&commit, small_fixture()).await.unwrap();
        let view = db.read_session().unwrap();
        view.neighbours(VId(1), RelationId(1)).unwrap();
        let probe = Arc::new(SimulationCheckpointProbe::new(None));
        let controlled = query.with_checkpoint_probe(Arc::clone(&probe));
        let expected = decoded_projection(&view, &controlled, view.frontier(), options()).unwrap();
        let checkpoints = probe.calls();
        assert!(checkpoints > 20);
        for stop in 1..=checkpoints {
            let probe = Arc::new(SimulationCheckpointProbe::new(Some(stop)));
            let controlled = query.with_checkpoint_probe(Arc::clone(&probe));
            assert!(
                matches!(
                    decoded_projection(&view, &controlled, view.frontier(), options()),
                    Err(FnxReadError::Cancelled(_))
                ),
                "checkpoint {stop}"
            );
            assert_eq!(probe.calls(), stop);
        }
        assert_eq!(
            decoded_projection(&view, &query, view.frontier(), options())
                .unwrap()
                .digest(),
            expected.digest()
        );
        assert_eq!(contexts.outstanding_obligations(), 0);
    });
}

/// The two paths read independently admitted copies of the same committed
/// generation. The candidate's OLTP index build is reported separately; it
/// precedes analytics just as a real prior traversal does. Each sample builds
/// a fresh decoded projection and consumes every certified result cell, so a
/// retained projection cannot turn later samples into cache-hit measurements.
#[test]
#[ignore = "paired complete projection and certified CALL measurement; use optimized --ignored --nocapture"]
fn indexed_decoded_projection_paired_complete_call_benchmark() {
    use std::time::Instant;

    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let commit = contexts.commit();
        let query = contexts.query();
        let call = dijkstra_call();
        for versions in [1, 8] {
            let n = 512usize;
            let fanout = 4;
            let identity = |source: usize, step: usize| {
                EId(((source * fanout + step) as u128)
                    .wrapping_mul(0x9e37_79b9_7f4a_7c15_f39c_c060_5ced_c835))
            };
            let mut db = Database::<MemVfs>::open_memory(&commit, keys()).await.unwrap();
            let mut batch = WriteBatch::new(RelationId(1));
            for source in 0..n {
                batch.create_vertex(VId(1 + source as u128), vec![LabelId(1)], vec![]);
            }
            for source in 0..n {
                for step in 1..=fanout {
                    batch.add_edge(identity(source, step), VId(1 + source as u128),
                        VId(1 + ((source + step) % n) as u128),
                        vec![(PropertyKeyId(1), CanonicalScalar::Int(1))]);
                }
            }
            db.write(&commit, batch).await.unwrap();
            for version in 1..versions {
                let mut batch = WriteBatch::new(RelationId(1));
                for source in 0..n {
                    for step in 1..=fanout {
                        batch.set_edge_property(identity(source, step), PropertyKeyId(1),
                            Some(CanonicalScalar::Int(version + 1)));
                    }
                }
                db.write(&commit, batch).await.unwrap();
            }
            let raw = Database::<MemVfs>::open_read_view_with_vfs(
                &commit, db.vfs.clone(), db.path(), keys()).await.unwrap();
            let indexed = Database::<MemVfs>::open_read_view_with_vfs(
                &commit, db.vfs.clone(), db.path(), keys()).await.unwrap();
            assert_eq!(raw.partition_root(), indexed.partition_root());
            assert!(raw.snapshot.adjacency.get().is_none());
            assert!(indexed.snapshot.adjacency.get().is_none());
            let start = Instant::now();
            indexed.neighbours(VId(1), RelationId(1)).unwrap();
            let index_build_ns = start.elapsed().as_nanos();
            let opts = FnxReadOptions::whole_graph(Some(raw.frontier()));
            let mut samples = [Vec::new(), Vec::new()];
            for trial in 0..8 {
                let mut outputs = Vec::new();
                for use_index in if trial % 2 == 0 { [false, true] } else { [true, false] } {
                    let view = if use_index { &indexed } else { &raw };
                    let start = Instant::now();
                    let graph = decoded_projection(view, &query, view.frontier(), opts).unwrap();
                    let result = call.execute(&graph, opts.execution_limits, || query.checkpoint()).unwrap();
                    let checksum = result.rows.iter().flatten().fold(0u128, |sum, value| {
                        sum.wrapping_add(match value {
                            FnxValue::Vertex(vertex) => vertex.0,
                            FnxValue::Score(value) | FnxValue::Float(value) => u128::from(value.to_bits()),
                            FnxValue::Integer(value) => u128::from(*value),
                        })
                    });
                    std::hint::black_box(checksum);
                    drop(graph);
                    samples[usize::from(use_index)].push(start.elapsed().as_nanos());
                    outputs.push(result);
                }
                assert_eq!(outputs[0], outputs[1]);
                assert!(raw.snapshot.adjacency.get().is_none());
            }
            println!("indexed_projection_pair vertices={n} edges={} versions={versions} index_build_ns={index_build_ns} raw_ns={:?} indexed_ns={:?}",
                n * fanout, samples[0], samples[1]);
        }
        assert_eq!(contexts.outstanding_obligations(), 0);
    });
}
