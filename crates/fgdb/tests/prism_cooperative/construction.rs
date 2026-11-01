//! Public construction observers separate logical work from scheduler fences.
//! Real authenticated history exercises raw retries and incoming-index builds.

use super::*;
use fgdb_prism::{SealedBuildEvent, SealedProjectionMask, SealedProjectionSpec};
use fgdb_strata::store::BlockStore;
use fgdb_strata::tiered::sealed::SealedPartition;
use std::rc::Rc;

fn assert_send<F: Future + Send>(future: F) -> F {
    future
}

struct Fixture {
    image: SealedPartition,
    vertices: [VId; 4],
    original: CommitSeq,
    latest: CommitSeq,
}

async fn fixture(commit: &CommitCx, cx: &QueryCx, hidden: usize, updates: usize) -> Fixture {
    let vfs = MemVfs::new().unwrap();
    let namespace = DatabaseSecurityNamespaceId([0x72; 32]);
    let keys = DatabaseKeys::new([0x71; 32], namespace, [0x73; 32]);
    let mut db = Database::create_with_vfs(commit, vfs.clone(), vfs.database_dir(), keys)
        .await
        .unwrap();
    let vertices = [VId(0), VId(1), VId(2), VId(u128::MAX)];
    let mut batch = WriteBatch::new(RelationId(1));
    for vertex in vertices {
        batch.create_vertex(vertex, vec![LabelId(1)], vec![]);
    }
    batch.create_vertex(VId(99), vec![LabelId(2)], vec![]);
    for (id, source, target, weight) in [
        (1, 0, 1, 2),
        (2, 0, 1, 3),
        (3, 1, 0, 4),
        (4, 1, 1, 5),
        (5, 1, 2, 6),
        (6, 2, 3, 7),
    ] {
        batch.add_edge(
            EId(id),
            vertices[source],
            vertices[target],
            vec![(PropertyKeyId(1), CanonicalScalar::Int(weight))],
        );
    }
    // Excluded endpoints are still raw input and incoming-index work; their
    // malformed weight must never be inspected by the selected projection.
    for index in 0..hidden {
        batch.add_edge(
            EId(100 + index as u128),
            VId(99),
            vertices[0],
            vec![(PropertyKeyId(1), CanonicalScalar::Bool(true))],
        );
    }
    let original = db.write(commit, batch).await.unwrap();
    for update in 0..updates {
        let mut batch = WriteBatch::new(RelationId(1));
        batch.set_edge_property(
            EId(1),
            PropertyKeyId(1),
            Some(CanonicalScalar::Int(10 + update as i64)),
        );
        db.write(commit, batch).await.unwrap();
    }
    let mut batch = WriteBatch::new(RelationId(1));
    batch.delete_edge(EId(6));
    batch.add_edge(
        EId(7),
        vertices[3],
        vertices[2],
        vec![(PropertyKeyId(1), CanonicalScalar::Int(-2))],
    );
    let latest = db.write(commit, batch).await.unwrap();
    let store = BlockStore::open_existing_for_read_with_vfs(
        commit,
        vfs.clone(),
        vfs.database_dir(),
        [0x71; 32],
        namespace,
    )
    .await
    .unwrap();
    let image = store
        .seal_relation(
            cx,
            db.read_session().unwrap().partition_root(),
            original,
            RelationId(1),
            SealedLimits::default(),
        )
        .await
        .unwrap();
    Fixture {
        image,
        vertices,
        original,
        latest,
    }
}

fn config(direction: Directedness, at: CommitSeq) -> (SealedProjectionSpec, ProjectionLimits) {
    let mut options = options(direction);
    options.selection.weight = FnxWeightSpec::Property {
        key: PropertyKeyId(1),
        missing: MissingWeightPolicy::Unit,
    };
    (
        SealedProjectionSpec {
            as_of: at,
            selection: options.selection,
            projection: options.projection,
        },
        options.projection_limits,
    )
}

fn assert_same_projection(cx: &QueryCx, actual: &SealedGraphView, expected: &SealedGraphView) {
    assert_eq!(actual.binding(), expected.binding());
    assert_eq!(actual.spec(), expected.spec());
    assert_eq!(actual.selection(), expected.selection());
    assert_eq!(actual.digest(), expected.digest());
    assert_eq!(actual.vertex_ids(), expected.vertex_ids());
    assert_eq!(actual.input_edge_count(), expected.input_edge_count());
    assert_eq!(actual.edge_count(), expected.edge_count());
    assert_eq!(
        actual.adjacency_entry_count(),
        expected.adjacency_entry_count()
    );
    assert_eq!(
        actual.incoming_index_stats(),
        expected.incoming_index_stats()
    );
    assert_eq!(
        actual.charged_workspace_bytes(),
        expected.charged_workspace_bytes()
    );
    assert_eq!(
        actual.charged_resident_bytes(),
        expected.charged_resident_bytes()
    );
    let neighbors = |graph: &SealedGraphView, source| {
        let mut row = graph.neighbor_cursor(cx, source).unwrap();
        let mut values = Vec::new();
        while let Some((vertex, weight)) = row.next(cx).unwrap() {
            values.push((graph.vertex_id(vertex).unwrap(), weight.to_bits()));
        }
        values
    };
    for source in 0..expected.node_count() {
        assert_eq!(actual.degree(source), expected.degree(source));
        assert_eq!(neighbors(actual, source), neighbors(expected, source));
    }
}

