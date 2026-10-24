//! Real Chronicle roots, compressed images and kernels exercise reuse through
//! public owner APIs. Answers, refusal boundaries and physical sharing all
//! matter: equal redacted diagnostics would not establish any of these laws.
#![recursion_limit = "256"]

#[path = "sealed_read_cache/construction.rs"]
mod construction;

use asupersync::{Budget, runtime::RuntimeBuilder};
use fgdb::{
    BufferLimits, BufferedReadLimits, BufferedReadView, Database, DatabaseKeys, MemVfs, MemoryPool,
    ReadCacheLimits, WriteBatch,
};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_prism::{
    Directedness, FnxCallSpec, FnxMemoryLimits, FnxParameters, FnxReadOptions, FnxSealedReadError,
    FnxValue, FnxWeightSpec, MissingWeightPolicy, ParallelEdgePolicy, SealedGraphView,
    SelfLoopPolicy,
};
use fgdb_strata::store::{BlockStore, RootReadLimits};
use fgdb_strata::tiered::sealed::SealedLimits;
use fgdb_types::context::SimulationCheckpointProbe;
use fgdb_types::{
    CanonicalList, CanonicalPropertyValue, CanonicalScalar, CommitCx, CommitSeq,
    DatabaseSecurityNamespaceId, EId, PurposeContexts, QueryCx, VId,
};
use std::sync::Arc;

const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x73; 32]);
const R: RelationId = RelationId(7);
const L: LabelId = LabelId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const RAW: usize = 1000;
const TRIANGLES: &str = "CALL fnx.triangles() YIELD vertex,triangles";

fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0x72; 32], NS, [0x74; 32])
}

fn options() -> FnxReadOptions {
    let mut options = FnxReadOptions::whole_graph(None);
    options.selection.vertex_label = Some(L);
    options.selection.relation = Some(R);
    options.projection.directedness = Directedness::Undirected;
    options.projection.parallel_edges = ParallelEdgePolicy::CollapseUnit;
    options.projection.self_loops = SelfLoopPolicy::Drop;
    options.source_limits.max_work_units = 100_000;
    options.source_limits.max_scratch_entries = 10_000;
    options.source_limits.max_staging_bytes = 1 << 20;
    options.projection_limits.max_vertices = 100;
    options.projection_limits.max_input_edges = 1000;
    options.projection_limits.max_adjacency_entries = 2000;
    options.projection_limits.max_workspace_bytes = 1 << 20;
    options.execution_limits.max_result_rows = 100;
    options.execution_limits.max_estimated_work = 1 << 25;
    options
}

fn memory() -> FnxMemoryLimits {
    FnxMemoryLimits {
        max_kernel_workspace_bytes: 1 << 20,
        max_result_bytes: 1 << 20,
    }
}

fn call(text: &str) -> FnxCallSpec {
    FnxCallSpec::bind(text, &FnxParameters::new()).unwrap()
}

fn expected() -> Vec<Vec<FnxValue>> {
    [1, 2, 3, u128::MAX]
        .into_iter()
        .map(|id| {
            vec![
                FnxValue::Vertex(VId(id)),
                FnxValue::Integer(u64::from(id != u128::MAX)),
            ]
        })
        .collect()
}

async fn seed(cx: &CommitCx, vfs: &MemVfs) -> Database<MemVfs> {
    let mut database = Database::create_with_vfs(cx, vfs.clone(), vfs.database_dir(), keys())
        .await
        .unwrap();
    let mut batch = WriteBatch::new(R);
    for id in [1, 2, 3, u128::MAX] {
        batch.create_vertex(VId(id), vec![L], vec![]);
    }
    batch.create_vertex(VId(9), vec![LabelId(2)], vec![]);
    // More than eight raw incidences force a compressed CSR row. Properties
    // include nested allocated values even though these unit kernels ignore it.
    for (eid, source, target) in [(1, 1, 2), (2, 2, 3), (3, 3, 1), (4, 1, 1)]
        .into_iter()
        .chain((10..22).map(|eid| (eid, 1, 2)))
    {
        let nested = CanonicalPropertyValue::List(
            CanonicalList::try_new(vec![
                CanonicalScalar::bytes(vec![eid as u8; 32]).unwrap().into(),
                CanonicalPropertyValue::List(
                    CanonicalList::try_new(vec![CanonicalScalar::Int(7).into()]).unwrap(),
                ),
            ])
            .unwrap(),
        );
        batch.add_edge_values(
            EId(eid),
            VId(source),
            VId(target),
            vec![
                (P, CanonicalScalar::Int(1).into()),
                (PropertyKeyId(2), nested),
            ],
        );
    }
    batch.add_edge(
        EId(99),
        VId(9),
        VId(1),
        vec![(P, CanonicalScalar::Bool(false))],
    );
    database.write(cx, batch).await.unwrap();
    let mut update = WriteBatch::new(R);
    update.set_edge_property(EId(1), P, Some(CanonicalScalar::Int(5)));
    database.write(cx, update).await.unwrap();
    let mut other = WriteBatch::new(RelationId(8));
    other.add_edge(
        EId(100),
        VId(1),
        VId(u128::MAX),
        vec![(P, CanonicalScalar::Int(2))],
    );
    database.write(cx, other).await.unwrap();
    database
}

