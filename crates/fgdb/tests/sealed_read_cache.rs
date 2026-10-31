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
use fgdb_gql::algebra::{GraphValue, GraphValueRow};
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphAggregateTextSlot, GraphSymbol, GraphSymbolKind,
    PreparedGraphSet, PreparedGraphSetText,
};
use fgdb_prism::{
    Directedness, FnxCallSpec, FnxMemoryLimits, FnxParameters, FnxReadOptions, FnxSealedReadError,
    FnxValue, FnxWeightSpec, MissingWeightPolicy, ParallelEdgePolicy, SealedGraphView,
    SelfLoopPolicy,
};
use fgdb_strata::store::{BlockStore, RootReadLimits};
use fgdb_strata::tiered::sealed::SealedLimits;
use fgdb_types::context::SimulationCheckpointProbe;
use fgdb_types::{
    CanonicalF64, CanonicalList, CanonicalPropertyValue, CanonicalScalar, CommitCx, CommitSeq,
    DatabaseSecurityNamespaceId, EId, PurposeContexts, QueryCx, VId,
};
use std::{mem::size_of, sync::Arc};

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

fn sealed_sql_symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "L") => Some(GraphSymbol::Label(L)),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Relation, "Other") => Some(GraphSymbol::Relation(RelationId(8))),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    }
}

fn sealed_sql_policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(1_000_000, 100_000, 100_000_000, 10_000_000)
}

fn sealed_sql_rows(text: &str) -> PreparedGraphSet {
    PreparedGraphSetText::prepare(text, sealed_sql_symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}

fn sealed_sql_bfs(yields: &str, tail: &str) -> String {
    format!(
        "CALL fnx.single_source_shortest_path_length(\
         args => [1], label => 'L', relation => 'R', direction => 'directed', \
         parallel => 'collapse', self_loops => 'drop') YIELD {yields} {tail}"
    )
}

fn sealed_sql_distances(pairs: &[(u128, i64)]) -> Vec<GraphValueRow> {
    pairs
        .iter()
        .map(|&(vertex, distance)| {
            GraphValueRow::from_owned_values(vec![
                GraphValue::Vertex(VId(vertex)),
                GraphValue::Scalar(CanonicalScalar::Int(distance)),
            ])
        })
        .collect()
}

fn sealed_sql_weighted(direction: &str, parallel: &str) -> String {
    format!(
        "CALL fnx.single_source_dijkstra_path_length(\
         args => [1], label => 'L', relation => 'R', weight => 'p', missing => 'reject', \
         direction => '{direction}', parallel => '{parallel}', self_loops => 'drop') \
         YIELD vertex, distance RETURN vertex, distance ORDER BY vertex"
    )
}

fn sealed_sql_costs(pairs: &[(u128, f64)]) -> Vec<GraphValueRow> {
    pairs
        .iter()
        .map(|&(vertex, distance)| {
            GraphValueRow::from_owned_values(vec![
                GraphValue::Vertex(VId(vertex)),
                GraphValue::Scalar(CanonicalScalar::Float(CanonicalF64::new(distance))),
            ])
        })
        .collect()
}

#[test]
fn readonly_sealed_native_templates_preserve_pages_group_slots_and_typed_refusals() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        let commit = contexts.commit();
        let vfs = MemVfs::new().unwrap();
        let database = seed(&commit, &vfs).await;
        let view = Database::open_sealed_read_with_vfs(
            &commit,
            &cx,
            vfs.clone(),
            vfs.database_dir(),
            keys(),
            None,
            R,
            SealedLimits::default(),
            RootReadLimits::default(),
        )
        .await
        .unwrap();
        assert_eq!(view.snapshot_seq(), CommitSeq(3));
        assert_eq!(
            view.partition_root(),
            database.read_session().unwrap().partition_root()
        );
        let params = GqlParameters::new().with_int64("minimum", 1).unwrap();
        let empty = GqlParameters::new();
        let arguments = [&params, &empty, &empty];
        let texts = [
            sealed_sql_bfs(
                "vertex AS node, distance AS hops",
                "WHERE hops >= $minimum RETURN node, hops ORDER BY hops DESC, node LIMIT 2",
            ),
            sealed_sql_weighted("directed", "sum"),
            sealed_sql_bfs(
                "vertex, distance",
                "WITH vertex, distance, distance % 2 AS parity \
                 RETURN SUM(distance) AS total, parity, COUNT(*) AS n \
                 GROUP BY parity ORDER BY parity",
            ),
        ];
        let prepared: Vec<_> = texts
            .iter()
            .zip(arguments)
            .map(|(text, params)| {
                fgdb::PreparedNativeRead::prepare(text, params, sealed_sql_symbols).unwrap()
            })
            .collect();
        for ((text, prepared), params) in texts.iter().zip(&prepared).zip(arguments) {
            let actual = view
                .execute_prepared(&cx, prepared, params, sealed_sql_policy(), memory())
                .unwrap();
            let expected = database
                .query(&cx, text, params, sealed_sql_symbols, sealed_sql_policy())
                .unwrap();
            assert_eq!(actual, expected, "{text}");
        }
        // Aggregate output order differs from both group-key order and the
        // aggregate array. The native result must preserve those exact slots.
        assert_eq!(
            view.execute_prepared(&cx, &prepared[2], &empty, sealed_sql_policy(), memory())
                .unwrap(),
            fgdb::QueryResult::Rows {
                columns: vec!["total".into(), "parity".into(), "n".into()],
                rows: vec![
                    vec![
                        fgdb::QueryValue::Integer(2),
                        fgdb::QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Int(0))),
                        fgdb::QueryValue::Count(2)
                    ],
                    vec![
                        fgdb::QueryValue::Integer(1),
                        fgdb::QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Int(1))),
                        fgdb::QueryValue::Count(1)
                    ],
                ],
            },
        );
        let rebound = GqlParameters::new().with_int64("minimum", 2).unwrap();
        assert_eq!(
            view.execute_prepared(&cx, &prepared[0], &rebound, sealed_sql_policy(), memory())
                .unwrap(),
            fgdb::QueryResult::Rows {
                columns: vec!["node".into(), "hops".into()],
                rows: vec![vec![
                    fgdb::QueryValue::Value(GraphValue::Vertex(VId(3))),
                    fgdb::QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Int(2)))
                ]],
            },
        );
        assert!(matches!(
            view.execute_prepared(
                &cx,
                &prepared[0],
                &GqlParameters::new(),
                sealed_sql_policy(),
                memory()
            ),
            Err(fgdb::SealedQueryError::Query(fgdb::QueryError::SetText(_))),
        ));
        let text = "MATCH (n:L) RETURN n.p";
        let prepared = fgdb::PreparedNativeRead::prepare(text, &empty, sealed_sql_symbols).unwrap();
        assert_eq!(
            view.execute_prepared(&cx, &prepared, &empty, sealed_sql_policy(), memory())
                .unwrap(),
            database
                .query(&cx, text, &empty, sealed_sql_symbols, sealed_sql_policy())
                .unwrap(),
        );
        for requested in [1, 4] {
            let text = format!(
                "MATCH (n:L) FOR SYSTEM_TIME AS OF SEQ {requested} \
                 RETURN n.p AS p UNION ALL MATCH (m:L) RETURN m.p AS p LIMIT 0"
            );
            let prepared =
                fgdb::PreparedNativeRead::prepare(&text, &empty, sealed_sql_symbols).unwrap();
            assert!(matches!(
                view.execute_prepared(&cx, &prepared, &empty, sealed_sql_policy(), memory()),
                Err(fgdb::SealedQueryError::SnapshotMismatch {
                    requested: actual,
                    retained: CommitSeq(3),
                }) if actual == CommitSeq(requested),
            ));
        }
        let wrong_relation = fgdb::PreparedNativeRead::prepare(
            "CALL fnx.weakly_connected_components(relation => 'Other') \
             YIELD vertex, component RETURN vertex, component LIMIT 0",
            &empty,
            sealed_sql_symbols,
        )
        .unwrap();
        assert!(matches!(
            view.execute_prepared(&cx, &wrong_relation, &empty, sealed_sql_policy(), memory()),
            Err(fgdb::SealedQueryError::Query(fgdb::QueryError::Set(
                fgdb_gql::GqlQueryError::Source(fgdb_gql::GraphSetExecutionError::Source(
                    fgdb::GqlError::Procedure(fgdb::ProcedureError::SealedRelationMismatch {
                        requested: Some(RelationId(8)),
                        retained: R,
                    }),
                )),
            ))),
        ));
        let zero = fgdb::PreparedNativeRead::prepare(
            &sealed_sql_bfs("vertex, distance", "RETURN vertex, distance LIMIT 0"),
            &empty,
            sealed_sql_symbols,
        )
        .unwrap();
        assert!(matches!(
            view.execute_prepared(
                &cx,
                &zero,
                &empty,
                sealed_sql_policy(),
                FnxMemoryLimits {
                    max_kernel_workspace_bytes: 0,
                    ..memory()
                }
            ),
            Err(fgdb::SealedQueryError::Query(fgdb::QueryError::Set(_))),
        ));
    });
}

