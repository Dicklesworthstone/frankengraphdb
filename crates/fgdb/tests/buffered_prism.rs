//! Public Chronicle -> authenticated buffered source -> existing Prism kernels.
//! Explicit historical graphs and the resident source are independent oracles.
#![recursion_limit = "256"]

use asupersync::lab::run_async_under_lab;
use fgdb::{
    BufferLimits, BufferedReadError, BufferedReadLimits, BufferedReadView, Database, DatabaseKeys,
    MemVfs, MemoryPool, WriteBatch,
};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_prism::{
    Directedness, FnxCallSpec, FnxExecutionError, FnxParameters, FnxReadError, FnxReadOptions,
    FnxSelection, FnxWeightError, FnxWeightSpec, MissingWeightPolicy, ParallelEdgePolicy,
    ProjectionEdge, ProjectionError, SelfLoopPolicy, SnapshotBinding, SnapshotGraphView,
};
use fgdb_types::context::SimulationCheckpointProbe;
use fgdb_types::{
    CanonicalScalar, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts, QueryCx, VId,
};
use std::sync::Arc;

const R: RelationId = RelationId(7);
const L: LabelId = LabelId(10);
const P: PropertyKeyId = PropertyKeyId(9);
const HIGH_EDGE: EId = EId(1_u128 << 100);
const RANK: &str = "CALL fnx.pagerank(0.85,200,1e-9,true) YIELD score AS rank,vertex AS id";
const DIJKSTRA: &str =
    "CALL fnx.single_source_dijkstra_path_length(1,NULL,true) YIELD distance,vertex";

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0xd1; 32],
        DatabaseSecurityNamespaceId([0xd2; 32]),
        [0xd3; 32],
    )
}

fn limits() -> BufferedReadLimits {
    BufferedReadLimits {
        max_root_bytes: 64 * 1024,
        max_source_bytes: 4 * 1024 * 1024,
        max_blocks: 512,
        max_vertex_patches: 512,
        max_work: 1_000_000,
        buffer: BufferLimits {
            max_frames: 1,
            max_ghost_entries: 2,
            max_extent_bytes: 16 * 1024,
        },
    }
}

fn pool() -> MemoryPool {
    MemoryPool::new(16 * 1024 * 1024, 0).unwrap()
}

fn options() -> FnxReadOptions {
    let mut options = FnxReadOptions::whole_graph(None);
    options.selection = FnxSelection {
        vertex_label: Some(L),
        relation: Some(R),
        weight: FnxWeightSpec::Property {
            key: P,
            missing: MissingWeightPolicy::Reject,
        },
    };
    options.projection.parallel_edges = ParallelEdgePolicy::Sum;
    options.projection.self_loops = SelfLoopPolicy::Drop;
    options.source_limits.max_work_units = 1_000_000;
    options.source_limits.max_scratch_entries = 1000;
    options.source_limits.max_staging_bytes = 64 * 1024;
    options.projection_limits.max_vertices = 100;
    options.projection_limits.max_input_edges = 100;
    options.projection_limits.max_adjacency_entries = 200;
    options.projection_limits.max_workspace_bytes = 256 * 1024;
    options.execution_limits.max_iterations = 200;
    options.execution_limits.max_result_rows = 100;
    options.execution_limits.max_estimated_work = 1_000_000;
    options
}

async fn cold(
    contexts: &PurposeContexts,
    vfs: &MemVfs,
    pool: MemoryPool,
) -> BufferedReadView<MemVfs> {
    Database::open_buffered_read_view_with_vfs(
        &contexts.commit(),
        vfs.clone(),
        vfs.database_dir(),
        keys(),
        pool,
        limits(),
    )
    .await
    .unwrap()
}

async fn project_buffered(
    view: &mut BufferedReadView<MemVfs>,
    cx: &QueryCx,
    options: FnxReadOptions,
) -> Result<SnapshotGraphView, FnxReadError<BufferedReadError, Box<asupersync::error::Error>>> {
    let at = options.as_of.unwrap_or(view.frontier());
    view.prism_projection_at(
        cx,
        at,
        options.selection,
        options.projection,
        options.projection_limits,
        options.source_limits,
    )
    .await
}

async fn seed(contexts: &PurposeContexts, vfs: &MemVfs) -> Database<MemVfs> {
    let commit = contexts.commit();
    let mut db = Database::create_with_vfs(&commit, vfs.clone(), vfs.database_dir(), keys())
        .await
        .unwrap();
    let mut first = WriteBatch::new(R);
    for vid in [1, 2, 3, u128::MAX] {
        first.create_vertex(VId(vid), vec![L], vec![]);
    }
    first.create_vertex(VId(99), vec![LabelId(11)], vec![]);
    for (eid, source, target, weight) in [(0, 1, 2, 2), (2, 2, 3, 1), (3, 3, 1, 4)] {
        first.add_edge(
            EId(eid),
            VId(source),
            VId(target),
            vec![(P, CanonicalScalar::Int(weight))],
        );
    }
    first.add_edge(
        HIGH_EDGE,
        VId(1),
        VId(2),
        vec![(P, CanonicalScalar::Int(5))],
    );
    // Bad values outside the induced selection and on a dropped loop must
    // never become weight refusals. Max-width identities also remain intact.
    first.add_edge(
        EId(5),
        VId(3),
        VId(99),
        vec![(P, CanonicalScalar::Bool(false))],
    );
    first.add_edge(
        EId(u128::MAX),
        VId(2),
        VId(2),
        vec![(P, CanonicalScalar::Bool(false))],
    );
    assert_eq!(db.write(&commit, first).await.unwrap(), CommitSeq(1));
    let mut other_relation = WriteBatch::new(RelationId(8));
    other_relation.add_edge(
        EId(6),
        VId(1),
        VId(3),
        vec![(P, CanonicalScalar::Bool(false))],
    );
    assert_eq!(
        db.write(&commit, other_relation).await.unwrap(),
        CommitSeq(2)
    );
    db
}