async fn buffered(contexts: &PurposeContexts, vfs: &MemVfs) -> BufferedReadView<MemVfs> {
    Database::open_buffered_read_view_with_vfs(
        &contexts.commit(),
        vfs.clone(),
        vfs.database_dir(),
        keys(),
        MemoryPool::new(16 << 20, 0).unwrap(),
        BufferedReadLimits {
            max_root_bytes: 64 << 10,
            max_source_bytes: 4 << 20,
            max_blocks: 512,
            max_vertex_patches: 512,
            max_work: 1_000_000,
            buffer: BufferLimits {
                max_frames: 1,
                max_ghost_entries: 2,
                max_extent_bytes: 16 << 10,
            },
        },
    )
    .await
    .unwrap()
}

async fn project(
    database: &Database<MemVfs>,
    cx: &QueryCx,
    options: FnxReadOptions,
) -> SealedGraphView {
    database
        .prism_sealed_projection_at(
            cx,
            options.as_of,
            options.selection,
            options.projection,
            options.projection_limits,
            options.source_limits,
            SealedLimits::default(),
        )
        .await
        .unwrap()
}

async fn buffered_project(
    view: &mut BufferedReadView<MemVfs>,
    cx: &QueryCx,
    options: FnxReadOptions,
) -> SealedGraphView {
    view.prism_sealed_projection_at(
        cx,
        options.as_of.unwrap_or(view.frontier()),
        options.selection,
        options.projection,
        options.projection_limits,
        options.source_limits,
        SealedLimits::default(),
        RAW,
    )
    .await
    .unwrap()
}

fn cache_limit(database: &Database<MemVfs>, cx: &QueryCx, bytes: usize) {
    database
        .set_read_cache_limits(
            cx,
            ReadCacheLimits {
                sealed_projection_bytes: bytes,
                ..ReadCacheLimits::default()
            },
        )
        .unwrap();
}

#[test]
fn database_reuses_compressed_storage_across_calls_and_enforces_exact_retention_bytes() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        let vfs = MemVfs::new().unwrap();
        let database = seed(&contexts.commit(), &vfs).await;
        let first = project(&database, &cx, options()).await;
        let bytes = first.charged_resident_bytes();
        assert!(
            bytes > first.charged_workspace_bytes(),
            "source image is additional retained ownership"
        );
        assert!(first.shares_cache_with(&project(&database, &cx, options()).await));
        let status = database.read_cache_status(&cx).unwrap();
        assert_eq!(status.sealed_projections.entries, 1);
        assert_eq!(status.sealed_projections.retained_bytes, bytes);
        let triangle = database
            .execute_fnx_sealed(
                &cx,
                &call(TRIANGLES),
                options(),
                memory(),
                SealedLimits::default(),
            )
            .await
            .unwrap();
        assert_eq!(triangle.analytics.rows, expected());
        let rank = call("CALL fnx.pagerank(0.85,50,1e-9,true) YIELD vertex,score");
        let warm = database
            .execute_fnx_sealed(&cx, &rank, options(), memory(), SealedLimits::default())
            .await
            .unwrap();
        assert_eq!(
            database
                .read_cache_status(&cx)
                .unwrap()
                .sealed_projections
                .entries,
            1
        );
        assert!(first.shares_cache_with(&project(&database, &cx, options()).await));

        cache_limit(&database, &cx, bytes - 1);
        let below = project(&database, &cx, options()).await;
        assert!(!below.shares_cache_with(&project(&database, &cx, options()).await));
        assert_eq!(
            database
                .read_cache_status(&cx)
                .unwrap()
                .sealed_projections
                .entries,
            0
        );
        assert_eq!(
            database
                .execute_fnx_sealed(&cx, &rank, options(), memory(), SealedLimits::default())
                .await
                .unwrap(),
            warm
        );
        cache_limit(&database, &cx, bytes);
        let exact = project(&database, &cx, options()).await;
        assert_eq!(exact.charged_resident_bytes(), bytes);
        assert!(exact.shares_cache_with(&project(&database, &cx, options()).await));
        let mut variant = options();
        variant.source_limits.max_work_units -= 1;
        assert!(!exact.shares_cache_with(&project(&database, &cx, variant).await));
        assert!(exact.shares_cache_with(&project(&database, &cx, options()).await));
        cache_limit(&database, &cx, 0);
        assert_eq!(
            database
                .read_cache_status(&cx)
                .unwrap()
                .sealed_projections
                .retained_bytes,
            0
        );
        assert_eq!(
            call(TRIANGLES)
                .execute_sealed(&cx, &first, options().execution_limits, memory())
                .unwrap()
                .rows,
            expected()
        );
    });
}