#[test]
fn sealed_owners_execute_every_native_class_at_current_and_historical_cuts() {
    use fgdb::NativeReadClass as Class;

    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        let commit = contexts.commit();
        let vfs = MemVfs::new().unwrap();
        let mut database = seed(&commit, &vfs).await;
        let mut update = WriteBatch::new(R);
        update.create_vertex(VId(4), vec![L], vec![(P, CanonicalScalar::Int(99))]);
        assert_eq!(database.write(&commit, update).await.unwrap(), CommitSeq(4));
        let cases = [
            (Class::Pattern, "MATCH (n:L) RETURN n.p"),
            (Class::Aggregate, "MATCH (n:L) RETURN COUNT(*) AS n"),
            (
                Class::PipelineAggregate,
                "MATCH (n:L) WITH n.p AS p RETURN COUNT(*) AS n",
            ),
            (
                Class::Set,
                "MATCH (n:L) RETURN n AS id UNION ALL MATCH (m:L) RETURN m AS id",
            ),
            (
                Class::TemporalPattern,
                "MATCH (n:L) FOR SYSTEM_TIME AS OF SEQ $at RETURN n.p",
            ),
            (
                Class::TemporalAggregate,
                "MATCH (n:L) FOR SYSTEM_TIME AS OF SEQ $at RETURN COUNT(*) AS n",
            ),
            (
                Class::TemporalAggregate,
                "MATCH (n:L) FOR SYSTEM_TIME AS OF SEQ $at WITH n \
                 CALL fnx.weakly_connected_components(label => 'L', relation => 'R', \
                 parallel => 'collapse', self_loops => 'drop') YIELD vertex \
                 WHERE vertex = n RETURN COUNT(*) AS n",
            ),
            (
                Class::TemporalSet,
                "MATCH (n:L) FOR SYSTEM_TIME AS OF SEQ $at \
                 RETURN n AS id UNION ALL MATCH (m:L) RETURN m AS id",
            ),
        ];
        let mut retained = Vec::new();
        for selected in [CommitSeq(1), CommitSeq(4)] {
            let view = database
                .prepare_sealed_read(
                    &cx,
                    Some(selected),
                    R,
                    SealedLimits::default(),
                    RootReadLimits::default(),
                )
                .await
                .unwrap();
            let params = GqlParameters::new().with_uint64("at", selected.0).unwrap();
            let empty = GqlParameters::new();
            let mut results = Vec::new();
            for (class, text) in cases {
                let arguments = if text.contains("FOR SYSTEM_TIME") {
                    &params
                } else {
                    &empty
                };
                let prepared =
                    fgdb::PreparedNativeRead::prepare(text, arguments, sealed_sql_symbols).unwrap();
                assert_eq!(prepared.facade_class(), class, "{text}");
                let oracle = if text.contains("FOR SYSTEM_TIME") {
                    text.to_owned()
                } else {
                    text.replacen(
                        "MATCH (n:L)",
                        "MATCH (n:L) FOR SYSTEM_TIME AS OF SEQ $at",
                        1,
                    )
                };
                let expected = database
                    .query(
                        &cx,
                        &oracle,
                        &params,
                        sealed_sql_symbols,
                        sealed_sql_policy(),
                    )
                    .unwrap();
                let actual = view
                    .execute_prepared(&cx, &prepared, arguments, sealed_sql_policy(), memory())
                    .unwrap();
                assert_eq!(actual, expected, "{class:?} at {selected:?}");
                let literal = match class {
                    Class::Aggregate | Class::PipelineAggregate | Class::TemporalAggregate => {
                        fgdb::QueryResult::Rows {
                            columns: vec!["n".into()],
                            rows: vec![vec![fgdb::QueryValue::Count(
                                if selected == CommitSeq(1) { 4 } else { 5 },
                            )]],
                        }
                    }
                    Class::Pattern | Class::TemporalPattern => {
                        let null =
                            fgdb::QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Null));
                        // Without ORDER BY, complete projected values use
                        // canonical order: the four NULLs precede INT(99).
                        let mut rows = vec![vec![null]; 4];
                        if selected == CommitSeq(4) {
                            rows.push(vec![fgdb::QueryValue::Value(GraphValue::Scalar(
                                CanonicalScalar::Int(99),
                            ))]);
                        }
                        fgdb::QueryResult::Rows {
                            columns: vec!["n.p".into()],
                            rows,
                        }
                    }
                    Class::Set | Class::TemporalSet => {
                        let ids: &[u128] = if selected == CommitSeq(1) {
                            &[1, 2, 3, u128::MAX]
                        } else {
                            &[1, 2, 3, 4, u128::MAX]
                        };
                        fgdb::QueryResult::Rows {
                            columns: vec!["id".into()],
                            rows: ids
                                .iter()
                                .flat_map(|&id| [id, id])
                                .map(|id| {
                                    vec![fgdb::QueryValue::Value(GraphValue::Vertex(VId(id)))]
                                })
                                .collect(),
                        }
                    }
                };
                if matches!(class, Class::Pattern | Class::TemporalPattern) {
                    let typed = |result: &fgdb::QueryResult| {
                        let fgdb::QueryResult::Rows { rows, .. } = result else {
                            panic!("a pattern read returns rows");
                        };
                        rows.iter()
                            .enumerate()
                            .map(|(index, row)| {
                                assert_eq!(row.len(), 1, "pattern row {index}");
                                match row[0].as_value() {
                                    Some(GraphValue::Scalar(CanonicalScalar::Null)) => None,
                                    Some(GraphValue::Scalar(CanonicalScalar::Int(value))) => {
                                        Some(*value)
                                    }
                                    _ => panic!("pattern row {index} must contain NULL or INT"),
                                }
                            })
                            .collect::<Vec<_>>()
                    };
                    assert_eq!(
                        typed(&actual),
                        typed(&literal),
                        "typed literal {class:?} at {selected:?}",
                    );
                }
                assert_eq!(actual, literal, "literal {class:?} at {selected:?}");
                if class == Class::TemporalAggregate && text.contains("CALL") {
                    let requested = if selected == CommitSeq(1) { 4 } else { 1 };
                    let wrong = GqlParameters::new().with_uint64("at", requested).unwrap();
                    assert!(matches!(
                        view.execute_prepared(
                            &cx, &prepared, &wrong, sealed_sql_policy(), memory(),
                        ),
                        Err(fgdb::SealedQueryError::SnapshotMismatch {
                            requested: actual,
                            retained,
                        }) if actual == CommitSeq(requested) && retained == selected,
                    ));
                    assert!(matches!(
                        view.execute_prepared(
                            &cx,
                            &prepared,
                            arguments,
                            sealed_sql_policy(),
                            FnxMemoryLimits {
                                max_kernel_workspace_bytes: 0,
                                ..memory()
                            },
                        ),
                        Err(fgdb::SealedQueryError::Query(fgdb::QueryError::Aggregate(
                            _
                        ))),
                    ));
                }
                results.push((prepared, arguments.clone(), expected));
            }
            retained.push((view, results));
        }
        let mut update = WriteBatch::new(R);
        update.create_vertex(VId(5), vec![L], vec![(P, CanonicalScalar::Int(100))]);
        assert_eq!(database.write(&commit, update).await.unwrap(), CommitSeq(5));
        drop(database);
        for (view, results) in retained {
            for (prepared, params, expected) in results {
                assert_eq!(
                    view.execute_prepared(&cx, &prepared, &params, sealed_sql_policy(), memory())
                        .unwrap(),
                    expected,
                    "retained {:?} after writer advancement and drop",
                    prepared.facade_class(),
                );
            }
        }
    });
}

