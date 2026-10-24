//! Direction-optimizing BFS through real retained Chronicle/Strata history.

use super::*;
use fgdb_prism::FnxValue;

fn vertex(index: usize) -> VId {
    VId((1_u128 << 100) + index as u128)
}

async fn dense_database(cx: &CommitCx) -> (Database<MemVfs>, CommitSeq) {
    let mut db = Database::<MemVfs>::open_memory(
        cx,
        DatabaseKeys::new(
            [0xb1; 32],
            DatabaseSecurityNamespaceId([0xb2; 32]),
            [0xb3; 32],
        ),
    )
    .await
    .unwrap();
    let mut batch = WriteBatch::new(RelationId(1));
    // A dense frontier, a narrow tail, and an unreachable vertex exercise
    // actual push -> pull -> push transitions, not only early completion.
    for index in 0..50 {
        batch.create_vertex(vertex(index), vec![LabelId(1)], vec![]);
    }
    batch.create_vertex(VId(u128::MAX), vec![LabelId(1)], vec![]);
    batch.create_vertex(vertex(500), vec![LabelId(2)], vec![]);
    let mut edge = 1;
    for source in 0..48 {
        for target in source + 1..48 {
            batch.add_edge(EId(edge), vertex(source), vertex(target), vec![]);
            edge += 1;
        }
    }
    batch.add_edge(EId(2000), vertex(47), vertex(48), vec![]);
    batch.add_edge(EId(2001), vertex(48), vertex(49), vec![]);
    // Retained excluded incidences are real physical work, but the hidden
    // endpoint must never acquire a result row or change the BFS distances.
    for id in 3000..3096 {
        batch.add_edge(EId(id), vertex(0), vertex(500), vec![]);
    }
    batch.add_edge(EId(4000), vertex(1), vertex(2), vec![]);
    batch.add_edge(EId(4001), vertex(4), vertex(4), vec![]);
    batch.add_edge(EId(4002), vertex(48), vertex(48), vec![]);
    db.write(cx, batch).await.unwrap();
    let original = db.frontier().unwrap();
    let mut change = WriteBatch::new(RelationId(1));
    change.delete_edge(EId(2001));
    change.create_vertex(vertex(50), vec![LabelId(1)], vec![]);
    db.write(cx, change).await.unwrap();
    (db, original)
}

fn read_options() -> FnxReadOptions {
    let mut opt = options(Directedness::Undirected);
    opt.selection.weight = FnxWeightSpec::Unit;
    opt.projection.parallel_edges = ParallelEdgePolicy::CollapseUnit;
    opt
}

fn assert_certificate_transcript(result: &FnxResult) {
    use fgdb_crypto::Hasher;

    fn text(hash: &mut Hasher, value: &str) {
        hash.update(&(value.len() as u128).to_le_bytes());
        hash.update(value.as_bytes());
    }
    // Reconstruct the public v5 transcript independently of the sealed
    // finishing helpers. A card that appears in the result but is omitted
    // from its certificate hash must fail this integration law.
    let c = &result.certificate;
    let card = c.bfs_decision_card.as_ref().unwrap();
    let mut hash = Hasher::new();
    hash.update(b"fgdb:prism:call-certificate:v5");
    hash.update(&c.registry_version.to_le_bytes());
    hash.update(c.implementation_revision.as_bytes());
    text(&mut hash, c.execution_kernel);
    hash.update(&c.kernel_source_digest.0);
    text(&mut hash, c.numeric_profile);
    hash.update(&c.projection_digest.0);
    hash.update(&c.call_digest.0);
    hash.update(&c.result_digest.0);
    text(&mut hash, c.adapter.as_str());
    for value in [
        c.vertices,
        c.edges,
        c.input_edges,
        c.estimated_work,
        c.kernel_workspace_bytes,
        c.witness.nodes_touched,
        c.witness.edges_scanned,
        c.witness.queue_peak,
    ] {
        hash.update(&(value as u128).to_le_bytes());
    }
    text(&mut hash, &c.witness.algorithm);
    text(&mut hash, &c.witness.complexity_claim);
    hash.update(&[1]);
    hash.update(&card.policy_version.to_le_bytes());
    text(&mut hash, card.fallback);
    for value in [
        card.dwell_levels,
        card.pull_cost_multiplier,
        card.pulls.len(),
    ] {
        hash.update(&(value as u128).to_le_bytes());
    }
    for pull in &card.pulls {
        for value in [
            pull.depth,
            pull.frontier_vertices,
            pull.frontier_arcs,
            pull.unseen_incoming_arcs,
            pull.vertices,
            pull.benefit_lower,
            pull.benefit_upper,
            pull.candidate_visits,
            pull.incoming_visits,
        ] {
            hash.update(&(value as u128).to_le_bytes());
        }
    }
    assert!(c.component_decision_card.is_none());
    hash.update(&[0]);
    assert_eq!(hash.finalize(), c.digest);
}