#[test]
fn buffered_hits_skip_source_faults_and_charge_a_separate_bounded_retention_pool() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        let vfs = MemVfs::new().unwrap();
        let _writer = seed(&contexts.commit(), &vfs).await;
        let mut view = buffered(&contexts, &vfs).await;
        let source_pool = view.memory_pool().clone();
        let initial = buffered_project(&mut view, &cx, options()).await;
        let bytes = initial.charged_resident_bytes();
        let faults = view.buffer_stats();
        assert!(initial.shares_cache_with(&buffered_project(&mut view, &cx, options()).await));
        assert_eq!(view.buffer_stats(), faults);
        assert_eq!(view.sealed_read_cache_usage().retained_bytes, bytes);
        assert_eq!(
            view.execute_fnx_sealed(
                &cx,
                &call(TRIANGLES),
                options(),
                memory(),
                SealedLimits::default(),
                RAW
            )
            .await
            .unwrap()
            .analytics
            .rows,
            expected()
        );
        assert_eq!(view.buffer_stats(), faults);

        // Two child caches obey a common ancestor. Source frames do not spend
        // this budget, and a declined retention never fails the actual query.
        let parent = MemoryPool::new(bytes, 0).unwrap();
        let retained = parent.child(bytes * 2, 0).unwrap();
        view.set_sealed_read_cache_pool(&cx, retained.clone())
            .unwrap();
        let first = buffered_project(&mut view, &cx, options()).await;
        assert_eq!(retained.used(), bytes);
        assert_eq!(parent.used(), bytes);
        let mut second = buffered(&contexts, &vfs).await;
        second
            .set_sealed_read_cache_pool(&cx, parent.child(bytes * 2, 0).unwrap())
            .unwrap();
        let separate = buffered_project(&mut second, &cx, options()).await;
        assert_eq!(first.digest(), separate.digest());
        assert!(!first.shares_cache_with(&separate));
        assert_eq!(second.sealed_read_cache_usage().entries, 0);
        assert_eq!(parent.used(), bytes);
        view.set_sealed_read_cache_pool(&cx, MemoryPool::new(bytes - 1, 0).unwrap())
            .unwrap();
        assert_eq!(parent.used(), 0);
        let below = buffered_project(&mut view, &cx, options()).await;
        assert!(!below.shares_cache_with(&buffered_project(&mut view, &cx, options()).await));
        assert_eq!(view.sealed_read_cache_usage().retained_bytes, 0);
        view.set_sealed_read_cache_pool(&cx, MemoryPool::new(0, 0).unwrap())
            .unwrap();
        assert_eq!(
            buffered_project(&mut view, &cx, options()).await.digest(),
            initial.digest()
        );
        let after_disabled = view.buffer_stats();
        let disabled = buffered_project(&mut view, &cx, options()).await;
        assert_ne!(
            view.buffer_stats(),
            after_disabled,
            "zero retention takes the ordinary source path"
        );
        assert!(!disabled.shares_cache_with(&initial));
        drop(view);
        assert_eq!(source_pool.used(), 0);
        assert_eq!(retained.used(), 0);
        assert_eq!(
            call(TRIANGLES)
                .execute_sealed(&cx, &initial, options().execution_limits, memory())
                .unwrap()
                .rows,
            expected()
        );
    });
}