#[test]
fn historical_sealed_native_parameters_and_temporal_calls_keep_the_exact_source() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        let vfs = MemVfs::new().unwrap();
        let database = seed(&contexts.commit(), &vfs).await;
        let view = database
            .prepare_sealed_read(
                &cx,
                Some(CommitSeq(1)),
                R,
                SealedLimits::default(),
                RootReadLimits::default(),
            )
            .await
            .unwrap();
        let params = GqlParameters::new().with_int64("maximum", 1).unwrap();
        let prepared = fgdb::PreparedNativeRead::prepare(
            "MATCH (a:L)-[e:R]->(b:L) WHERE e.p <= $maximum RETURN e.p AS p",
            &params,
            sealed_sql_symbols,
        )
        .unwrap();
        assert_eq!(prepared.facade_class(), fgdb::NativeReadClass::Pattern);
        let expected = fgdb::QueryResult::Rows {
            columns: vec!["p".into()],
            rows: vec![
                vec![fgdb::QueryValue::Value(GraphValue::Scalar(
                    CanonicalScalar::Int(1)
                ))];
                16
            ],
        };
        assert_eq!(
            view.execute_prepared(&cx, &prepared, &params, sealed_sql_policy(), memory())
                .unwrap(),
            expected,
        );
        let empty = GqlParameters::new().with_int64("maximum", 0).unwrap();
        assert!(matches!(
            view.execute_prepared(&cx, &prepared, &empty, sealed_sql_policy(), memory()).unwrap(),
            fgdb::QueryResult::Rows { rows, .. } if rows.is_empty(),
        ));
        assert!(matches!(
            view.execute_prepared(
                &cx,
                &prepared,
                &GqlParameters::new(),
                sealed_sql_policy(),
                memory()
            ),
            Err(fgdb::SealedQueryError::Query(
                fgdb::QueryError::PatternText(_)
            )),
        ));
        let temporal = "MATCH (n:L) FOR SYSTEM_TIME AS OF SEQ $at WITH n \
            CALL fnx.weakly_connected_components(label => 'L', relation => 'R', \
            parallel => 'collapse', self_loops => 'drop') YIELD vertex, component \
            WHERE vertex = n RETURN vertex, component ORDER BY vertex";
        let at = GqlParameters::new().with_uint64("at", 1).unwrap();
        let call = fgdb::PreparedNativeRead::prepare(temporal, &at, sealed_sql_symbols).unwrap();
        assert_eq!(call.facade_class(), fgdb::NativeReadClass::TemporalSet);
        let expected = database
            .query(&cx, temporal, &at, sealed_sql_symbols, sealed_sql_policy())
            .unwrap();
        assert_eq!(
            view.execute_prepared(&cx, &call, &at, sealed_sql_policy(), memory())
                .unwrap(),
            expected,
        );
        assert!(matches!(
            view.execute_prepared(
                &cx,
                &call,
                &at,
                sealed_sql_policy(),
                FnxMemoryLimits {
                    max_kernel_workspace_bytes: 0,
                    ..memory()
                }
            ),
            Err(fgdb::SealedQueryError::Query(fgdb::QueryError::Set(_))),
        ));
        for requested in [0, 2, 4] {
            let at = GqlParameters::new().with_uint64("at", requested).unwrap();
            assert!(matches!(
                view.execute_prepared(&cx, &call, &at, sealed_sql_policy(), memory()),
                Err(fgdb::SealedQueryError::SnapshotMismatch {
                    requested: actual,
                    retained: CommitSeq(1),
                }) if actual == CommitSeq(requested),
            ));
        }
    });
}