#[test]
fn compressed_bfs_pull_matches_history_oracle_and_bounded_queue_fallback() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        let (db, original) = dense_database(&contexts.commit()).await;
        let current = db.frontier().unwrap();
        let call = FnxCallSpec::single_source_shortest_path_length(vertex(0), None);
        for at in [original, current] {
            let mut opt = read_options();
            opt.as_of = Some(at);
            let graph = projection(&db, &cx, opt).await;
            let actual = call
                .execute_sealed(&cx, &graph, opt.execution_limits, memory())
                .unwrap();
            let native = db
                .read_session()
                .unwrap()
                .execute_fnx(&cx, &call, opt)
                .unwrap()
                .analytics;
            let reached = if at == original { 50 } else { 49 };
            let expected: Vec<_> = (0..reached)
                .map(|index| {
                    vec![
                        FnxValue::Vertex(vertex(index)),
                        FnxValue::Integer(match index {
                            0 => 0,
                            1..48 => 1,
                            48 => 2,
                            _ => 3,
                        }),
                    ]
                })
                .collect();
            assert_eq!(actual.rows, expected);
            assert_eq!(actual.rows, native.rows);
            assert_eq!(
                actual.certificate.result_digest,
                native.certificate.result_digest
            );
            let card = actual.certificate.bfs_decision_card.as_ref().unwrap();
            assert_eq!(card.policy_version, 2);
            assert!(!card.pulls.is_empty());
            assert!(card.pulls.iter().any(|pull| pull.incoming_visits > 0));
            assert!(card.pulls.len() <= usize::BITS as usize);
            assert!(card.pulls.iter().all(|pull| pull.benefit_lower > 0));
            assert_certificate_transcript(&actual);

            // The same graph and answer fit this tighter row limit, but a
            // different discovery order must retain the queue fallback.
            let fallback_limits = FnxExecutionLimits {
                max_result_rows: reached,
                ..opt.execution_limits
            };
            assert!(reached < graph.node_count());
            let fallback = call
                .execute_sealed(&cx, &graph, fallback_limits, memory())
                .unwrap();
            assert_eq!(fallback.rows, actual.rows);
            assert_eq!(
                fallback.certificate.result_digest,
                actual.certificate.result_digest
            );
            assert!(fallback.certificate.bfs_decision_card.is_none());
            assert!(
                actual.certificate.witness.edges_scanned * 4
                    < fallback.certificate.witness.edges_scanned,
                "compare real cursor visits against the live queue incumbent"
            );
            let mut previous = None;
            for q in [1, 7, 257] {
                let probe = Arc::new(SimulationCheckpointProbe::new(None));
                let controlled = cx.with_checkpoint_probe(Arc::clone(&probe));
                let (cooperative, pending) = drive(
                    call.execute_sealed_cooperative(
                        &controlled,
                        &graph,
                        opt.execution_limits,
                        memory(),
                        quantum(q),
                        yield_now,
                    ),
                    &probe,
                    q,
                );
                let cooperative = cooperative.unwrap();
                compare_execution(&cooperative, &actual);
                assert_certificate_transcript(&cooperative);
                assert_eq!(
                    cooperative.certificate.bfs_decision_card,
                    actual.certificate.bfs_decision_card
                );
                assert!(pending > 0);
                if let Some(previous) = &previous {
                    assert_eq!(&cooperative, previous);
                }
                previous = Some(cooperative);
            }
            assert_eq!(
                db.execute_fnx_sealed(&cx, &call, opt, memory(), SealedLimits::default())
                    .await
                    .unwrap()
                    .analytics,
                actual
            );
            assert_eq!(
                db.execute_fnx_sealed_cooperative(
                    &cx,
                    &call,
                    opt,
                    memory(),
                    SealedLimits::default(),
                    quantum(7),
                )
                .await
                .unwrap()
                .analytics,
                previous.unwrap()
            );
        }
        assert_eq!(contexts.outstanding_obligations(), 0);
    });
}