#[cfg(test)]
fn refusal(error: SealedProjectionError, stop: usize) {
    let SealedProjectionError::Guard(error) = error else {
        panic!("typed construction refusal was lost: {error}")
    };
    assert_eq!(
        error.downcast_ref::<LiveRefusal>(),
        Some(&LiveRefusal(stop))
    );
}

#[test]
fn construction_work_is_quantum_invariant_for_history_masks_loops_and_incoming_rows() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        let fixture = fixture(&contexts.commit(), &cx, 300, 20).await;
        let original_stats = fixture.image.stats();
        for at in [fixture.original, fixture.latest] {
            for direction in [
                Directedness::Directed,
                Directedness::Reversed,
                Directedness::Undirected,
            ] {
                for mask in [
                    SealedProjectionMask::UNMASKED,
                    SealedProjectionMask {
                        relation_visible: true,
                        weight_property_visible: false,
                    },
                    SealedProjectionMask {
                        relation_visible: false,
                        weight_property_visible: false,
                    },
                ] {
                    let (config, limits) = config(direction, at);
                    let mut synchronous_work = 0;
                    let expected = SealedGraphView::build_with_checkpoint(
                        &cx,
                        &fixture.image,
                        &fixture.vertices,
                        config,
                        mask,
                        limits,
                        || {
                            synchronous_work += 1;
                            Ok(())
                        },
                    )
                    .unwrap();
                    for q in [1, 2, 7, 64, usize::MAX] {
                        let mut work = 0;
                        let mut scheduling = 0;
                        let mut yields = 0;
                        let awaiting_resume = Cell::new(false);
                        let actual = SealedGraphView::build_cooperative_with_events(
                            &cx,
                            &fixture.image,
                            &fixture.vertices,
                            config,
                            mask,
                            limits,
                            quantum(q),
                            || {
                                assert!(!awaiting_resume.replace(true));
                                yields += 1;
                                yield_now()
                            },
                            |event| {
                                if awaiting_resume.replace(false) {
                                    assert_eq!(event, SealedBuildEvent::Scheduling);
                                }
                                match event {
                                    SealedBuildEvent::Work => work += 1,
                                    SealedBuildEvent::Scheduling => scheduling += 1,
                                }
                                Ok(())
                            },
                        )
                        .await
                        .unwrap();
                        assert_eq!(
                            work, synchronous_work,
                            "direction={direction:?} cut={at:?} mask={mask:?} q={q}"
                        );
                        assert!(
                            scheduling >= yields,
                            "every resumed yield has a scheduling check; incoming builders may use Work for the preceding live check"
                        );
                        assert!(!awaiting_resume.get());
                        if q == 1 {
                            assert!(yields > 0);
                        }
                        assert_same_projection(&cx, &actual, &expected);
                    }
                }
            }
        }
        assert_eq!(fixture.image.stats(), original_stats);
        assert_eq!(contexts.outstanding_obligations(), 0);
    });
}

#[test]
fn every_work_refusal_keeps_the_synchronous_prefix_and_typed_cause_for_every_quantum() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        let fixture = fixture(&contexts.commit(), &cx, 2, 1).await;
        let original_stats = fixture.image.stats();
        for direction in [
            Directedness::Directed,
            Directedness::Reversed,
            Directedness::Undirected,
        ] {
            let (config, limits) = config(direction, fixture.original);
            let mut total = 0;
            let expected = SealedGraphView::build_with_checkpoint(
                &cx,
                &fixture.image,
                &fixture.vertices,
                config,
                SealedProjectionMask::UNMASKED,
                limits,
                || {
                    total += 1;
                    Ok(())
                },
            )
            .unwrap();
            for stop in 1..=total {
                let mut synchronous = 0;
                let error = SealedGraphView::build_with_checkpoint(
                    &cx,
                    &fixture.image,
                    &fixture.vertices,
                    config,
                    SealedProjectionMask::UNMASKED,
                    limits,
                    || {
                        synchronous += 1;
                        if synchronous == stop {
                            Err(SealedProjectionError::Guard(Box::new(LiveRefusal(stop))))
                        } else {
                            Ok(())
                        }
                    },
                )
                .unwrap_err();
                refusal(error, stop);
                assert_eq!(synchronous, stop);
                for q in [1, 2, 7, 64, usize::MAX] {
                    let mut work = 0;
                    let error = SealedGraphView::build_cooperative_with_events(
                        &cx,
                        &fixture.image,
                        &fixture.vertices,
                        config,
                        SealedProjectionMask::UNMASKED,
                        limits,
                        quantum(q),
                        yield_now,
                        |event| {
                            if event == SealedBuildEvent::Work {
                                work += 1;
                                if work == stop {
                                    return Err(SealedProjectionError::Guard(Box::new(
                                        LiveRefusal(stop),
                                    )));
                                }
                            }
                            Ok(())
                        },
                    )
                    .await
                    .unwrap_err();
                    refusal(error, stop);
                    assert_eq!(
                        work, synchronous,
                        "direction={direction:?} q={q} stop={stop}"
                    );
                }
            }
            let actual =
                SealedGraphView::build(&cx, &fixture.image, &fixture.vertices, config, limits)
                    .unwrap();
            assert_same_projection(&cx, &actual, &expected);
        }
        assert_eq!(fixture.image.stats(), original_stats);
        assert_eq!(contexts.outstanding_obligations(), 0);
    });
}