#[test]
fn retained_temporal_gql_components_match_literal_partitions_across_a_new_bridge() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        let commit = contexts.commit();
        let vfs = MemVfs::new().unwrap();
        let mut database = seed(&commit, &vfs).await;
        let mut bridge = WriteBatch::new(R);
        bridge.add_edge(EId(200), VId(u128::MAX), VId(3), vec![]);
        assert_eq!(database.write(&commit, bridge).await.unwrap(), CommitSeq(4));
        let text = "MATCH (n:L) FOR SYSTEM_TIME AS OF SEQ $at WITH n \
            CALL fnx.weakly_connected_components(label => 'L', relation => 'R', \
            direction => 'directed', parallel => 'collapse', self_loops => 'keep') \
            YIELD vertex, component WHERE vertex = n \
            RETURN vertex, component ORDER BY vertex";
        let mut retained = Vec::new();
        for selected in [CommitSeq(1), CommitSeq(4)] {
            let owner = database
                .prepare_sealed_read(
                    &cx,
                    Some(selected),
                    R,
                    SealedLimits::default(),
                    RootReadLimits::default(),
                )
                .await
                .unwrap();
            let params = GqlParameters::new().with_uint64("at", selected.0).unwrap();
            let prepared =
                fgdb::PreparedNativeRead::prepare(text, &params, sealed_sql_symbols).unwrap();
            assert_eq!(prepared.facade_class(), fgdb::NativeReadClass::TemporalSet);
            // Independent literal labels: the cycle 1/2/3 is connected at
            // both cuts. MAX is isolated until the new directed bridge.
            // The seed also includes a self-loop, parallel incidences, an
            // excluded-label edge and a bridge of the wrong relation.
            let pairs = [
                (1, 1),
                (2, 1),
                (3, 1),
                (u128::MAX, if selected.0 == 1 { u128::MAX } else { 1 }),
            ];
            let expected = fgdb::QueryResult::Rows {
                columns: vec!["vertex".into(), "component".into()],
                rows: pairs
                    .iter()
                    .map(|&(vertex, component)| {
                        vec![
                            fgdb::QueryValue::Value(GraphValue::Vertex(VId(vertex))),
                            fgdb::QueryValue::Value(GraphValue::Vertex(VId(component))),
                        ]
                    })
                    .collect(),
            };
            assert_eq!(
                database
                    .query(&cx, text, &params, sealed_sql_symbols, sealed_sql_policy())
                    .unwrap(),
                expected,
            );
            let actual = owner
                .execute_prepared(&cx, &prepared, &params, sealed_sql_policy(), memory())
                .unwrap();
            assert_eq!(actual, expected);
            let fgdb::QueryResult::Rows { rows, .. } = &actual else {
                panic!("component query must return rows")
            };
            let expected_json = pairs
                .iter()
                .map(|(vertex, component)| format!(r#"["{vertex}","{component}"]"#))
                .collect::<Vec<_>>()
                .join(",");
            let actual_json = rows
                .iter()
                .map(|row| match row.as_slice() {
                    [
                        fgdb::QueryValue::Value(GraphValue::Vertex(vertex)),
                        fgdb::QueryValue::Value(GraphValue::Vertex(component)),
                    ] => format!(r#"["{}","{}"]"#, vertex.0, component.0),
                    other => panic!("typed vertex/component pair: {other:?}"),
                })
                .collect::<Vec<_>>()
                .join(",");
            println!(
                r#"{{"schema":"fgdb.component.gql.v1","source_kind":"retained-native-temporal-gql","source_root":"{:?}","seq":{},"seed":null,"projection":{{"label":"L","relation":"R","direction":"directed","parallel":"collapse","self_loops":"keep"}},"projection_digest":null,"digest_boundary":"native response exposes no projection digest","expected":[{}],"actual":[{}],"terminal":"ok"}}"#,
                owner.partition_root(), selected.0, expected_json, actual_json,
            );
            retained.push((owner, prepared, params, expected));
        }
        let mut later = WriteBatch::new(R);
        later.create_vertex(VId(0), vec![L], vec![]);
        assert_eq!(database.write(&commit, later).await.unwrap(), CommitSeq(5));
        drop(database);
        for (owner, prepared, params, expected) in retained {
            assert_eq!(
                owner
                    .execute_prepared(&cx, &prepared, &params, sealed_sql_policy(), memory())
                    .unwrap(),
                expected,
            );
        }
    });
}

#[test]
fn composed_sealed_calls_match_decoded_queries_for_sparse_pages_aliases_and_projections() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        let vfs = MemVfs::new().unwrap();
        let database = seed(&contexts.commit(), &vfs).await;
        let view = database
            .prepare_sealed_read(
                &cx,
                None,
                R,
                SealedLimits::default(),
                RootReadLimits::default(),
            )
            .await
            .unwrap();
        assert_eq!(view.snapshot_seq(), CommitSeq(3));
        assert_eq!(view.frontier(), CommitSeq(3));
        assert_eq!(view.relation(), R);
        let texts = [
            sealed_sql_bfs(
                "vertex, distance",
                "RETURN vertex, distance ORDER BY vertex",
            ),
            sealed_sql_bfs(
                "vertex AS n, distance AS hops",
                "RETURN n, hops ORDER BY hops DESC, n SKIP 1 LIMIT 1",
            ),
            sealed_sql_bfs("vertex, distance", "RETURN vertex, distance LIMIT 0"),
            sealed_sql_bfs(
                "vertex, distance",
                "RETURN vertex, distance ORDER BY distance DESC SKIP 9223372036854775807 LIMIT 2",
            ),
            sealed_sql_bfs(
                "vertex, distance",
                "WHERE distance >= 1 WITH vertex, distance + 10 AS score \
                 RETURN vertex, score ORDER BY score DESC, vertex",
            ),
            "CALL fnx.triangles(label => 'L', relation => 'R', direction => 'undirected', \
             parallel => 'collapse', self_loops => 'drop') YIELD vertex AS n, triangles AS count \
             RETURN n, count ORDER BY count DESC, n"
                .to_owned(),
            sealed_sql_weighted("directed", "sum"),
            sealed_sql_weighted("reversed", "min"),
            sealed_sql_bfs(
                "vertex AS n, distance",
                "MATCH (n)-[:R]->(m) RETURN n, m, distance ORDER BY n, m, distance LIMIT 5",
            ),
        ];
        let queries: Vec<_> = texts.iter().map(|text| sealed_sql_rows(text)).collect();
        let mut actual = Vec::new();
        for (text, query) in texts.iter().zip(&queries) {
            actual.push(
                view.execute_graph_set_governed(&cx, query, sealed_sql_policy(), memory())
                    .unwrap_or_else(|error| panic!("{text}: {error}")),
            );
        }
        assert_eq!(
            actual[0].value,
            sealed_sql_distances(&[(1, 0), (2, 1), (3, 2)])
        );
        assert_eq!(actual[1].value, sealed_sql_distances(&[(2, 1)]));
        assert_eq!(queries[1].columns(), &["n", "hops"]);
        assert!(actual[2].value.is_empty());
        assert!(actual[3].value.is_empty());
        assert_eq!(actual[4].value, sealed_sql_distances(&[(3, 12), (2, 11)]));
        assert_eq!(
            actual[5].value,
            sealed_sql_distances(&[(1, 1), (2, 1), (3, 1), (u128::MAX, 0)])
        );
        // Thirteen parallel 1->2 identities have weights 5 + twelve ones at
        // this cut. The excluded-label Boolean edge must never be read as a weight.
        assert_eq!(
            actual[6].value,
            sealed_sql_costs(&[(1, 0.0), (2, 17.0), (3, 18.0)])
        );
        assert_eq!(
            actual[7].value,
            sealed_sql_costs(&[(1, 0.0), (2, 2.0), (3, 1.0)])
        );
        // This is a retention witness, not an allocator/RSS measurement. Run
        // every compressed query before the independent decoded oracle can
        // populate the owner's decoded analytics cache.
        let cache = database.read_cache_status(&cx).unwrap();
        assert_eq!(cache.projections.entries, 0);
        assert_eq!(cache.projections.retained_bytes, 0);
        for ((text, query), actual) in texts.iter().zip(&queries).zip(actual) {
            let decoded = database
                .execute_graph_set_governed_at(&cx, query, view.snapshot_seq(), sealed_sql_policy())
                .unwrap_or_else(|error| panic!("decoded {text}: {error}"));
            assert_eq!(actual.value, decoded.value, "{text}");
        }
    });
}

