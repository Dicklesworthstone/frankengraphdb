//! Selected edge weights retain their exact history without materializing
//! unrelated sidecar values. The codec/cursor laws measure omitted payloads;
//! these public laws compare complete projections and analytics independently.

use super::*;
use fgdb_types::CanonicalF64;

const Q: PropertyKeyId = PropertyKeyId(8);
const NEW_EDGE: EId = EId((2_u128 << 100) | 7);

async fn selected_weight_cold(
    contexts: &PurposeContexts,
    vfs: &MemVfs,
    source_pool: MemoryPool,
) -> BufferedReadView<MemVfs> {
    let mut read_limits = limits();
    // Multiple historical versions of Q may share one sidecar. Keep the
    // single-frame cache, with enough extent allowance for those versions;
    // this law makes no claim of a smaller source-admission requirement.
    read_limits.buffer.max_extent_bytes = 64 * 1024;
    Database::open_buffered_read_view_with_vfs(
        &contexts.commit(),
        vfs.clone(),
        vfs.database_dir(),
        keys(),
        source_pool,
        read_limits,
    )
    .await
    .unwrap()
}

fn expected_selected_weight(
    binding: SnapshotBinding,
    options: FnxReadOptions,
) -> SnapshotGraphView {
    let cut = binding.as_of.0;
    assert!((3..=5).contains(&cut));
    let missing = match options.selection.weight {
        FnxWeightSpec::Property {
            key: P,
            missing: MissingWeightPolicy::Unit,
        } => 1.0,
        FnxWeightSpec::Property {
            key: P,
            missing: MissingWeightPolicy::Zero,
        } => 0.0,
        FnxWeightSpec::Property {
            key: P,
            missing: MissingWeightPolicy::Reject,
        } if cut != 4 => 0.0,
        _ => panic!("the refused missing-weight case has no projection"),
    };
    // These are the literal visible selected inputs, independent of either
    // storage frontend. Parallel identities remain separate until projection.
    let weights = match cut {
        3 => [2.0, 5.0, 1.0],
        4 => [missing; 3],
        5 => [2.5, 6.0, 3.0],
        _ => unreachable!(),
    };
    let mut edges = vec![
        ProjectionEdge {
            eid: EId(0),
            source: VId(1),
            target: VId(2),
            weight: weights[0],
        },
        ProjectionEdge {
            eid: HIGH_EDGE,
            source: VId(1),
            target: VId(2),
            weight: weights[1],
        },
        ProjectionEdge {
            eid: EId(2),
            source: VId(2),
            target: VId(3),
            weight: weights[2],
        },
    ];
    if cut == 3 {
        edges.push(ProjectionEdge {
            eid: EId(3),
            source: VId(3),
            target: VId(1),
            weight: 4.0,
        });
    }
    if cut == 5 {
        edges.push(ProjectionEdge {
            eid: NEW_EDGE,
            source: VId(u128::MAX),
            target: VId(1),
            weight: 7.0,
        });
    }
    SnapshotGraphView::build(
        binding,
        &[VId(1), VId(2), VId(3), VId(u128::MAX)],
        &edges,
        options.projection,
        options.projection_limits,
    )
    .unwrap()
}