#[test]
fn compressed_bfs_optional_pull_respects_cutoff_and_independent_resource_limits() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        let (db, original) = dense_database(&contexts.commit()).await;
        let mut opt = read_options();
        opt.as_of = Some(original);
        let graph = projection(&db, &cx, opt).await;
        for cutoff in [Some(0), Some(1), Some(2), None] {
            let call = FnxCallSpec::single_source_shortest_path_length(vertex(0), cutoff);
            let expected = call
                .execute_sealed(&cx, &graph, opt.execution_limits, memory())
                .unwrap();
            if cutoff.is_some_and(|limit| limit <= 1) {
                assert_eq!(
                    expected.certificate.execution_kernel,
                    "fgdb-prism/sealed-bfs-v1"
                );
                assert!(expected.certificate.bfs_decision_card.is_none());
            }
            let native = db
                .read_session()
                .unwrap()
                .execute_fnx(&cx, &call, opt)
                .unwrap()
                .analytics;
            assert_eq!(expected.rows, native.rows);
            let columns = call.outputs().len() * size_of::<String>()
                + call
                    .outputs()
                    .iter()
                    .map(|column| column.name.len())
                    .sum::<usize>();
            let bytes = columns
                + expected.rows.len()
                    * (size_of::<Vec<FnxValue>>() + call.outputs().len() * size_of::<FnxValue>());
            // The output allowance fits exactly the reachable rows, but not
            // all projected vertices. It must disable optional pull, not
            // reject the valid request during full-population preflight.
            let tight = FnxMemoryLimits {
                max_result_bytes: bytes,
                ..memory()
            };
            let fallback = call
                .execute_sealed(&cx, &graph, opt.execution_limits, tight)
                .unwrap();
            assert_eq!(fallback.rows, expected.rows);
            assert!(fallback.certificate.bfs_decision_card.is_none());
            let exact = FnxMemoryLimits {
                max_kernel_workspace_bytes: expected.certificate.kernel_workspace_bytes,
                ..memory()
            };
            assert_eq!(
                call.execute_sealed(&cx, &graph, opt.execution_limits, exact)
                    .unwrap(),
                expected
            );
            let queue_memory = FnxMemoryLimits {
                max_kernel_workspace_bytes: fallback.certificate.kernel_workspace_bytes,
                ..memory()
            };
            let low_memory = call
                .execute_sealed(&cx, &graph, opt.execution_limits, queue_memory)
                .unwrap();
            assert_eq!(low_memory.rows, expected.rows);
            assert!(low_memory.certificate.bfs_decision_card.is_none());
            let failure = FnxMemoryLimits {
                max_result_bytes: bytes - 1,
                ..memory()
            };
            for cooperative in [false, true] {
                let result = if cooperative {
                    call.execute_sealed_cooperative(
                        &cx,
                        &graph,
                        opt.execution_limits,
                        failure,
                        quantum(7),
                        yield_now,
                    )
                    .await
                } else {
                    call.execute_sealed(&cx, &graph, opt.execution_limits, failure)
                };
                assert!(matches!(
                    result,
                    Err(FnxSealedExecutionError::Execution(
                        FnxExecutionError::LimitExceeded {
                            resource: "result bytes",
                            ..
                        }
                    ))
                ));
            }
        }
        let isolated = FnxCallSpec::single_source_shortest_path_length(VId(u128::MAX), None);
        let actual = isolated
            .execute_sealed(&cx, &graph, opt.execution_limits, memory())
            .unwrap();
        assert_eq!(
            actual.rows,
            vec![vec![FnxValue::Vertex(VId(u128::MAX)), FnxValue::Integer(0)]]
        );
        assert_eq!(
            actual.certificate.execution_kernel,
            "fgdb-prism/sealed-bfs-v1"
        );
        assert!(actual.certificate.bfs_decision_card.is_none());
        assert_eq!(contexts.outstanding_obligations(), 0);
    });
}