#[test]
fn composed_sealed_grouping_matches_decoded_and_hand_counted_components_and_distances() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        let vfs = MemVfs::new().unwrap();
        let database = seed(&contexts.commit(), &vfs).await;
        let view = database
            .prepare_sealed_read(
                &cx,
                None,
                R,
                SealedLimits::default(),
                RootReadLimits::default(),
            )
            .await
            .unwrap();
        let params = GqlParameters::new();
        let texts = [
            "CALL fnx.weakly_connected_components(label => 'L', relation => 'R', \
             parallel => 'collapse', self_loops => 'drop') YIELD vertex, component \
             RETURN component, COUNT(*) AS n GROUP BY component ORDER BY component"
                .to_owned(),
            sealed_sql_bfs(
                "vertex, distance",
                "WITH vertex, distance, distance % 2 AS parity \
                 RETURN parity, COUNT(*) AS n, SUM(distance) AS total \
                 GROUP BY parity ORDER BY parity",
            ),
        ];
        let expected = [
            vec![
                vec![
                    fgdb::QueryValue::Value(GraphValue::Vertex(VId(1))),
                    fgdb::QueryValue::Count(3),
                ],
                vec![
                    fgdb::QueryValue::Value(GraphValue::Vertex(VId(u128::MAX))),
                    fgdb::QueryValue::Count(1),
                ],
            ],
            vec![
                vec![
                    fgdb::QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Int(0))),
                    fgdb::QueryValue::Count(2),
                    fgdb::QueryValue::Integer(2),
                ],
                vec![
                    fgdb::QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Int(1))),
                    fgdb::QueryValue::Count(1),
                    fgdb::QueryValue::Integer(1),
                ],
            ],
        ];
        for (text, expected) in texts.iter().zip(expected) {
            let fgdb::PreparedNativeRead::PipelineAggregate(prepared) =
                fgdb::PreparedNativeRead::prepare(text, &params, sealed_sql_symbols).unwrap()
            else {
                panic!("CALL grouping must retain its relational input");
            };
            let slots = prepared.output_slots().to_vec();
            let query = prepared.bind_relation_parameters(&params).unwrap();
            let actual = view
                .execute_graph_set_aggregate_governed(&cx, &query, sealed_sql_policy(), memory())
                .unwrap();
            let cells: Vec<Vec<_>> = actual
                .value
                .iter()
                .map(|row| {
                    slots
                        .iter()
                        .map(|slot| match *slot {
                            GraphAggregateTextSlot::GroupKey(index) => {
                                fgdb::QueryValue::Value(row.keys()[index].clone())
                            }
                            GraphAggregateTextSlot::Aggregate(index) => row.values()[index].clone(),
                        })
                        .collect()
                })
                .collect();
            assert_eq!(cells, expected, "{text}");
            let decoded = database
                .execute_graph_set_aggregate_governed_at(
                    &cx,
                    &query,
                    view.snapshot_seq(),
                    sealed_sql_policy(),
                )
                .unwrap();
            assert_eq!(actual.value, decoded.value, "{text}");
            let exact = GqlQueryPolicy::new(
                sealed_sql_policy().rows.max_snapshot_records().unwrap(),
                actual.rows.result_rows,
                actual.evaluator.work_units,
                actual.evaluator.scratch_entries,
            );
            assert_eq!(
                view.execute_graph_set_aggregate_governed(&cx, &query, exact, memory())
                    .unwrap(),
                actual
            );
            let mut short = exact;
            short.evaluator.max_work_units -= 1;
            assert!(
                view.execute_graph_set_aggregate_governed(&cx, &query, short, memory())
                    .is_err()
            );
        }
    });
}