// Independent expected input, without reading a buffered or resident row.
fn oracle(binding: SnapshotBinding, options: FnxReadOptions) -> SnapshotGraphView {
    let cut = binding.as_of.0;
    let vertices = match cut {
        0 => vec![],
        1 | 2 => vec![VId(1), VId(2), VId(3), VId(u128::MAX)],
        3 => vec![VId(1), VId(2), VId(u128::MAX)],
        _ => vec![VId(1), VId(u128::MAX)],
    };
    let mut edges = Vec::new();
    if (1..=3).contains(&cut) {
        edges.extend([
            ProjectionEdge {
                eid: EId(0),
                source: VId(1),
                target: VId(2),
                weight: if cut == 3 { 9.0 } else { 2.0 },
            },
            ProjectionEdge {
                eid: HIGH_EDGE,
                source: VId(1),
                target: VId(2),
                weight: 5.0,
            },
        ]);
        if cut <= 2 {
            edges.extend([
                ProjectionEdge {
                    eid: EId(2),
                    source: VId(2),
                    target: VId(3),
                    weight: 1.0,
                },
                ProjectionEdge {
                    eid: EId(3),
                    source: VId(3),
                    target: VId(1),
                    weight: 4.0,
                },
            ]);
        }
    }
    SnapshotGraphView::build(
        binding,
        &vertices,
        &edges,
        options.projection,
        options.projection_limits,
    )
    .unwrap()
}