#[test]
fn compressed_bfs_pull_cancels_revokes_and_drops_without_partial_output() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        let (db, _) = dense_database(&contexts.commit()).await;
        let opt = read_options();
        let graph = projection(&db, &cx, opt).await;
        let root_before = db.read_session().unwrap().partition_root();
        let call = FnxCallSpec::single_source_shortest_path_length(vertex(0), None);
        let probe = Arc::new(SimulationCheckpointProbe::new(None));
        let controlled = cx.with_checkpoint_probe(Arc::clone(&probe));
        let (expected, yields) = drive(
            call.execute_sealed_cooperative(
                &controlled,
                &graph,
                opt.execution_limits,
                memory(),
                quantum(7),
                yield_now,
            ),
            &probe,
            7,
        );
        let expected = expected.unwrap();
        assert!(expected.certificate.bfs_decision_card.is_some());
        for stop in 1..=probe.calls() {
            let stop_probe = Arc::new(SimulationCheckpointProbe::new(Some(stop)));
            let controlled = cx.with_checkpoint_probe(Arc::clone(&stop_probe));
            let (result, _) = drive(
                call.execute_sealed_cooperative(
                    &controlled,
                    &graph,
                    opt.execution_limits,
                    memory(),
                    quantum(7),
                    yield_now,
                ),
                &stop_probe,
                7,
            );
            assert!(matches!(result, Err(FnxSealedExecutionError::Cancelled(_))));
            assert_eq!(stop_probe.calls(), stop);
        }
        for stop in 0..=yields {
            let revoked = Cell::new(false);
            let waker = Waker::from(Arc::new(Wakes::default()));
            let mut task = Context::from_waker(&waker);
            let mut future = Box::pin(call.execute_sealed_cooperative_with_checkpoint(
                &cx,
                &graph,
                opt.execution_limits,
                memory(),
                quantum(7),
                yield_now,
                || {
                    if revoked.get() {
                        Err(SealedProjectionError::Guard(Box::new(LiveRefusal(stop))))
                    } else {
                        Ok(())
                    }
                },
            ));
            for _ in 0..stop {
                assert!(future.as_mut().poll(&mut task).is_pending());
            }
            revoked.set(true);
            let Poll::Ready(result) = future.as_mut().poll(&mut task) else {
                panic!("revocation must precede resumed compressed BFS work");
            };
            live_refusal(result.unwrap_err(), stop);
            drop(future);
            let mut abandoned = Box::pin(call.execute_sealed_cooperative(
                &cx,
                &graph,
                opt.execution_limits,
                memory(),
                quantum(7),
                yield_now,
            ));
            for _ in 0..stop {
                assert!(abandoned.as_mut().poll(&mut task).is_pending());
            }
            drop(abandoned);
            assert_eq!(contexts.outstanding_obligations(), 0);
            assert_eq!(db.read_session().unwrap().partition_root(), root_before);
        }
        assert_eq!(
            call.execute_sealed_cooperative(
                &cx,
                &graph,
                opt.execution_limits,
                memory(),
                quantum(13),
                yield_now,
            )
            .await
            .unwrap(),
            expected
        );
    });
}