#[test]
fn composed_sealed_queries_keep_their_cut_after_writer_advance_drop_and_reopen() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        let commit = contexts.commit();
        let vfs = MemVfs::new().unwrap();
        let mut database = seed(&commit, &vfs).await;
        let historical = database
            .prepare_sealed_read(
                &cx,
                Some(CommitSeq(1)),
                R,
                SealedLimits::default(),
                RootReadLimits::default(),
            )
            .await
            .unwrap();
        let pinned = database
            .prepare_sealed_read(
                &cx,
                None,
                R,
                SealedLimits::default(),
                RootReadLimits::default(),
            )
            .await
            .unwrap();
        assert_eq!(historical.snapshot_seq(), CommitSeq(1));
        assert_eq!(historical.frontier(), CommitSeq(3));
        assert_eq!(historical.partition_root(), pinned.partition_root());
        let prior_root = pinned.partition_root();
        let weighted = sealed_sql_rows(&sealed_sql_weighted("directed", "sum"));
        let bfs = sealed_sql_rows(&sealed_sql_bfs(
            "vertex, distance",
            "RETURN vertex, distance ORDER BY vertex",
        ));
        let before = historical
            .execute_graph_set_governed(&cx, &weighted, sealed_sql_policy(), memory())
            .unwrap();
        let old_head = pinned
            .execute_graph_set_governed(&cx, &weighted, sealed_sql_policy(), memory())
            .unwrap();
        assert_eq!(
            before.value,
            sealed_sql_costs(&[(1, 0.0), (2, 13.0), (3, 14.0)])
        );
        assert_eq!(
            old_head.value,
            sealed_sql_costs(&[(1, 0.0), (2, 17.0), (3, 18.0)])
        );

        let mut change = WriteBatch::new(R);
        change.set_edge_property(EId(1), P, Some(CanonicalScalar::Int(9)));
        change.add_edge(
            EId(101),
            VId(3),
            VId(u128::MAX),
            vec![(P, CanonicalScalar::Int(1))],
        );
        let now = database.write(&commit, change).await.unwrap();
        let latest = database
            .prepare_sealed_read(
                &cx,
                None,
                R,
                SealedLimits::default(),
                RootReadLimits::default(),
            )
            .await
            .unwrap();
        assert_eq!(latest.snapshot_seq(), now);
        assert_ne!(latest.partition_root(), prior_root);
        let reached = latest
            .execute_graph_set_governed(&cx, &bfs, sealed_sql_policy(), memory())
            .unwrap();
        assert_eq!(
            reached.value,
            sealed_sql_distances(&[(1, 0), (2, 1), (3, 2), (u128::MAX, 3)])
        );
        drop(database);

        assert_eq!(
            historical
                .execute_graph_set_governed(&cx, &weighted, sealed_sql_policy(), memory())
                .unwrap(),
            before
        );
        assert_eq!(
            pinned
                .execute_graph_set_governed(&cx, &weighted, sealed_sql_policy(), memory())
                .unwrap(),
            old_head
        );
        assert_eq!(pinned.partition_root(), prior_root);
        assert_eq!(
            pinned
                .execute_graph_set_governed(&cx, &bfs, sealed_sql_policy(), memory())
                .unwrap()
                .value,
            sealed_sql_distances(&[(1, 0), (2, 1), (3, 2)])
        );
        let reopened = Database::open_with_vfs(&commit, vfs.clone(), vfs.database_dir(), keys())
            .await
            .unwrap();
        for (cut, expected) in [(CommitSeq(1), before.value), (CommitSeq(3), old_head.value)] {
            let after_reopen = reopened
                .prepare_sealed_read(
                    &cx,
                    Some(cut),
                    R,
                    SealedLimits::default(),
                    RootReadLimits::default(),
                )
                .await
                .unwrap();
            assert_eq!(
                after_reopen
                    .execute_graph_set_governed(&cx, &weighted, sealed_sql_policy(), memory())
                    .unwrap()
                    .value,
                expected
            );
            assert_eq!(
                reopened
                    .execute_graph_set_governed_at(&cx, &weighted, cut, sealed_sql_policy())
                    .unwrap()
                    .value,
                expected
            );
        }
        assert_eq!(
            latest
                .execute_graph_set_governed(&cx, &bfs, sealed_sql_policy(), memory())
                .unwrap(),
            reached
        );
    });
}

#[test]
fn composed_calls_reuse_one_projection_across_remaining_allowances() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        let vfs = MemVfs::new().unwrap();
        let database = seed(&contexts.commit(), &vfs).await;
        let view = database
            .prepare_sealed_read(
                &cx,
                None,
                R,
                SealedLimits::default(),
                RootReadLimits::default(),
            )
            .await
            .unwrap();
        let arm = |source| {
            format!(
                "CALL fnx.single_source_shortest_path_length(args => [{source}], \
             label => 'L', relation => 'R', direction => 'directed', \
             parallel => 'collapse', self_loops => 'drop') YIELD vertex, distance \
             RETURN vertex, distance"
            )
        };
        let query = sealed_sql_rows(&format!("{} UNION ALL {}", arm(1), arm(2)));
        cache_limit(&database, &cx, 0);
        let cold_probe = Arc::new(SimulationCheckpointProbe::new(None));
        let cold = view
            .execute_graph_set_governed(
                &cx.with_checkpoint_probe(Arc::clone(&cold_probe)),
                &query,
                sealed_sql_policy(),
                memory(),
            )
            .unwrap();
        assert_eq!(
            cold.value,
            sealed_sql_distances(&[(1, 0), (1, 2), (2, 0), (2, 1), (3, 1), (3, 2),])
        );
        assert_eq!(
            database
                .read_cache_status(&cx)
                .unwrap()
                .sealed_projections
                .entries,
            0
        );

        cache_limit(&database, &cx, 1 << 20);
        let first_probe = Arc::new(SimulationCheckpointProbe::new(None));
        let first = view
            .execute_graph_set_governed(
                &cx.with_checkpoint_probe(Arc::clone(&first_probe)),
                &query,
                sealed_sql_policy(),
                memory(),
            )
            .unwrap();
        assert_eq!(
            first, cold,
            "both kernels and all original construction charges remain"
        );
        assert!(
            first_probe.calls() < cold_probe.calls(),
            "the second CALL must avoid a physical projection rebuild"
        );
        let retained = database.read_cache_status(&cx).unwrap().sealed_projections;
        assert_eq!(
            retained.entries, 1,
            "the second CALL has a smaller remaining allowance but the same projection"
        );
        assert!(retained.retained_bytes > 0);

        let exact = GqlQueryPolicy::new(
            sealed_sql_policy().rows.max_snapshot_records().unwrap(),
            cold.rows.result_rows,
            cold.evaluator.work_units,
            cold.evaluator.scratch_entries,
        );
        let warm_probe = Arc::new(SimulationCheckpointProbe::new(None));
        let warm = view
            .execute_graph_set_governed(
                &cx.with_checkpoint_probe(Arc::clone(&warm_probe)),
                &query,
                exact,
                memory(),
            )
            .unwrap();
        assert_eq!(warm, cold);
        assert!(
            warm_probe.calls() < first_probe.calls(),
            "both complete projections can now be reused"
        );
        assert_eq!(
            database.read_cache_status(&cx).unwrap().sealed_projections,
            retained
        );
        for stop in [1, warm_probe.calls() / 2, warm_probe.calls()] {
            let probe = Arc::new(SimulationCheckpointProbe::new(Some(stop)));
            let interrupted = cx.with_checkpoint_probe(Arc::clone(&probe));
            assert!(matches!(
                view.execute_graph_set_governed(&interrupted, &query, exact, memory()),
                Err(fgdb_gql::GqlQueryError::Interrupted(_))
            ));
            assert_eq!(probe.calls(), stop);
            assert_eq!(
                database.read_cache_status(&cx).unwrap().sealed_projections,
                retained
            );
        }
        assert_eq!(
            view.execute_graph_set_governed(&cx, &query, exact, memory())
                .unwrap(),
            cold
        );
    });
}