#[test]
fn selected_weights_preserve_missing_null_propertyless_history_and_pinned_results() {
    let ((), report) = run_async_under_lab(0x6270_7220, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let vfs = MemVfs::new().unwrap();
        let mut writer = seed(&contexts, &vfs).await;
        // Q sorts before P. Its large nonnumeric payload must not become a
        // weight or be retained as part of the selected scalar source row.
        let mut payload = WriteBatch::new(R);
        payload.set_edge_property(
            EId(0),
            Q,
            Some(CanonicalScalar::bytes(vec![0xa7; 8400]).unwrap()),
        );
        payload.set_edge_property(
            HIGH_EDGE,
            Q,
            Some(CanonicalScalar::Bool(false)),
        );
        assert_eq!(
            writer.write(&commit, payload).await.unwrap(),
            CommitSeq(3)
        );
        let old_resident = writer.read_session().unwrap();
        let old_source = pool();
        let mut old = selected_weight_cold(&contexts, &vfs, old_source.clone()).await;
        let old_retention = old.projection_read_cache_pool().clone();
        let retained = project_buffered(&mut old, &cx, options()).await.unwrap();
        let parameters = FnxParameters::new();
        let old_answer = old
            .call_fnx(&cx, DIJKSTRA, &parameters, options())
            .await
            .unwrap();
        assert_eq!(
            old_answer,
            old_resident
                .call_fnx(&cx, DIJKSTRA, &parameters, options())
                .unwrap(),
        );
        let old_faults = old.buffer_stats();

        let mut missing = WriteBatch::new(R);
        missing.set_edge_property(EId(0), P, None);
        missing.set_edge_property(HIGH_EDGE, P, None);
        missing.set_edge_property(HIGH_EDGE, Q, None);
        missing.set_edge_property(EId(2), P, Some(CanonicalScalar::Null));
        missing.delete_edge(EId(3));
        assert_eq!(
            writer.write(&commit, missing).await.unwrap(),
            CommitSeq(4)
        );
        // At cut 4 the three inputs have different representations: Q-only,
        // a completely propertyless winner, and an explicit NULL weight.
        let mut restored = WriteBatch::new(R);
        restored.set_edge_property(
            EId(0),
            P,
            Some(CanonicalScalar::Float(CanonicalF64::new(2.5))),
        );
        restored.set_edge_property(HIGH_EDGE, P, Some(CanonicalScalar::Int(6)));
        restored.set_edge_property(EId(2), P, Some(CanonicalScalar::Int(3)));
        restored.add_edge(
            NEW_EDGE,
            VId(u128::MAX),
            VId(1),
            vec![(P, CanonicalScalar::Int(7))],
        );
        assert_eq!(
            writer.write(&commit, restored).await.unwrap(),
            CommitSeq(5)
        );
        let resident = writer.read_session().unwrap();
        let source = pool();
        let retention = pool();
        let mut view = selected_weight_cold(&contexts, &vfs, source.clone()).await;
        let call = FnxCallSpec::bind(DIJKSTRA, &parameters).unwrap();

        for direction in [
            Directedness::Directed,
            Directedness::Reversed,
            Directedness::Undirected,
        ] {
            for cut in [3, 4, 5, 3] {
                for missing in [
                    MissingWeightPolicy::Reject,
                    MissingWeightPolicy::Unit,
                    MissingWeightPolicy::Zero,
                ] {
                    // A fresh entry for each shape makes the repeat below a
                    // concrete source-free hit rather than a capacity fallback.
                    view.set_projection_read_cache_pool(&cx, retention.clone())
                        .unwrap();
                    let mut opt = options();
                    opt.as_of = Some(CommitSeq(cut));
                    opt.projection.directedness = direction;
                    opt.selection.weight = FnxWeightSpec::Property { key: P, missing };
                    let graph = project_buffered(&mut view, &cx, opt).await;
                    if cut == 4 && missing == MissingWeightPolicy::Reject {
                        assert!(matches!(
                            graph,
                            Err(FnxReadError::Weight {
                                edge: EId(0),
                                reason: FnxWeightError::Missing,
                            })
                        ));
                        assert!(matches!(
                            resident.call_fnx(&cx, DIJKSTRA, &parameters, opt),
                            Err(FnxReadError::Weight {
                                edge: EId(0),
                                reason: FnxWeightError::Missing,
                            })
                        ));
                        assert_eq!(view.projection_read_cache_usage().entries, 0);
                        continue;
                    }
                    let graph = graph.unwrap();
                    let expected = expected_selected_weight(graph.binding(), opt);
                    assert_eq!(
                        graph.digest(),
                        expected.digest(),
                        "cut {cut}, {direction:?}, {missing:?}"
                    );
                    assert_eq!(graph.vertex_ids(), expected.vertex_ids());
                    for ordinal in 0..graph.vertex_ids().len() {
                        assert_eq!(
                            graph.projected_row(ordinal),
                            expected.projected_row(ordinal),
                        );
                    }
                    let faults = view.buffer_stats();
                    assert_eq!(view.projection_read_cache_usage().entries, 1);
                    assert!(
                        graph.shares_cache_with(
                            &project_buffered(&mut view, &cx, opt).await.unwrap()
                        )
                    );
                    let answer = view
                        .call_fnx(&cx, DIJKSTRA, &parameters, opt)
                        .await
                        .unwrap();
                    assert_eq!(
                        answer,
                        resident.call_fnx(&cx, DIJKSTRA, &parameters, opt).unwrap(),
                    );
                    assert_eq!(
                        answer.analytics,
                        call.execute(&expected, opt.execution_limits, || cx.checkpoint())
                            .unwrap(),
                    );
                    assert_eq!(
                        view.buffer_stats(),
                        faults,
                        "warm selected weights must not refault a sidecar",
                    );
                }
            }
        }

        // A populated projection cache cannot evade a different construction
        // allowance, even when the requested graph would otherwise be equal.
        let mut denied = options();
        denied.source_limits.max_work_units = 0;
        assert!(matches!(
            project_buffered(&mut old, &cx, denied).await,
            Err(FnxReadError::SourceLimit {
                resource: "work units",
                requested: 1,
                limit: 0,
            })
        ));
        assert_eq!(old.projection_read_cache_usage().entries, 1);
        assert_eq!(
            old.call_fnx(&cx, DIJKSTRA, &parameters, options())
                .await
                .unwrap(),
            old_answer,
        );
        assert!(retained.shares_cache_with(
            &project_buffered(&mut old, &cx, options()).await.unwrap()
        ));
        assert_eq!(old.frontier(), CommitSeq(3));
        assert_eq!(old.buffer_stats(), old_faults);
        drop(writer);
        drop(view);
        drop(old);
        assert_eq!(source.used(), 0);
        assert_eq!(old_source.used(), 0);
        assert_eq!(retention.used(), 0);
        assert_eq!(old_retention.used(), 0);
        assert_eq!(
            call.execute(&retained, options().execution_limits, || cx.checkpoint())
                .unwrap(),
            old_answer.analytics,
            "the completed selected-weight graph outlives both source owners",
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