/// Manual same-process measurement of the public Database call, including
/// vertex selection, authenticated sealing, projection, BFS, result/certificate
/// construction, consumption and result cleanup on every sample. Database
/// creation/open is outside the timed region; this small MemVfs fixture is not
/// a durable-I/O benchmark or a Section 17 performance-gate claim.
#[test]
#[ignore = "manual paired Database-to-result measurement; no timing pass threshold"]
fn measure_compressed_bfs_database_to_result_against_queue_fallback() {
    use fgdb_crypto::Hasher;
    use std::hint::black_box;
    use std::time::{Duration, Instant};

    struct Sample {
        elapsed: Duration,
        rows: usize,
        consumed: [u8; 32],
        result_digest: [u8; 32],
        workspace: usize,
        scanned: usize,
        pulls: usize,
    }

    async fn sample(
        db: &Database<MemVfs>,
        cx: &QueryCx,
        call: &FnxCallSpec,
        options: FnxReadOptions,
        memory: FnxMemoryLimits,
    ) -> Sample {
        let start = Instant::now();
        let result = db
            .execute_fnx_sealed(cx, call, options, memory, SealedLimits::default())
            .await
            .unwrap();
        let mut hash = Hasher::new();
        for row in &result.analytics.rows {
            for value in row {
                match value {
                    FnxValue::Vertex(vertex) => {
                        hash.update(&[0]);
                        hash.update(&vertex.0.to_le_bytes());
                    }
                    FnxValue::Integer(distance) => {
                        hash.update(&[1]);
                        hash.update(&distance.to_le_bytes());
                    }
                    _ => panic!("BFS returns vertex identities and exact distances"),
                }
            }
        }
        let consumed = black_box(hash.finalize().0);
        let rows = result.analytics.rows.len();
        let certificate = black_box(&result.analytics.certificate);
        black_box(certificate.digest);
        let result_digest = certificate.result_digest.0;
        let workspace = certificate.kernel_workspace_bytes;
        let scanned = certificate.witness.edges_scanned;
        let pulls = certificate
            .bfs_decision_card
            .as_ref()
            .map_or(0, |card| card.pulls.len());
        drop(result);
        Sample {
            elapsed: start.elapsed(),
            rows,
            consumed,
            result_digest,
            workspace,
            scanned,
            pulls,
        }
    }

    fn milliseconds(samples: &[Duration]) -> (f64, f64, f64) {
        let mut ordered = samples.to_vec();
        ordered.sort_unstable();
        let ms = |elapsed: Duration| elapsed.as_secs_f64() * 1000.0;
        (
            ms(ordered[0]),
            ms(ordered[ordered.len() / 2]),
            ms(*ordered.last().unwrap()),
        )
    }

    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        let (db, original) = dense_database(&contexts.commit()).await;
        let current = db.frontier().unwrap();
        println!(
            "scope=warm_database_to_consumed_result storage=MemVfs fixture=small_dense_with_tail_excluded_history_and_isolates debug_assertions={} pairs=5 alternating_order=true database_creation_open_excluded=true source_preparation_included=true timing_gate=false",
            cfg!(debug_assertions)
        );
        for (shape, at, source, cutoff, expect_pull) in [
            ("dense_historical", original, vertex(0), None, true),
            ("dense_current", current, vertex(0), None, true),
            ("cutoff_one", current, vertex(0), Some(1), false),
            ("isolated_source", current, VId(u128::MAX), None, false),
        ] {
            let mut opt = read_options();
            opt.as_of = Some(at);
            // Read only the population needed for the independently bounded
            // queue workspace. Drop this image before any timed call; neither
            // lane receives a prebuilt projection instead of the public API.
            let n = projection(&db, &cx, opt).await.node_count();
            let call = FnxCallSpec::single_source_shortest_path_length(source, cutoff);
            let queue_memory = FnxMemoryLimits {
                max_kernel_workspace_bytes: n * (size_of::<usize>() + size_of::<Option<usize>>()),
                ..memory()
            };
            // Workspace is the only differing request field. It selects the
            // unchanged production queue fallback without altering graph,
            // source, cutoff, result limits, work limits or sealing limits.
            let incumbent = sample(&db, &cx, &call, opt, queue_memory).await;
            let candidate = sample(&db, &cx, &call, opt, memory()).await;
            assert_eq!(incumbent.result_digest, candidate.result_digest);
            assert_eq!(incumbent.consumed, candidate.consumed);
            assert_eq!(incumbent.pulls, 0);
            assert_eq!(candidate.pulls > 0, expect_pull);
            let mut old_times = Vec::new();
            let mut new_times = Vec::new();
            for pair in 0..5 {
                let (old, new) = if pair % 2 == 0 {
                    let old = sample(&db, &cx, &call, opt, queue_memory).await;
                    let new = sample(&db, &cx, &call, opt, memory()).await;
                    (old, new)
                } else {
                    let new = sample(&db, &cx, &call, opt, memory()).await;
                    let old = sample(&db, &cx, &call, opt, queue_memory).await;
                    (old, new)
                };
                for value in [&old, &new] {
                    assert_eq!(value.result_digest, incumbent.result_digest);
                    assert_eq!(value.consumed, incumbent.consumed);
                    assert_eq!(value.rows, incumbent.rows);
                }
                assert_eq!(old.pulls, 0);
                assert_eq!(new.pulls > 0, expect_pull);
                assert_eq!(old.scanned, incumbent.scanned);
                assert_eq!(new.scanned, candidate.scanned);
                old_times.push(old.elapsed);
                new_times.push(new.elapsed);
            }
            let (old_min, old_median, old_max) = milliseconds(&old_times);
            let (new_min, new_median, new_max) = milliseconds(&new_times);
            println!(
                "shape={shape} vertices={n} rows={} queue_ms_min={old_min:.6} queue_ms_median={old_median:.6} queue_ms_max={old_max:.6} candidate_ms_min={new_min:.6} candidate_ms_median={new_median:.6} candidate_ms_max={new_max:.6} queue_workspace={} candidate_workspace={} queue_arcs={} candidate_arcs={} pull_levels={} result_digest={:02x?} parity=exact",
                incumbent.rows, incumbent.workspace, candidate.workspace,
                incumbent.scanned, candidate.scanned, candidate.pulls,
                incumbent.result_digest,
            );
        }
        assert_eq!(contexts.outstanding_obligations(), 0);
    });
}