#[test]
fn compatible_composed_reuse_keeps_the_first_cold_budget_refusal() {
    #[derive(Debug, PartialEq, Eq)]
    enum Refusal {
        Rows(fgdb_gql::GqlBudgetExceeded),
        Evaluator(fgdb_gql::GlaLimitExceeded),
        Source(&'static str, u128, u128),
        Kernel(&'static str, usize, usize),
    }
    fn refusal<C: core::fmt::Debug>(
        error: fgdb_gql::GqlQueryError<fgdb_gql::GraphSetExecutionError<fgdb::GqlError>, C>,
    ) -> Refusal {
        use fgdb_gql::{GqlQueryError, GraphSetExecutionError};
        match error {
            GqlQueryError::Rows(error) => Refusal::Rows(error),
            GqlQueryError::Evaluator(error) => Refusal::Evaluator(error),
            GqlQueryError::Source(GraphSetExecutionError::Source(fgdb::GqlError::Procedure(
                fgdb::ProcedureError::Read(error),
            ))) => match *error {
                fgdb_prism::FnxReadError::SourceLimit {
                    resource,
                    limit,
                    requested,
                } => Refusal::Source(resource, limit, requested),
                other => panic!("unexpected source refusal: {other:?}"),
            },
            GqlQueryError::Source(GraphSetExecutionError::Source(fgdb::GqlError::Procedure(
                fgdb::ProcedureError::SealedRead(error),
            ))) => match *error {
                FnxSealedReadError::Execution(fgdb_prism::FnxSealedExecutionError::Execution(
                    fgdb_prism::FnxExecutionError::LimitExceeded {
                        resource,
                        limit,
                        requested,
                    },
                )) => Refusal::Kernel(resource, limit, requested),
                other => panic!("unexpected sealed refusal: {other:?}"),
            },
            other => panic!("unexpected refusal: {other:?}"),
        }
    }

    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        let vfs = MemVfs::new().unwrap();
        let database = seed(&contexts.commit(), &vfs).await;
        let view = database
            .prepare_sealed_read(
                &cx,
                None,
                R,
                SealedLimits::default(),
                RootReadLimits::default(),
            )
            .await
            .unwrap();
        let query = sealed_sql_rows(&sealed_sql_bfs(
            "vertex, distance",
            "RETURN vertex, distance ORDER BY vertex",
        ));
        cache_limit(&database, &cx, 0);
        let cold = view
            .execute_graph_set_governed(&cx, &query, sealed_sql_policy(), memory())
            .unwrap();
        let exact = GqlQueryPolicy::new(
            sealed_sql_policy().rows.max_snapshot_records().unwrap(),
            cold.rows.result_rows,
            cold.evaluator.work_units,
            cold.evaluator.scratch_entries,
        );
        let mut policies = vec![
            GqlQueryPolicy::new(0, 100, 100_000_000, 10_000_000),
            GqlQueryPolicy::new(1_000_000, 100, 0, 10_000_000),
            GqlQueryPolicy::new(1_000_000, 100, 100_000_000, 0),
            GqlQueryPolicy::new(1_000_000, 0, 100_000_000, 10_000_000),
            // Multiple insufficient dimensions must preserve the original
            // source-before-GQL refusal, not just return some budget error.
            GqlQueryPolicy::new(0, 0, 100_000_000, 0),
        ];
        let mut short_work = exact;
        short_work.evaluator.max_work_units -= 1;
        policies.push(short_work);
        let mut short_scratch = exact;
        short_scratch.evaluator.max_scratch_entries -= 1;
        policies.push(short_scratch);
        policies.push(GqlQueryPolicy::new(
            exact.rows.max_snapshot_records().unwrap(),
            cold.rows.result_rows - 1,
            exact.evaluator.max_work_units,
            exact.evaluator.max_scratch_entries,
        ));
        // This admits every GQL snapshot record but not the source visitor's
        // additional work. Checking only AdmissionUsage would wrongly reuse.
        policies.push(GqlQueryPolicy::new(
            cold.rows.snapshot_records,
            100,
            100_000_000,
            10_000_000,
        ));
        let expected: Vec<_> = policies
            .iter()
            .map(|&policy| {
                refusal(
                    view.execute_graph_set_governed(&cx, &query, policy, memory())
                        .unwrap_err(),
                )
            })
            .collect();
        assert!(matches!(
            expected.last(),
            Some(Refusal::Source("work units", _, _))
        ));
        cache_limit(&database, &cx, 1 << 20);
        assert_eq!(
            view.execute_graph_set_governed(&cx, &query, sealed_sql_policy(), memory())
                .unwrap(),
            cold
        );
        let retained = database.read_cache_status(&cx).unwrap().sealed_projections;
        assert_eq!(retained.entries, 1);
        for (policy, expected) in policies.into_iter().zip(expected) {
            let error = view
                .execute_graph_set_governed(&cx, &query, policy, memory())
                .unwrap_err();
            assert_eq!(
                refusal(error),
                expected,
                "cold and warm must name the same dimension, limit and first refused amount"
            );
            assert_eq!(
                database.read_cache_status(&cx).unwrap().sealed_projections,
                retained
            );
        }
        assert_eq!(
            view.execute_graph_set_governed(&cx, &query, exact, memory())
                .unwrap(),
            cold
        );
        assert_eq!(
            database.read_cache_status(&cx).unwrap().sealed_projections,
            retained
        );
    });
}