#[test]
fn warm_construction_never_bypasses_stricter_source_image_projection_or_kernel_limits() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        let vfs = MemVfs::new().unwrap();
        let database = seed(&contexts.commit(), &vfs).await;
        let original = project(&database, &cx, options()).await;
        let mut view = buffered(&contexts, &vfs).await;
        let buffered_original = buffered_project(&mut view, &cx, options()).await;
        for dimension in 0..11 {
            let mut opt = options();
            let mut sealing = SealedLimits::default();
            match dimension {
                0 => opt.source_limits.max_work_units = 0,
                1 => opt.source_limits.max_scratch_entries = 0,
                2 => opt.source_limits.max_staging_bytes = 0,
                3 => opt.projection_limits.max_vertices = 0,
                4 => opt.projection_limits.max_input_edges = 0,
                5 => opt.projection_limits.max_adjacency_entries = 0,
                6 => opt.projection_limits.max_workspace_bytes = 0,
                7 => sealing.max_rows = 0,
                8 => sealing.max_incidences = 0,
                9 => sealing.max_image_bytes = 0,
                10 => sealing.max_property_bytes = 0,
                _ => unreachable!(),
            }
            assert!(
                database
                    .prism_sealed_projection_at(
                        &cx,
                        opt.as_of,
                        opt.selection,
                        opt.projection,
                        opt.projection_limits,
                        opt.source_limits,
                        sealing
                    )
                    .await
                    .is_err(),
                "resident dimension {dimension}"
            );
            assert!(
                view.prism_sealed_projection_at(
                    &cx,
                    view.frontier(),
                    opt.selection,
                    opt.projection,
                    opt.projection_limits,
                    opt.source_limits,
                    sealing,
                    RAW
                )
                .await
                .is_err(),
                "buffered dimension {dimension}"
            );
            assert!(original.shares_cache_with(&project(&database, &cx, options()).await));
            assert!(
                buffered_original
                    .shares_cache_with(&buffered_project(&mut view, &cx, options()).await)
            );
        }
        for dimension in 0..6 {
            let mut source = RootReadLimits::default();
            match dimension {
                0 => source.max_root_bytes = 0,
                1 => source.max_source_bytes = 0,
                2 => source.max_blocks = 0,
                3 => source.max_vertex_patches = 0,
                4 => source.max_incidences = 0,
                5 => source.max_vertex_versions = 0,
                _ => unreachable!(),
            }
            let opt = options();
            assert!(
                database
                    .prism_sealed_projection_with_source_limits(
                        &cx,
                        opt.as_of,
                        opt.selection,
                        opt.projection,
                        opt.projection_limits,
                        opt.source_limits,
                        SealedLimits::default(),
                        source
                    )
                    .await
                    .is_err(),
                "root dimension {dimension}"
            );
        }
        let opt = options();
        assert!(
            view.prism_sealed_projection_at(
                &cx,
                view.frontier(),
                opt.selection,
                opt.projection,
                opt.projection_limits,
                opt.source_limits,
                SealedLimits::default(),
                0
            )
            .await
            .is_err()
        );
        let faults = view.buffer_stats();
        assert!(
            view.prism_sealed_projection_at(
                &cx,
                CommitSeq(view.frontier().0 + 1),
                opt.selection,
                opt.projection,
                opt.projection_limits,
                opt.source_limits,
                SealedLimits::default(),
                RAW
            )
            .await
            .is_err()
        );
        assert_eq!(view.buffer_stats(), faults);
        for dimension in 0..4 {
            let mut opt = options();
            let mut memory = memory();
            match dimension {
                0 => opt.execution_limits.max_result_rows = 0,
                1 => opt.execution_limits.max_estimated_work = 0,
                2 => memory.max_kernel_workspace_bytes = 0,
                3 => memory.max_result_bytes = 0,
                _ => unreachable!(),
            }
            assert!(
                database
                    .execute_fnx_sealed(&cx, &call(TRIANGLES), opt, memory, SealedLimits::default())
                    .await
                    .is_err()
            );
            assert!(
                view.execute_fnx_sealed(
                    &cx,
                    &call(TRIANGLES),
                    opt,
                    memory,
                    SealedLimits::default(),
                    RAW
                )
                .await
                .is_err()
            );
        }
        assert_eq!(
            database
                .read_cache_status(&cx)
                .unwrap()
                .sealed_projections
                .entries,
            1
        );
        assert_eq!(view.sealed_read_cache_usage().entries, 1);
        assert_eq!(
            view.buffer_stats(),
            faults,
            "kernel/result refusal reuses the admitted source"
        );
    });
}