#[test]
fn buffered_analytics_match_independent_history_and_resident_calls_without_losing_the_pin() {
    let ((), report) = run_async_under_lab(0x6270_7201, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let vfs = MemVfs::new().unwrap();
        let mut db = seed(&contexts, &vfs).await;
        let old_pool = pool();
        let mut old = cold(&contexts, &vfs, old_pool.clone()).await;
        let old_resident = db.read_session().unwrap();
        let parameters = FnxParameters::new();
        let first = old
            .call_fnx(&cx, RANK, &parameters, options())
            .await
            .unwrap();
        assert_eq!(
            first,
            old_resident
                .call_fnx(&cx, RANK, &parameters, options())
                .unwrap()
        );
        let retained = old
            .prism_projection_at(
                &cx,
                old.frontier(),
                options().selection,
                options().projection,
                options().projection_limits,
                options().source_limits,
            )
            .await
            .unwrap();

        let mut change = WriteBatch::new(R);
        change.set_edge_property(EId(0), P, Some(CanonicalScalar::Int(9)));
        change.set_vertex_label(VId(3), L, false);
        assert_eq!(db.write(&commit, change).await.unwrap(), CommitSeq(3));
        let mut deletion = WriteBatch::new(R);
        deletion.delete_vertex(VId(2));
        assert_eq!(db.write(&commit, deletion).await.unwrap(), CommitSeq(4));
        let resident = db.read_session().unwrap();
        let current_pool = pool();
        let mut buffered = cold(&contexts, &vfs, current_pool.clone()).await;
        assert_eq!(buffered.partition_root(), resident.partition_root());
        assert_eq!(buffered.buffer_stats().misses, 0);

        for cut in 0..=4 {
            for direction in [
                Directedness::Directed,
                Directedness::Reversed,
                Directedness::Undirected,
            ] {
                let mut opt = options();
                opt.as_of = Some(CommitSeq(cut));
                opt.projection.directedness = direction;
                let graph = buffered
                    .prism_projection_at(
                        &cx,
                        CommitSeq(cut),
                        opt.selection,
                        opt.projection,
                        opt.projection_limits,
                        opt.source_limits,
                    )
                    .await
                    .unwrap();
                let expected = oracle(graph.binding(), opt);
                assert_eq!(
                    graph.digest(),
                    expected.digest(),
                    "cut {cut}, {direction:?}"
                );
                assert_eq!(graph.vertex_ids(), expected.vertex_ids());
                for ordinal in 0..graph.vertex_ids().len() {
                    assert_eq!(
                        graph.projected_row(ordinal),
                        expected.projected_row(ordinal)
                    );
                }
                let text = if direction == Directedness::Undirected {
                    "CALL fnx.connected_components() YIELD component AS group,vertex AS id"
                } else {
                    "CALL fnx.strongly_connected_components() YIELD component AS group,vertex AS id"
                };
                assert_eq!(
                    buffered
                        .call_fnx(&cx, text, &parameters, opt)
                        .await
                        .unwrap(),
                    resident.call_fnx(&cx, text, &parameters, opt).unwrap(),
                );
                if cut > 0 {
                    for text in [RANK, DIJKSTRA] {
                        assert_eq!(
                            buffered
                                .call_fnx(&cx, text, &parameters, opt)
                                .await
                                .unwrap(),
                            resident.call_fnx(&cx, text, &parameters, opt).unwrap(),
                            "{text}, cut {cut}, {direction:?}",
                        );
                    }
                }
            }
        }
        assert!(buffered.buffer_stats().misses > 1);
        assert!(buffered.buffer_stats().bypasses > 0);
        assert_eq!(buffered.buffer_stats().evictions, 0);
        db.compact(&commit).await.unwrap();
        drop(db);
        assert_eq!(
            old.call_fnx(&cx, RANK, &parameters, options())
                .await
                .unwrap(),
            first
        );
        assert_eq!(old.frontier(), CommitSeq(2));
        drop(old);
        drop(buffered);
        assert_eq!(old_pool.used(), 0);
        assert_eq!(current_pool.used(), 0);
        assert_eq!(
            FnxCallSpec::bind(RANK, &parameters)
                .unwrap()
                .execute(&retained, options().execution_limits, || cx.checkpoint())
                .unwrap(),
            first.analytics,
            "the explicitly budgeted CSR outlives its buffered source pool",
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn buffered_analytics_preserve_graph_law_refusals_and_all_admission_layers() {
    let ((), report) = run_async_under_lab(0x6270_7202, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let vfs = MemVfs::new().unwrap();
        let _writer = seed(&contexts, &vfs).await;
        let source_pool = pool();
        let mut view = cold(&contexts, &vfs, source_pool.clone()).await;
        let parameters = FnxParameters::new();
        let before = view.buffer_stats();
        assert!(matches!(
            view.call_fnx(&cx, "CALL fnx.unknown()", &parameters, options())
                .await,
            Err(FnxReadError::Bind(_)),
        ));
        let mut future = options();
        future.as_of = Some(CommitSeq(3));
        assert!(matches!(
            view.call_fnx(&cx, RANK, &parameters, future).await,
            Err(FnxReadError::Read(
                BufferedReadError::BeyondPublication { .. }
            )),
        ));
        assert_eq!(view.buffer_stats(), before);

        let mut opt = options();
        opt.projection.self_loops = SelfLoopPolicy::Reject;
        assert!(matches!(
            view.call_fnx(&cx, RANK, &parameters, opt).await,
            Err(FnxReadError::Projection(ProjectionError::SelfLoop(EId(
                u128::MAX
            )))),
        ));
        opt.projection.self_loops = SelfLoopPolicy::Keep;
        assert!(matches!(
            view.call_fnx(&cx, RANK, &parameters, opt).await,
            Err(FnxReadError::Weight {
                edge: EId(u128::MAX),
                reason: FnxWeightError::NotNumeric
            }),
        ));
        opt.projection.parallel_edges = ParallelEdgePolicy::CollapseUnit;
        view.call_fnx(&cx, RANK, &parameters, opt).await.unwrap();
        opt = options();
        opt.projection.parallel_edges = ParallelEdgePolicy::Reject;
        assert!(matches!(
            view.call_fnx(&cx, RANK, &parameters, opt).await,
            Err(FnxReadError::Projection(ProjectionError::ParallelEdge {
                source: VId(1),
                target: VId(2)
            })),
        ));

        opt = options();
        opt.source_limits.max_work_units = 0;
        assert!(matches!(
            view.call_fnx(&cx, RANK, &parameters, opt).await,
            Err(FnxReadError::SourceLimit {
                resource: "work units",
                requested: 1,
                limit: 0
            }),
        ));
        // Five vertex identities plus the six selected-relation edge identities
        // are admitted even at origin, where none is visible. Unrestricted
        // relation scans still admit the seventh edge. Both preserve the same
        // exact boundary rather than silently ignoring invisible candidates.
        opt = options();
        opt.as_of = Some(CommitSeq::ORIGIN);
        opt.source_limits.max_scratch_entries = 10;
        assert!(matches!(
            view.call_fnx(&cx, RANK, &parameters, opt).await,
            Err(FnxReadError::SourceLimit {
                resource: "scratch entries",
                requested: 11,
                limit: 10
            }),
        ));
        opt.source_limits.max_scratch_entries = 11;
        assert!(
            view.call_fnx(&cx, RANK, &parameters, opt)
                .await
                .unwrap()
                .analytics
                .rows
                .is_empty()
        );
        opt.selection.relation = None;
        assert!(matches!(
            view.call_fnx(&cx, RANK, &parameters, opt).await,
            Err(FnxReadError::SourceLimit {
                resource: "scratch entries",
                requested: 12,
                limit: 11
            }),
        ));
        opt = options();
        opt.source_limits.max_staging_bytes = 0;
        assert!(matches!(
            view.call_fnx(&cx, RANK, &parameters, opt).await,
            Err(FnxReadError::SourceLimit {
                resource: "staging bytes",
                ..
            }),
        ));
        for resource in [
            "vertices",
            "input edges",
            "adjacency entries",
            "workspace bytes",
        ] {
            opt = options();
            match resource {
                "vertices" => opt.projection_limits.max_vertices = 0,
                "input edges" => opt.projection_limits.max_input_edges = 0,
                "adjacency entries" => opt.projection_limits.max_adjacency_entries = 0,
                "workspace bytes" => opt.projection_limits.max_workspace_bytes = 0,
                _ => unreachable!(),
            }
            assert!(
                matches!(
                    view.call_fnx(&cx, RANK, &parameters, opt).await,
                    Err(FnxReadError::Projection(ProjectionError::LimitExceeded { resource: actual, .. })) if actual == resource,
                ),
                "{resource}"
            );
        }
        opt = options();
        opt.execution_limits.max_result_rows = 0;
        assert!(matches!(
            view.call_fnx(&cx, RANK, &parameters, opt).await,
            Err(FnxReadError::Execution(
                FnxExecutionError::LimitExceeded { .. }
            )),
        ));
        view.call_fnx(&cx, RANK, &parameters, options())
            .await
            .unwrap();
        drop(view);
        assert_eq!(
            source_pool.used(),
            0,
            "failed calls retain no source reservations"
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn selected_relation_analytics_skip_unrelated_payloads_under_a_tighter_source_budget() {
    let ((), report) = run_async_under_lab(0x6270_7204, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let vfs = MemVfs::new().unwrap();
        let mut writer =
            Database::create_with_vfs(&commit, vfs.clone(), vfs.database_dir(), keys())
                .await
                .unwrap();
        let unrelated = 48_u128;
        let mut first = WriteBatch::new(R);
        for vid in [1, 2, 3] {
            first.create_vertex(VId(vid), vec![L], vec![]);
        }
        for vid in 100..100 + unrelated {
            first.create_vertex(VId(vid), vec![LabelId(11)], vec![]);
        }
        for (eid, source, target) in [(0, 1, 2), (1, 2, 3), (2, 3, 1)] {
            first.add_edge(
                EId(eid),
                VId(source),
                VId(target),
                vec![(P, CanonicalScalar::Int(1))],
            );
        }
        writer.write(&commit, first).await.unwrap();
        let mut noise = WriteBatch::new(RelationId(8));
        for offset in 0..unrelated {
            noise.add_edge(
                EId(100 + offset),
                VId(100 + offset),
                VId(100 + (offset + 1) % unrelated),
                // Full native payloads have to be authenticated at open, but
                // excluded endpoints/relations must not expose a weight error.
                vec![(
                    P,
                    CanonicalScalar::ucs_basic_text(&"x".repeat(1024)).unwrap(),
                )],
            );
        }
        writer.write(&commit, noise).await.unwrap();
        let mut change = WriteBatch::new(R);
        change.set_edge_property(EId(0), P, Some(CanonicalScalar::Int(3)));
        writer.write(&commit, change).await.unwrap();
        let source_pool = pool();
        let mut view = cold(&contexts, &vfs, source_pool.clone()).await;
        let parameters = FnxParameters::new();
        for cut in 0..=3 {
            let mut selected = options();
            selected.as_of = Some(CommitSeq(cut));
            // Every vertex history, then only the selected relation's three
            // edge histories. Restatements consume work, not extra identities.
            selected.source_limits.max_scratch_entries = unrelated as u64 + 6;
            let before = view.buffer_stats();
            let selected_graph = view
                .prism_projection_at(
                    &cx,
                    CommitSeq(cut),
                    selected.selection,
                    selected.projection,
                    selected.projection_limits,
                    selected.source_limits,
                )
                .await
                .unwrap();
            let selected_faults = view.buffer_stats().misses - before.misses;
            let mut all = selected;
            all.selection.relation = None;
            // The full scan is the incumbent in this invocation. Its induced
            // graph is identical: all other-relation endpoints lack L.
            assert!(matches!(
                view.call_fnx(&cx, RANK, &parameters, all).await,
                Err(FnxReadError::SourceLimit {
                    resource: "scratch entries",
                    ..
                })
            ));
            all.source_limits.max_scratch_entries = 1000;
            let before = view.buffer_stats();
            let all_graph = view
                .prism_projection_at(
                    &cx,
                    CommitSeq(cut),
                    all.selection,
                    all.projection,
                    all.projection_limits,
                    all.source_limits,
                )
                .await
                .unwrap();
            let all_faults = view.buffer_stats().misses - before.misses;
            assert_eq!(selected_graph.digest(), all_graph.digest(), "cut {cut}");
            assert_eq!(selected_graph.vertex_ids(), all_graph.vertex_ids());
            for ordinal in 0..selected_graph.vertex_ids().len() {
                assert_eq!(
                    selected_graph.projected_row(ordinal),
                    all_graph.projected_row(ordinal)
                );
            }
            assert!(
                all_faults >= selected_faults + 2 * unrelated as u64,
                "every excluded block and property patch stays unfaulted: cut {cut}, selected {selected_faults}, all {all_faults}"
            );
            let bound = FnxCallSpec::bind(RANK, &parameters).unwrap();
            assert_eq!(
                bound
                    .execute(&selected_graph, selected.execution_limits, || cx
                        .checkpoint())
                    .unwrap(),
                bound
                    .execute(&all_graph, all.execution_limits, || cx.checkpoint())
                    .unwrap(),
            );
        }
        drop(view);
        drop(writer);
        assert_eq!(source_pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn unit_and_collapsed_projections_skip_sidecars_and_match_the_full_property_oracle() {
    let ((), report) = run_async_under_lab(0x6270_7204, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let vfs = MemVfs::new().unwrap();
        let mut writer =
            Database::create_with_vfs(&commit, vfs.clone(), vfs.database_dir(), keys())
                .await
                .unwrap();
        let input = [
            (EId(0), 1, 2),
            (HIGH_EDGE, 1, 2),
            (EId(2), 2, 3),
            (EId(3), 3, 1),
            (EId(u128::MAX), 2, 2),
        ];
        let payload_key = PropertyKeyId(10);
        let mut first = WriteBatch::new(R);
        for vid in [1, 2, 3, u128::MAX] {
            first.create_vertex(VId(vid), vec![L], vec![]);
        }
        for (eid, source, target) in input {
            first.add_edge(
                eid,
                VId(source),
                VId(target),
                vec![
                    (P, CanonicalScalar::Int(1)),
                    (
                        payload_key,
                        CanonicalScalar::bytes(vec![0xa7; 1024]).unwrap(),
                    ),
                ],
            );
        }
        assert_eq!(writer.write(&commit, first).await.unwrap(), CommitSeq(1));
        let mut change = WriteBatch::new(R);
        change.delete_edge(EId(2));
        change.set_edge_property(
            EId(0),
            payload_key,
            Some(CanonicalScalar::bytes(vec![0xb8; 1024]).unwrap()),
        );
        assert_eq!(writer.write(&commit, change).await.unwrap(), CommitSeq(2));
        let source_pool = pool();
        let mut view = cold(&contexts, &vfs, source_pool.clone()).await;
        let call = FnxCallSpec::bind(RANK, &FnxParameters::new()).unwrap();
        for cut in 0..=2 {
            for direction in [
                Directedness::Directed,
                Directedness::Reversed,
                Directedness::Undirected,
            ] {
                let mut opt = options();
                opt.projection.directedness = direction;
                let before = view.buffer_stats().misses;
                let complete = view
                    .prism_projection_at(
                        &cx,
                        CommitSeq(cut),
                        opt.selection,
                        opt.projection,
                        opt.projection_limits,
                        opt.source_limits,
                    )
                    .await
                    .unwrap();
                let complete_faults = view.buffer_stats().misses - before;

                let vertices = if cut == 0 {
                    vec![]
                } else {
                    vec![VId(1), VId(2), VId(3), VId(u128::MAX)]
                };
                let edges: Vec<_> = input
                    .into_iter()
                    .filter(|(eid, source, target)| {
                        cut > 0 && (cut < 2 || *eid != EId(2)) && source != target
                    })
                    .map(|(eid, source, target)| ProjectionEdge {
                        eid,
                        source: VId(source),
                        target: VId(target),
                        weight: 1.0,
                    })
                    .collect();
                let expected = SnapshotGraphView::build(
                    complete.binding(),
                    &vertices,
                    &edges,
                    opt.projection,
                    opt.projection_limits,
                )
                .unwrap();
                assert_eq!(complete.digest(), expected.digest());

                opt.selection.weight = FnxWeightSpec::Unit;
                let before = view.buffer_stats().misses;
                let topology = view
                    .prism_projection_at(
                        &cx,
                        CommitSeq(cut),
                        opt.selection,
                        opt.projection,
                        opt.projection_limits,
                        opt.source_limits,
                    )
                    .await
                    .unwrap();
                let topology_faults = view.buffer_stats().misses - before;
                assert_eq!(
                    topology.digest(),
                    complete.digest(),
                    "cut {cut}, {direction:?}"
                );
                assert_eq!(topology.vertex_ids(), complete.vertex_ids());
                for ordinal in 0..topology.vertex_ids().len() {
                    assert_eq!(
                        topology.projected_row(ordinal),
                        complete.projected_row(ordinal)
                    );
                }
                assert_eq!(
                    call.execute(&topology, opt.execution_limits, || cx.checkpoint())
                        .unwrap(),
                    call.execute(&complete, opt.execution_limits, || cx.checkpoint())
                        .unwrap()
                );
                if cut > 0 {
                    assert!(
                        topology_faults < complete_faults,
                        "unit projection skips sidecars against the live complete source: cut {cut}, {direction:?}, topology {topology_faults}, complete {complete_faults}"
                    );
                }

                // CollapseUnit deliberately ignores even a missing required
                // weight. The independent input keeps parallel EIds; the
                // source selection already excludes the dropped self-loop.
                opt.selection.weight = FnxWeightSpec::Property {
                    key: PropertyKeyId(999),
                    missing: MissingWeightPolicy::Reject,
                };
                opt.projection.parallel_edges = ParallelEdgePolicy::CollapseUnit;
                let before = view.buffer_stats().misses;
                let collapsed = view
                    .prism_projection_at(
                        &cx,
                        CommitSeq(cut),
                        opt.selection,
                        opt.projection,
                        opt.projection_limits,
                        opt.source_limits,
                    )
                    .await
                    .unwrap();
                let collapsed_faults = view.buffer_stats().misses - before;
                let expected = SnapshotGraphView::build(
                    collapsed.binding(),
                    &vertices,
                    &edges,
                    opt.projection,
                    opt.projection_limits,
                )
                .unwrap();
                assert_eq!(collapsed.digest(), expected.digest());
                if cut > 0 {
                    assert!(collapsed_faults < complete_faults);
                    opt.projection.parallel_edges = ParallelEdgePolicy::Sum;
                    assert!(matches!(
                        view.prism_projection_at(
                            &cx,
                            CommitSeq(cut),
                            opt.selection,
                            opt.projection,
                            opt.projection_limits,
                            opt.source_limits,
                        )
                        .await,
                        Err(FnxReadError::Weight {
                            reason: FnxWeightError::Missing,
                            ..
                        })
                    ));
                }
            }
        }
        drop(view);
        drop(writer);
        assert_eq!(source_pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn every_buffered_analytics_checkpoint_cancels_without_publishing_a_partial_projection() {
    let ((), report) = run_async_under_lab(0x6270_7203, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let vfs = MemVfs::new().unwrap();
        let commit = contexts.commit();
        let mut writer =
            Database::create_with_vfs(&commit, vfs.clone(), vfs.database_dir(), keys())
                .await
                .unwrap();
        let mut batch = WriteBatch::new(R);
        for vid in [1, 2] {
            batch.create_vertex(VId(vid), vec![L], vec![]);
        }
        batch.add_edge(EId(0), VId(1), VId(2), vec![(P, CanonicalScalar::Int(2))]);
        writer.write(&commit, batch).await.unwrap();
        let call = FnxCallSpec::bind(DIJKSTRA, &FnxParameters::new()).unwrap();
        for weight in [options().selection.weight, FnxWeightSpec::Unit] {
            let mut opt = options();
            opt.selection.weight = weight;
            let observe = Arc::new(SimulationCheckpointProbe::new(None));
            let mut view = cold(&contexts, &vfs, pool()).await;
            let complete = view
                .execute_fnx(&cx.with_checkpoint_probe(Arc::clone(&observe)), &call, opt)
                .await
                .unwrap();
            let total = observe.calls();
            assert!(total > 20);
            drop(view);
            for stop in 1..=total {
                let source_pool = pool();
                // Start with the same empty extent cache on every cut. A warm
                // cache has a different legal physical checkpoint transcript.
                let mut view = cold(&contexts, &vfs, source_pool.clone()).await;
                let probe = Arc::new(SimulationCheckpointProbe::new(Some(stop)));
                let controlled = cx.with_checkpoint_probe(Arc::clone(&probe));
                let failure = view.execute_fnx(&controlled, &call, opt).await.unwrap_err();
                assert!(
                    failure.split_cancel::<core::convert::Infallible>().is_err(),
                    "cut {stop}, {weight:?}"
                );
                assert_eq!(
                    probe.calls(),
                    stop,
                    "no source/kernel work after cancellation"
                );
                assert_eq!(view.execute_fnx(&cx, &call, opt).await.unwrap(), complete);
                drop(view);
                assert_eq!(source_pool.used(), 0, "cut {stop}, {weight:?}");
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn decoded_reuse_skips_source_faults_across_calls_and_keeps_its_exact_generation() {
    let ((), report) = run_async_under_lab(0x6270_7210, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let vfs = MemVfs::new().unwrap();
        let mut writer = seed(&contexts, &vfs).await;
        let source = pool();
        let mut view = cold(&contexts, &vfs, source.clone()).await;
        let retained = view.projection_read_cache_pool().clone();
        let first = project_buffered(&mut view, &cx, options()).await.unwrap();
        let expected = oracle(first.binding(), options());
        assert_eq!(first.digest(), expected.digest());
        assert_eq!(retained.used(), first.charged_resident_bytes());
        assert_eq!(view.projection_read_cache_usage().entries, 1);
        assert_eq!(view.sealed_read_cache_usage().entries, 0);
        let faults = view.buffer_stats();
        assert!(
            first.shares_cache_with(&project_buffered(&mut view, &cx, options()).await.unwrap())
        );
        let params = FnxParameters::new();
        for text in [RANK, DIJKSTRA] {
            let call = FnxCallSpec::bind(text, &params).unwrap();
            assert_eq!(
                view.call_fnx(&cx, text, &params, options())
                    .await
                    .unwrap()
                    .analytics,
                call.execute(&expected, options().execution_limits, || cx.checkpoint())
                    .unwrap(),
            );
            assert_eq!(
                view.buffer_stats(),
                faults,
                "cached CALL must not rescan source"
            );
        }
        // Independently opened owners do not share a graph even at equal roots.
        let mut separate = cold(&contexts, &vfs, pool()).await;
        let other = project_buffered(&mut separate, &cx, options())
            .await
            .unwrap();
        assert_eq!(first.digest(), other.digest());
        assert!(!first.shares_cache_with(&other));
        let mut change = WriteBatch::new(R);
        change.set_edge_property(EId(0), P, Some(CanonicalScalar::Int(9)));
        writer.write(&contexts.commit(), change).await.unwrap();
        assert!(
            first.shares_cache_with(&project_buffered(&mut view, &cx, options()).await.unwrap())
        );
        assert_eq!(view.buffer_stats(), faults);
        let mut current = cold(&contexts, &vfs, pool()).await;
        let newer = project_buffered(&mut current, &cx, options())
            .await
            .unwrap();
        assert_ne!(first.binding().root, newer.binding().root);
        assert_ne!(first.digest(), newer.digest());
        assert!(!first.shares_cache_with(&newer));
        drop(view);
        assert_eq!(source.used(), 0);
        assert_eq!(retained.used(), 0);
        // Caller-owned graphs remain usable after their cache/source owner drops.
        let call = FnxCallSpec::bind(DIJKSTRA, &params).unwrap();
        assert_eq!(
            call.execute(&first, options().execution_limits, || cx.checkpoint())
                .unwrap(),
            call.execute(&expected, options().execution_limits, || cx.checkpoint())
                .unwrap(),
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn decoded_retention_obeys_exact_ancestor_caps_and_refunds_on_replacement_and_drop() {
    let ((), report) = run_async_under_lab(0x6270_7211, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let vfs = MemVfs::new().unwrap();
        let _writer = seed(&contexts, &vfs).await;
        let mut view = cold(&contexts, &vfs, pool()).await;
        let initial = project_buffered(&mut view, &cx, options()).await.unwrap();
        let bytes = initial.charged_resident_bytes();
        assert!(bytes > 1);
        let parent = MemoryPool::new(bytes, 0).unwrap();
        let child = parent.child(bytes * 2, 0).unwrap();
        view.set_projection_read_cache_pool(&cx, child.clone())
            .unwrap();
        let first = project_buffered(&mut view, &cx, options()).await.unwrap();
        assert_eq!(view.projection_read_cache_usage().max_bytes, bytes);
        assert_eq!(view.projection_read_cache_usage().retained_bytes, bytes);
        assert_eq!((parent.used(), child.used()), (bytes, bytes));
        assert!(
            first.shares_cache_with(&project_buffered(&mut view, &cx, options()).await.unwrap())
        );
        assert_eq!(parent.used(), bytes, "a hit does not charge another owner");
        let mut second = cold(&contexts, &vfs, pool()).await;
        second
            .set_projection_read_cache_pool(&cx, parent.child(bytes * 2, 0).unwrap())
            .unwrap();
        let declined = project_buffered(&mut second, &cx, options()).await.unwrap();
        assert_eq!(declined.digest(), first.digest());
        assert_eq!(second.projection_read_cache_usage().entries, 0);
        assert!(
            !declined
                .shares_cache_with(&project_buffered(&mut second, &cx, options()).await.unwrap())
        );
        assert_eq!(parent.used(), bytes);
        let probe = Arc::new(SimulationCheckpointProbe::new(Some(1)));
        assert!(
            view.set_projection_read_cache_pool(
                &cx.with_checkpoint_probe(Arc::clone(&probe)),
                MemoryPool::new(0, 0).unwrap(),
            )
            .is_err()
        );
        assert_eq!(probe.calls(), 1);
        assert_eq!(
            (view.projection_read_cache_usage().entries, parent.used()),
            (1, bytes)
        );
        for limit in [bytes - 1, 0] {
            let replacement = MemoryPool::new(limit, 0).unwrap();
            view.set_projection_read_cache_pool(&cx, replacement.clone())
                .unwrap();
            assert_eq!((parent.used(), child.used()), (0, 0));
            let a = project_buffered(&mut view, &cx, options()).await.unwrap();
            let faults = view.buffer_stats();
            let b = project_buffered(&mut view, &cx, options()).await.unwrap();
            assert_eq!(a.digest(), initial.digest());
            assert_eq!(a.digest(), b.digest());
            assert!(!a.shares_cache_with(&b));
            assert_ne!(
                view.buffer_stats(),
                faults,
                "declined retention still scans normally"
            );
            assert_eq!(
                (
                    view.projection_read_cache_usage().entries,
                    replacement.used()
                ),
                (0, 0)
            );
        }
        let exact = MemoryPool::new(bytes, 0).unwrap();
        view.set_projection_read_cache_pool(&cx, exact.clone())
            .unwrap();
        let kept = project_buffered(&mut view, &cx, options()).await.unwrap();
        assert_eq!(exact.used(), bytes);
        drop(view);
        assert_eq!(exact.used(), 0);
        assert_eq!(kept.digest(), initial.digest());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn decoded_reuse_keys_every_construction_dimension_and_preserves_kernel_refusals() {
    let ((), report) = run_async_under_lab(0x6270_7212, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let vfs = MemVfs::new().unwrap();
        let _writer = seed(&contexts, &vfs).await;
        let mut view = cold(&contexts, &vfs, pool()).await;
        let retention = pool();
        // Even a harmless relaxation is a distinct construction request.
        for dimension in 0..14 {
            view.set_projection_read_cache_pool(&cx, retention.clone())
                .unwrap();
            let first = project_buffered(&mut view, &cx, options()).await.unwrap();
            let mut opt = options();
            match dimension {
                0 => opt.source_limits.max_work_units += 1,
                1 => opt.source_limits.max_scratch_entries += 1,
                2 => opt.source_limits.max_staging_bytes += 1,
                3 => opt.projection_limits.max_vertices += 1,
                4 => opt.projection_limits.max_input_edges += 1,
                5 => opt.projection_limits.max_adjacency_entries += 1,
                6 => opt.projection_limits.max_workspace_bytes += 1,
                7 => opt.as_of = Some(CommitSeq(1)),
                8 => opt.selection.vertex_label = Some(LabelId(11)),
                9 => opt.selection.relation = Some(RelationId(9)),
                10 => opt.selection.weight = FnxWeightSpec::Unit,
                11 => {
                    opt.selection.weight = FnxWeightSpec::Property {
                        key: P,
                        missing: MissingWeightPolicy::Zero,
                    }
                }
                12 => opt.projection.directedness = Directedness::Reversed,
                13 => opt.projection.parallel_edges = ParallelEdgePolicy::Minimum,
                _ => unreachable!(),
            }
            let different = project_buffered(&mut view, &cx, opt).await.unwrap();
            assert!(
                !first.shares_cache_with(&different),
                "dimension {dimension}"
            );
            assert_eq!(view.projection_read_cache_usage().entries, 2);
            assert!(
                different.shares_cache_with(&project_buffered(&mut view, &cx, opt).await.unwrap())
            );
            assert!(
                first
                    .shares_cache_with(&project_buffered(&mut view, &cx, options()).await.unwrap())
            );
        }
        view.set_projection_read_cache_pool(&cx, retention.clone())
            .unwrap();
        let first = project_buffered(&mut view, &cx, options()).await.unwrap();
        for dimension in 0..10 {
            let mut opt = options();
            match dimension {
                0 => opt.source_limits.max_work_units = 0,
                1 => opt.source_limits.max_scratch_entries = 0,
                2 => opt.source_limits.max_staging_bytes = 0,
                3 => opt.projection_limits.max_vertices = 0,
                4 => opt.projection_limits.max_input_edges = 0,
                5 => opt.projection_limits.max_adjacency_entries = 0,
                6 => opt.projection_limits.max_workspace_bytes = 0,
                7 => {
                    opt.selection.weight = FnxWeightSpec::Property {
                        key: PropertyKeyId(999),
                        missing: MissingWeightPolicy::Reject,
                    }
                }
                8 => opt.projection.self_loops = SelfLoopPolicy::Reject,
                9 => opt.as_of = Some(CommitSeq(view.frontier().0 + 1)),
                _ => unreachable!(),
            }
            assert!(
                project_buffered(&mut view, &cx, opt).await.is_err(),
                "dimension {dimension}"
            );
            assert_eq!(view.projection_read_cache_usage().entries, 1);
            assert!(
                first
                    .shares_cache_with(&project_buffered(&mut view, &cx, options()).await.unwrap())
            );
        }
        let faults = view.buffer_stats();
        for dimension in 0..3 {
            let mut opt = options();
            match dimension {
                0 => opt.execution_limits.max_iterations = 0,
                1 => opt.execution_limits.max_result_rows = 0,
                2 => opt.execution_limits.max_estimated_work = 0,
                _ => unreachable!(),
            }
            assert!(matches!(
                view.call_fnx(&cx, RANK, &FnxParameters::new(), opt).await,
                Err(FnxReadError::Execution(
                    FnxExecutionError::LimitExceeded { .. }
                ))
            ));
            assert_eq!(view.buffer_stats(), faults);
        }
        // Full retention declines the ninth request without displacing any live entry.
        view.set_projection_read_cache_pool(&cx, retention).unwrap();
        let mut kept = Vec::new();
        for extra in 0..8 {
            let mut opt = options();
            opt.source_limits.max_work_units += extra;
            kept.push(project_buffered(&mut view, &cx, opt).await.unwrap());
        }
        assert_eq!(view.projection_read_cache_usage().entries, 8);
        let mut ninth = options();
        ninth.source_limits.max_work_units += 8;
        let a = project_buffered(&mut view, &cx, ninth).await.unwrap();
        let b = project_buffered(&mut view, &cx, ninth).await.unwrap();
        assert_eq!(a.digest(), b.digest());
        assert!(!a.shares_cache_with(&b));
        for (extra, graph) in kept.iter().enumerate() {
            let mut opt = options();
            opt.source_limits.max_work_units += extra as u64;
            assert!(graph.shares_cache_with(&project_buffered(&mut view, &cx, opt).await.unwrap()));
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn decoded_cache_hits_and_last_build_checkpoints_keep_live_cancellation() {
    let ((), report) = run_async_under_lab(0x6270_7213, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let vfs = MemVfs::new().unwrap();
        let _writer = seed(&contexts, &vfs).await;
        let mut view = cold(&contexts, &vfs, pool()).await;
        let observe = Arc::new(SimulationCheckpointProbe::new(None));
        let first = project_buffered(
            &mut view,
            &cx.with_checkpoint_probe(Arc::clone(&observe)),
            options(),
        )
        .await
        .unwrap();
        let cold_checks = observe.calls();
        let observe = Arc::new(SimulationCheckpointProbe::new(None));
        let hit = project_buffered(
            &mut view,
            &cx.with_checkpoint_probe(Arc::clone(&observe)),
            options(),
        )
        .await
        .unwrap();
        let warm_checks = observe.calls();
        assert!(first.shares_cache_with(&hit));
        assert!(cold_checks > warm_checks && warm_checks > 0);
        let faults = view.buffer_stats();
        for stop in 1..=warm_checks {
            let probe = Arc::new(SimulationCheckpointProbe::new(Some(stop)));
            assert!(matches!(
                project_buffered(
                    &mut view,
                    &cx.with_checkpoint_probe(Arc::clone(&probe)),
                    options()
                )
                .await,
                Err(FnxReadError::Cancelled(_))
            ));
            assert_eq!(probe.calls(), stop);
            assert_eq!(view.buffer_stats(), faults);
            assert_eq!(view.projection_read_cache_usage().entries, 1);
        }
        for stop in [1, 2, cold_checks / 2, cold_checks - 1, cold_checks] {
            let source = pool();
            let mut fresh = cold(&contexts, &vfs, source.clone()).await;
            let retention = fresh.projection_read_cache_pool().clone();
            let probe = Arc::new(SimulationCheckpointProbe::new(Some(stop)));
            assert!(matches!(
                project_buffered(
                    &mut fresh,
                    &cx.with_checkpoint_probe(Arc::clone(&probe)),
                    options()
                )
                .await,
                Err(FnxReadError::Cancelled(_))
            ));
            assert_eq!(probe.calls(), stop);
            assert_eq!(
                (
                    fresh.projection_read_cache_usage().entries,
                    retention.used()
                ),
                (0, 0)
            );
            assert_eq!(
                project_buffered(&mut fresh, &cx, options())
                    .await
                    .unwrap()
                    .digest(),
                first.digest()
            );
            assert_eq!(fresh.projection_read_cache_usage().entries, 1);
            drop(fresh);
            assert_eq!((source.used(), retention.used()), (0, 0));
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