#[test]
fn composed_sealed_queries_keep_exact_admission_and_source_failures_before_empty_pages() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        let vfs = MemVfs::new().unwrap();
        let database = seed(&contexts.commit(), &vfs).await;
        let view = database
            .prepare_sealed_read(&cx, None, R, SealedLimits::default(), RootReadLimits::default())
            .await
            .unwrap();
        let ranked = sealed_sql_rows(&sealed_sql_bfs(
            "vertex, distance",
            "RETURN vertex, distance ORDER BY distance DESC, vertex LIMIT 1",
        ));
        let zero = sealed_sql_rows(&sealed_sql_bfs(
            "vertex, distance",
            "RETURN vertex, distance LIMIT 0",
        ));
        cache_limit(&database, &cx, 0);
        let cold = view.execute_graph_set_governed(&cx, &ranked, sealed_sql_policy(), memory()).unwrap();
        assert_eq!(database.read_cache_status(&cx).unwrap().sealed_projections.entries, 0);
        cache_limit(&database, &cx, 1 << 20);
        let actual = view.execute_graph_set_governed(&cx, &ranked, sealed_sql_policy(), memory()).unwrap();
        assert_eq!(actual, cold, "retaining a projection preserves complete query accounting");
        let retained = database.read_cache_status(&cx).unwrap().sealed_projections;
        assert_eq!(retained.entries, 1);
        assert!(retained.retained_bytes > 0 && retained.retained_bytes <= retained.max_bytes);
        assert_eq!(view.execute_graph_set_governed(&cx, &ranked, sealed_sql_policy(), memory()).unwrap(), cold, "warm admission replays the cold source and construction cost");
        assert_eq!(database.read_cache_status(&cx).unwrap().sealed_projections, retained);
        assert_eq!(actual.value, sealed_sql_distances(&[(3, 2)]));
        let exact = GqlQueryPolicy::new(
            sealed_sql_policy().rows.max_snapshot_records().unwrap(),
            actual.rows.result_rows,
            actual.evaluator.work_units,
            actual.evaluator.scratch_entries,
        );
        assert_eq!(view.execute_graph_set_governed(&cx, &ranked, exact, memory()).unwrap(), actual);
        for dimension in 0..3 {
            let mut short = exact;
            match dimension {
                0 => short.evaluator.max_work_units -= 1,
                1 => short.evaluator.max_scratch_entries -= 1,
                2 => short = GqlQueryPolicy::new(
                    exact.rows.max_snapshot_records().unwrap(),
                    actual.rows.result_rows - 1,
                    exact.evaluator.max_work_units,
                    exact.evaluator.max_scratch_entries,
                ),
                _ => unreachable!(),
            }
            assert!(view.execute_graph_set_governed(&cx, &ranked, short, memory()).is_err(), "dimension {dimension}");
        }
        let after_short = database.read_cache_status(&cx).unwrap().sealed_projections;
        // A fitting construction reuses the original owner; a construction
        // that cannot fit fails cold and retains no partial projection.
        assert_eq!(after_short, retained);
        assert!(after_short.retained_bytes <= after_short.max_bytes);
        assert_eq!(view.execute_graph_set_governed(&cx, &ranked, sealed_sql_policy(), memory()).unwrap(), cold);

        let mut opt = options();
        opt.projection.directedness = Directedness::Directed;
        let completed = database.execute_fnx_sealed(
            &cx,
            &call("CALL fnx.single_source_shortest_path_length(1) YIELD vertex, distance"),
            opt,
            memory(),
            SealedLimits::default(),
        ).await.unwrap();
        assert_eq!(completed.analytics.rows.len(), 3);
        // Retained delivery preserves the registered complete-result admission,
        // including aliases and every reachable row, even for a one/zero-row page.
        let bytes = 2 * size_of::<String>() + "vertex".len() + "distance".len()
            + 3 * (size_of::<Vec<FnxValue>>() + 2 * size_of::<FnxValue>());
        let exact_memory = FnxMemoryLimits {
            max_kernel_workspace_bytes: completed.analytics.certificate.kernel_workspace_bytes,
            max_result_bytes: bytes,
        };
        assert_eq!(view.execute_graph_set_governed(&cx, &ranked, sealed_sql_policy(), exact_memory).unwrap(), actual);
        assert!(view.execute_graph_set_governed(&cx, &zero, sealed_sql_policy(), exact_memory).unwrap().value.is_empty());
        for workspace in [false, true] {
            let mut short = exact_memory;
            if workspace {
                short.max_kernel_workspace_bytes -= 1;
            } else {
                short.max_result_bytes -= 1;
            }
            for query in [&ranked, &zero] {
                assert!(view.execute_graph_set_governed(&cx, query, sealed_sql_policy(), short).is_err());
            }
        }

        for (relation, expected) in [(Some("Other"), Some(RelationId(8))), (None, None)] {
            let relation = relation.map_or_else(String::new, |name| format!(", relation => '{name}'"));
            let query = sealed_sql_rows(&format!(
                "CALL fnx.single_source_shortest_path_length(args => [1]{relation}) \
                 YIELD vertex, distance RETURN vertex, distance LIMIT 0"
            ));
            let error = view.execute_graph_set_governed(&cx, &query, sealed_sql_policy(), memory()).unwrap_err();
            assert!(matches!(error,
                fgdb_gql::GqlQueryError::Source(fgdb_gql::GraphSetExecutionError::Source(
                    fgdb::GqlError::Procedure(fgdb::ProcedureError::SealedRelationMismatch { requested, retained })
                )) if requested == expected && retained == R
            ));
        }
        for (index, text) in [
            "CALL fnx.single_source_shortest_path_length(args => [42], relation => 'R', \
             parallel => 'collapse') YIELD vertex, distance RETURN vertex, distance LIMIT 0",
            // Removing the label selector admits edge99's Boolean property.
            "CALL fnx.single_source_dijkstra_path_length(args => [1], relation => 'R', \
             weight => 'p', missing => 'reject', parallel => 'min', self_loops => 'drop') \
             YIELD vertex, distance RETURN vertex, distance LIMIT 0",
        ].into_iter().enumerate() {
            let query = sealed_sql_rows(text);
            let error = view.execute_graph_set_governed(&cx, &query, sealed_sql_policy(), memory()).unwrap_err();
            let fgdb_gql::GqlQueryError::Source(fgdb_gql::GraphSetExecutionError::Source(
                fgdb::GqlError::Procedure(fgdb::ProcedureError::SealedRead(error)),
            )) = error else {
                panic!("expected compressed source/kernel refusal for {text}: {error}");
            };
            match index {
                0 => assert!(matches!(*error,
                    FnxSealedReadError::Execution(fgdb_prism::FnxSealedExecutionError::Execution(
                        fgdb_prism::FnxExecutionError::UnknownSource(VId(42))
                    ))
                )),
                1 => assert!(matches!(*error,
                    FnxSealedReadError::Projection(fgdb_prism::SealedProjectionError::Weight {
                        edge: EId(99), reason: fgdb_prism::FnxWeightError::NotNumeric,
                    })
                )),
                _ => unreachable!(),
            }
        }
        let late = sealed_sql_rows(&sealed_sql_bfs(
            "vertex, distance",
            "WITH vertex, 1 / (2 - distance) AS value \
             RETURN vertex, value ORDER BY vertex LIMIT 1",
        ));
        assert!(matches!(
            view.execute_graph_set_governed(&cx, &late, sealed_sql_policy(), memory()),
            Err(fgdb_gql::GqlQueryError::Source(fgdb_gql::GraphSetExecutionError::Projection { .. }))
                | Err(fgdb_gql::GqlQueryError::Data(_))
        ));
        let no_source = GqlQueryPolicy::new(0, 0, 100_000_000, 10_000_000);
        assert!(view.execute_graph_set_governed(&cx, &zero, no_source, memory()).is_err());
        let measured = Arc::new(SimulationCheckpointProbe::new(None));
        let observed = cx.with_checkpoint_probe(Arc::clone(&measured));
        assert_eq!(view.execute_graph_set_governed(&observed, &ranked, sealed_sql_policy(), memory()).unwrap(), actual);
        assert!(measured.calls() > 2);
        for stop in [1, measured.calls() / 2, measured.calls()] {
            let probe = Arc::new(SimulationCheckpointProbe::new(Some(stop)));
            let interrupted = cx.with_checkpoint_probe(Arc::clone(&probe));
            assert!(view.execute_graph_set_governed(&interrupted, &ranked, sealed_sql_policy(), memory()).is_err());
            assert_eq!(probe.calls(), stop);
        }
        assert_eq!(view.execute_graph_set_governed(&cx, &ranked, sealed_sql_policy(), memory()).unwrap(), actual);

        assert!(database.prepare_sealed_read(
            &cx, None, R, SealedLimits::default(),
            RootReadLimits { max_source_bytes: 0, ..RootReadLimits::default() },
        ).await.is_err());
        assert!(database.prepare_sealed_read(
            &cx, None, R,
            SealedLimits { max_incidences: 0, ..SealedLimits::default() },
            RootReadLimits::default(),
        ).await.is_err());
        assert!(database.prepare_sealed_read(
            &cx, Some(CommitSeq(4)), R, SealedLimits::default(), RootReadLimits::default(),
        ).await.is_err());
    });
}