#[test]
fn live_cancellation_covers_hits_and_prevents_partial_cache_publication() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        let vfs = MemVfs::new().unwrap();
        let database = seed(&contexts.commit(), &vfs).await;
        let mut view = buffered(&contexts, &vfs).await;
        let opt = options();
        let cold = Arc::new(SimulationCheckpointProbe::new(None));
        let observed = cx.with_checkpoint_probe(Arc::clone(&cold));
        let first = project(&database, &observed, opt).await;
        let cold_calls = cold.calls();
        for stop in [1, cold_calls / 2, cold_calls] {
            cache_limit(&database, &cx, 0);
            cache_limit(&database, &cx, first.charged_resident_bytes());
            let probe = Arc::new(SimulationCheckpointProbe::new(Some(stop)));
            let interrupted = cx.with_checkpoint_probe(Arc::clone(&probe));
            assert!(
                database
                    .prism_sealed_projection_at(
                        &interrupted,
                        opt.as_of,
                        opt.selection,
                        opt.projection,
                        opt.projection_limits,
                        opt.source_limits,
                        SealedLimits::default()
                    )
                    .await
                    .is_err(),
                "cold checkpoint {stop}"
            );
            assert_eq!(probe.calls(), stop);
            assert_eq!(
                database
                    .read_cache_status(&cx)
                    .unwrap()
                    .sealed_projections
                    .entries,
                0
            );
        }
        let warmed = project(&database, &cx, opt).await;
        let buffered_warmed = buffered_project(&mut view, &cx, opt).await;
        for buffered in [false, true] {
            let probe = Arc::new(SimulationCheckpointProbe::new(None));
            let measured = cx.with_checkpoint_probe(Arc::clone(&probe));
            if buffered {
                buffered_project(&mut view, &measured, opt).await;
            } else {
                project(&database, &measured, opt).await;
            }
            assert!(probe.calls() < cold_calls);
            for stop in 1..=probe.calls() {
                let probe = Arc::new(SimulationCheckpointProbe::new(Some(stop)));
                let interrupted = cx.with_checkpoint_probe(Arc::clone(&probe));
                if buffered {
                    assert!(
                        view.prism_sealed_projection_at(
                            &interrupted,
                            view.frontier(),
                            opt.selection,
                            opt.projection,
                            opt.projection_limits,
                            opt.source_limits,
                            SealedLimits::default(),
                            RAW
                        )
                        .await
                        .is_err()
                    );
                } else {
                    assert!(
                        database
                            .prism_sealed_projection_at(
                                &interrupted,
                                opt.as_of,
                                opt.selection,
                                opt.projection,
                                opt.projection_limits,
                                opt.source_limits,
                                SealedLimits::default()
                            )
                            .await
                            .is_err()
                    );
                }
                assert_eq!(probe.calls(), stop);
                assert!(warmed.shares_cache_with(&project(&database, &cx, opt).await));
                assert!(
                    buffered_warmed.shares_cache_with(&buffered_project(&mut view, &cx, opt).await)
                );
            }
        }
    });
}