#[test]
fn construction_futures_are_send_for_mutable_non_sync_observers_and_keep_local_callbacks() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        let fixture = fixture(&contexts.commit(), &cx, 2, 1).await;
        let (config, limits) = config(Directedness::Undirected, fixture.original);
        let mut total = 0;
        let expected = SealedGraphView::build_with_checkpoint(
            &cx,
            &fixture.image,
            &fixture.vertices,
            config,
            SealedProjectionMask::UNMASKED,
            limits,
            || {
                total += 1;
                Ok(())
            },
        )
        .unwrap();
        let mut work = 0;
        let mut scheduling = 0;
        let mut yields = 0;
        // Cell is Send but not Sync; the ordinary borrowed counters also make
        // this a FnMut observer. No shared-observer Sync bound may leak out.
        let not_sync = Cell::new(0usize);
        let work_count = &mut work;
        let schedule_count = &mut scheduling;
        let actual = assert_send(SealedGraphView::build_cooperative_with_events(
            &cx,
            &fixture.image,
            &fixture.vertices,
            config,
            SealedProjectionMask::UNMASKED,
            limits,
            quantum(2),
            || {
                yields += 1;
                yield_now()
            },
            move |event| {
                not_sync.set(not_sync.get() + 1);
                match event {
                    SealedBuildEvent::Work => *work_count += 1,
                    SealedBuildEvent::Scheduling => *schedule_count += 1,
                }
                Ok(())
            },
        ))
        .await
        .unwrap();
        assert_eq!(work, total);
        assert!(scheduling > 0 && yields > 0);
        assert_same_projection(&cx, &actual, &expected);

        let work = Rc::new(Cell::new(0));
        let yields = Rc::new(Cell::new(0));
        let local = SealedGraphView::build_cooperative_with_events(
            &cx,
            &fixture.image,
            &fixture.vertices,
            config,
            SealedProjectionMask::UNMASKED,
            limits,
            quantum(2),
            || {
                let local = Rc::clone(&yields);
                async move {
                    local.set(local.get() + 1);
                    yield_now().await;
                    assert!(local.get() > 0);
                }
            },
            |event| {
                if event == SealedBuildEvent::Work {
                    work.set(work.get() + 1);
                }
                Ok(())
            },
        )
        .await
        .unwrap();
        assert_eq!(work.get(), total);
        assert!(yields.get() > 0);
        assert_same_projection(&cx, &local, &expected);
        assert_eq!(contexts.outstanding_obligations(), 0);
    });
}

#[test]
fn scheduling_refusal_after_resume_precedes_further_source_work_or_publication() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        let fixture = fixture(&contexts.commit(), &cx, 2, 1).await;
        for direction in [
            Directedness::Directed,
            Directedness::Reversed,
            Directedness::Undirected,
        ] {
            let (config, limits) = config(direction, fixture.original);
            let yielded = Cell::new(false);
            let mut work = 0;
            let mut scheduling = 0;
            let mut yields = 0;
            let error = SealedGraphView::build_cooperative_with_events(
                &cx,
                &fixture.image,
                &fixture.vertices,
                config,
                SealedProjectionMask::UNMASKED,
                limits,
                quantum(1),
                || {
                    yielded.set(true);
                    yields += 1;
                    yield_now()
                },
                |event| {
                    match event {
                        SealedBuildEvent::Work => {
                            assert!(
                                !yielded.get(),
                                "work resumed before the live scheduling guard"
                            );
                            work += 1;
                        }
                        SealedBuildEvent::Scheduling => {
                            scheduling += 1;
                            if yielded.get() {
                                return Err(SealedProjectionError::Guard(Box::new(LiveRefusal(
                                    91,
                                ))));
                            }
                        }
                    }
                    Ok(())
                },
            )
            .await
            .unwrap_err();
            refusal(error, 91);
            assert_eq!(yields, 1);
            assert_eq!(scheduling, 2);
            assert!(work > 0);
        }
        assert_eq!(contexts.outstanding_obligations(), 0);
    });
}