#[test]
fn pinned_roots_and_opaque_source_receipts_prevent_stale_or_cross_owner_reuse() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        let commit = contexts.commit();
        let vfs = MemVfs::new().unwrap();
        let mut database = seed(&commit, &vfs).await;
        let old = database.read_session().unwrap();
        let original = project(&database, &cx, options()).await;
        let mut cold = buffered(&contexts, &vfs).await;
        let old_buffered = buffered_project(&mut cold, &cx, options()).await;
        let store = BlockStore::open_existing_for_read_with_vfs(
            &commit,
            vfs.clone(),
            vfs.database_dir(),
            [0x72; 32],
            NS,
        )
        .await
        .unwrap();
        let partition = store
            .seal_relation(
                &cx,
                old.partition_root(),
                CommitSeq(1),
                R,
                SealedLimits::default(),
            )
            .await
            .unwrap();
        let opt = options();
        let old_project = || {
            old.prism_sealed_projection_at(
                &cx,
                &partition,
                old.frontier(),
                opt.selection,
                opt.projection,
                opt.projection_limits,
                opt.source_limits,
            )
            .unwrap()
        };
        let from_receipt = old_project();
        assert!(from_receipt.shares_cache_with(&old_project()));
        let independent =
            Database::open_read_view_with_vfs(&commit, vfs.clone(), vfs.database_dir(), keys())
                .await
                .unwrap();
        let isolated = independent
            .prism_sealed_projection_at(
                &cx,
                &partition,
                independent.frontier(),
                opt.selection,
                opt.projection,
                opt.projection_limits,
                opt.source_limits,
            )
            .unwrap();
        assert_eq!(isolated.digest(), from_receipt.digest());
        assert!(!isolated.shares_cache_with(&from_receipt));
        let later_floor = store
            .seal_relation(
                &cx,
                old.partition_root(),
                old.frontier(),
                R,
                SealedLimits::default(),
            )
            .await
            .unwrap();
        let later = old
            .prism_sealed_projection_at(
                &cx,
                &later_floor,
                old.frontier(),
                opt.selection,
                opt.projection,
                opt.projection_limits,
                opt.source_limits,
            )
            .unwrap();
        assert!(
            !later.shares_cache_with(&from_receipt),
            "the opaque receipt, including floor, is part of the key"
        );

        let mut change = WriteBatch::new(R);
        change.delete_edge(EId(3));
        database.write(&commit, change).await.unwrap();
        database.compact(&commit).await.unwrap();
        let current = project(&database, &cx, opt).await;
        assert_ne!(current.binding(), original.binding());
        assert!(!current.shares_cache_with(&original));
        assert!(from_receipt.shares_cache_with(&old_project()));
        assert!(old_buffered.shares_cache_with(&buffered_project(&mut cold, &cx, opt).await));
        assert_eq!(
            call(TRIANGLES)
                .execute_sealed(&cx, &old_buffered, opt.execution_limits, memory())
                .unwrap()
                .rows,
            expected()
        );
        assert!(
            call(TRIANGLES)
                .execute_sealed(&cx, &current, opt.execution_limits, memory())
                .unwrap()
                .rows
                .iter()
                .all(|row| row[1] == FnxValue::Integer(0))
        );
        let new_view = database.read_session().unwrap();
        assert!(matches!(
            new_view.prism_sealed_projection_at(
                &cx,
                &partition,
                new_view.frontier(),
                opt.selection,
                opt.projection,
                opt.projection_limits,
                opt.source_limits
            ),
            Err(FnxSealedReadError::SourceMismatch)
        ));
    });
}

#[test]
fn buffered_slots_do_not_displace_live_entries_and_selection_keys_do_not_alias() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        let vfs = MemVfs::new().unwrap();
        let _writer = seed(&contexts.commit(), &vfs).await;
        let mut view = buffered(&contexts, &vfs).await;
        let first = buffered_project(&mut view, &cx, options()).await;
        for dimension in 0..7 {
            let mut variant = options();
            match dimension {
                0 => variant.selection.vertex_label = None,
                1 => variant.selection.relation = Some(RelationId(8)),
                2 => {
                    variant.selection.weight = FnxWeightSpec::Property {
                        key: P,
                        missing: MissingWeightPolicy::Reject,
                    }
                }
                3 => {
                    variant.selection.weight = FnxWeightSpec::Property {
                        key: P,
                        missing: MissingWeightPolicy::Zero,
                    }
                }
                4 => variant.projection.directedness = Directedness::Directed,
                5 => variant.projection.self_loops = SelfLoopPolicy::Keep,
                6 => variant.projection.parallel_edges = ParallelEdgePolicy::Minimum,
                _ => unreachable!(),
            }
            let changed = buffered_project(&mut view, &cx, variant).await;
            assert!(!first.shares_cache_with(&changed), "dimension {dimension}");
            assert!(changed.shares_cache_with(&buffered_project(&mut view, &cx, variant).await));
        }
        assert_eq!(view.sealed_read_cache_usage().entries, 8);
        let mut extra = options();
        extra.as_of = Some(CommitSeq(1));
        let overflow = buffered_project(&mut view, &cx, extra).await;
        assert!(!overflow.shares_cache_with(&buffered_project(&mut view, &cx, extra).await));
        assert!(first.shares_cache_with(&buffered_project(&mut view, &cx, options()).await));
        assert_eq!(view.sealed_read_cache_usage().entries, 8);
    });
}
