//! Weighted paths through real stored snapshots and actual cooperative futures.
//! Floyd-Warshall and explicit numeric counterexamples are independent oracles.

use super::*;
use fgdb_prism::FnxValue;
use fgdb_types::CanonicalF64;

fn call(source: VId, cutoff: Option<f64>, strict: bool) -> FnxCallSpec {
    let parameters: FnxParameters = [
        ("source".to_owned(), FnxArgument::Vertex(source)),
        ("cutoff".to_owned(), cutoff.map_or(FnxArgument::Null, FnxArgument::Float)),
        ("strict".to_owned(), FnxArgument::Boolean(strict)),
    ].into_iter().collect();
    FnxCallSpec::bind(
        "CALL fnx.single_source_dijkstra_path_length($source,$cutoff,$strict) YIELD vertex,distance",
        &parameters,
    ).unwrap()
}

async fn weighted(cx: &CommitCx, n: usize, edges: &[(usize, usize, f64)]) -> Database<MemVfs> {
    let mut db = small_database(cx, 0, false).await;
    let mut batch = WriteBatch::new(RelationId(1));
    for vertex in 0..n {
        batch.create_vertex(VId(vertex as u128), vec![LabelId(1)], vec![]);
    }
    for (id, &(source, target, weight)) in edges.iter().enumerate() {
        batch.add_edge(EId(id as u128), VId(source as u128), VId(target as u128),
            vec![(PropertyKeyId(1), CanonicalScalar::Float(CanonicalF64::new(weight)))]);
    }
    db.write(cx, batch).await.unwrap();
    db
}

fn rows(values: &[Option<f64>]) -> Vec<Vec<FnxValue>> {
    values.iter().enumerate().filter_map(|(vertex, distance)| distance.map(|distance| {
        vec![FnxValue::Vertex(VId(vertex as u128)), FnxValue::Float(distance)]
    })).collect()
}

#[test]
fn all_small_weighted_topologies_match_dense_oracle_and_both_existing_drivers() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        let pairs = [(0, 1), (0, 2), (1, 0), (1, 2), (2, 0), (2, 1)];
        for mask in 0..64 {
            let edges: Vec<_> = pairs.iter().enumerate()
                .filter(|(bit, _)| mask & (1 << bit) != 0)
                .map(|(bit, &(s, t))| (s, t, [0.0, 0.5, 3.0, 0.0, 7.0, 2.0][bit]))
                .collect();
            let db = weighted(&contexts.commit(), 3, &edges).await;
            for direction in [Directedness::Directed, Directedness::Reversed, Directedness::Undirected] {
                let mut opt = options(direction);
                opt.projection.parallel_edges = ParallelEdgePolicy::Minimum;
                let graph = projection(&db, &cx, opt).await;
                let view = db.read_session().unwrap();
                let decoded = view.prism_projection_at(&cx, view.frontier(), opt.selection,
                    opt.projection, opt.projection_limits, opt.source_limits).unwrap();
                let mut dense = [[f64::INFINITY; 3]; 3];
                for vertex in 0..3 { dense[vertex][vertex] = 0.0; }
                for &(s, t, weight) in &edges {
                    if direction != Directedness::Reversed { dense[s][t] = dense[s][t].min(weight); }
                    if direction != Directedness::Directed { dense[t][s] = dense[t][s].min(weight); }
                }
                for k in 0..3 { for s in 0..3 { for t in 0..3 {
                    dense[s][t] = dense[s][t].min(dense[s][k] + dense[k][t]);
                } } }
                for source in 0..3 { for cutoff in [None, Some(0.5)] { for strict in [false, true] {
                    let call = call(VId(source as u128), cutoff, strict);
                    let expected = call.execute_sealed(&cx, &graph, opt.execution_limits, memory()).unwrap();
                    let oracle: Vec<_> = dense[source].iter().map(|&distance| {
                        (distance.is_finite() && cutoff.is_none_or(|c| distance <= c)).then_some(distance)
                    }).collect();
                    assert_eq!(expected.rows, rows(&oracle), "mask={mask} {direction:?}");
                    let independent = call.execute(&decoded, opt.execution_limits, || cx.checkpoint()).unwrap();
                    assert_eq!(expected.rows, independent.rows);
                    assert_eq!(expected.certificate.result_digest, independent.certificate.result_digest);
                    let mut prior = None;
                    for q in [1, 7] {
                        let probe = Arc::new(SimulationCheckpointProbe::new(None));
                        let controlled = cx.with_checkpoint_probe(Arc::clone(&probe));
                        let (actual, pending) = drive(call.execute_sealed_cooperative(&controlled, &graph,
                            opt.execution_limits, memory(), quantum(q), yield_now), &probe, q);
                        let actual = actual.unwrap();
                        compare_execution(&actual, &expected);
                        assert!(actual.certificate.witness.queue_peak <= 3);
                        if q == 1 { assert!(pending > 3); }
                        if let Some(prior) = &prior { assert_eq!(&actual, prior); }
                        prior = Some(actual);
                    }
                } } }
            }
        }
        assert_eq!(contexts.outstanding_obligations(), 0);
    });
}

#[test]
fn numeric_policy_cutoff_closure_and_overflow_alternatives_survive_suspension() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        let opt = options(Directedness::Directed);
        let near = 1.0 - 5e-13;
        let db = weighted(&contexts.commit(), 3, &[(0, 1, 0.0), (0, 2, 1.0), (1, 2, near)]).await;
        let graph = projection(&db, &cx, opt).await;
        let mut outputs = Vec::new();
        for strict in [false, true] {
            let result = call(VId(0), None, strict).execute_sealed_cooperative(&cx, &graph,
                opt.execution_limits, memory(), quantum(1), yield_now).await.unwrap();
            assert_eq!(result.rows, rows(&[Some(0.0), Some(0.0), Some(if strict { near } else { 1.0 })]));
            outputs.push(result.rows);
        }
        assert_ne!(outputs[0], outputs[1], "the epsilon policy must remain observable");
        let db = weighted(&contexts.commit(), 4,
            &[(0, 1, 2.0), (1, 2, 0.0), (2, 1, 0.0), (2, 3, 0.5)]).await;
        let graph = projection(&db, &cx, opt).await;
        let result = call(VId(0), Some(2.0), true).execute_sealed_cooperative(&cx, &graph,
            opt.execution_limits, memory(), quantum(1), yield_now).await.unwrap();
        assert_eq!(result.rows, rows(&[Some(0.0), Some(2.0), Some(2.0), None]));

        let overflow = [(0, 1, f64::MAX * 0.75), (1, 3, f64::MAX * 0.75)];
        for finite_alternative in [false, true] {
            let mut edges = overflow.to_vec();
            if finite_alternative { edges.extend([(0, 2, f64::MAX * 0.875), (2, 3, 0.0)]); }
            let db = weighted(&contexts.commit(), 4, &edges).await;
            let graph = projection(&db, &cx, opt).await;
            for cutoff in [None, Some(f64::MAX)] {
                let actual = call(VId(0), cutoff, true).execute_sealed_cooperative(&cx, &graph,
                    opt.execution_limits, memory(), quantum(1), yield_now).await;
                if finite_alternative {
                    assert_eq!(actual.unwrap().rows, rows(&[Some(0.0), Some(f64::MAX * 0.75),
                        Some(f64::MAX * 0.875), Some(f64::MAX * 0.875)]));
                } else if cutoff.is_some() {
                    assert_eq!(actual.unwrap().rows, rows(&[Some(0.0), Some(f64::MAX * 0.75), None, None]));
                } else {
                    assert!(matches!(actual, Err(FnxSealedExecutionError::Execution(
                        FnxExecutionError::InvalidNumericResult))));
                }
            }
        }
        // Entire-source validation is independent of reachability and cutoff.
        let db = weighted(&contexts.commit(), 4, &[(2, 3, -1.0)]).await;
        let graph = projection(&db, &cx, opt).await;
        assert!(matches!(call(VId(0), Some(0.0), true).execute_sealed_cooperative(&cx, &graph,
            opt.execution_limits, memory(), quantum(1), yield_now).await,
            Err(FnxSealedExecutionError::Execution(FnxExecutionError::NegativeWeight))));
    });
}

#[test]
fn exact_independent_limits_preflight_and_large_heap_repairs_obey_the_same_quantum() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        let opt = options(Directedness::Directed);
        // Descending offers force repeated upward repairs; popping exercises
        // the opposite direction. A lazy duplicate queue is not used.
        let n = 257;
        let edges: Vec<_> = (1..n).map(|v| (0, v, (n - v) as f64)).collect();
        let db = weighted(&contexts.commit(), n, &edges).await;
        let graph = projection(&db, &cx, opt).await;
        drop(db);
        let call = call(VId(0), None, true);
        let expected = call.execute_sealed(&cx, &graph, opt.execution_limits, memory()).unwrap();
        let bytes = call.outputs().len() * size_of::<String>()
            + call.outputs().iter().map(|column| column.name.len()).sum::<usize>()
            + n * (size_of::<Vec<FnxValue>>() + call.outputs().len() * size_of::<FnxValue>());
        let limits = FnxExecutionLimits { max_iterations: 0, max_result_rows: n,
            max_estimated_work: expected.certificate.estimated_work };
        let mem = FnxMemoryLimits { max_kernel_workspace_bytes: expected.certificate.kernel_workspace_bytes,
            max_result_bytes: bytes };
        let probe = Arc::new(SimulationCheckpointProbe::new(None));
        let controlled = cx.with_checkpoint_probe(Arc::clone(&probe));
        let (actual, pending) = drive(call.execute_sealed_cooperative(&controlled, &graph,
            limits, mem, quantum(1), yield_now), &probe, 1);
        compare_execution(&actual.unwrap(), &expected);
        assert_eq!(expected.certificate.witness.queue_peak, n - 1);
        assert!(pending > n * 5, "heap initialization, sifts and result conversion must cooperate");
        for (resource, cap, memory) in [
            ("estimated work", FnxExecutionLimits { max_estimated_work: limits.max_estimated_work - 1, ..limits }, mem),
            ("kernel workspace bytes", limits, FnxMemoryLimits { max_kernel_workspace_bytes: mem.max_kernel_workspace_bytes - 1, ..mem }),
            ("result rows", FnxExecutionLimits { max_result_rows: n - 1, ..limits }, mem),
            ("result bytes", limits, FnxMemoryLimits { max_result_bytes: bytes - 1, ..mem }),
        ] {
            assert!(matches!(call.execute_sealed_cooperative(&cx, &graph, cap, memory,
                quantum(1), yield_now).await,
                Err(FnxSealedExecutionError::Execution(FnxExecutionError::LimitExceeded { resource: actual, .. }))
                    if actual == resource));
        }
        let absent = self::call(VId(u128::MAX), None, true);
        assert!(matches!(absent.execute_sealed_cooperative(&cx, &graph, limits, mem,
            quantum(1), || -> std::future::Ready<()> { panic!("preflight yielded") }).await,
            Err(FnxSealedExecutionError::Execution(FnxExecutionError::UnknownSource(VId(u128::MAX))))));
        let db = small_database(&contexts.commit(), 0, false).await;
        let empty = projection(&db, &cx, opt).await;
        assert!(matches!(call.execute_sealed_cooperative(&cx, &empty, limits, mem,
            quantum(1), yield_now).await,
            Err(FnxSealedExecutionError::Execution(FnxExecutionError::UnknownSource(VId(0))))));
    });
}

#[test]
fn cancellation_live_refusal_and_drop_cover_every_heap_and_delivery_suspension() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        let opt = options(Directedness::Directed);
        let db = weighted(&contexts.commit(), 5, &[(0, 1, 8.0), (0, 2, 4.0), (0, 3, 2.0),
            (3, 1, 1.0), (1, 2, 0.0), (2, 4, 1.0)]).await;
        let graph = projection(&db, &cx, opt).await;
        let original = db.read_session().unwrap().partition_root();
        let call = call(VId(0), None, true);
        let probe = Arc::new(SimulationCheckpointProbe::new(None));
        let controlled = cx.with_checkpoint_probe(Arc::clone(&probe));
        let (expected, pending) = drive(call.execute_sealed_cooperative(&controlled, &graph,
            opt.execution_limits, memory(), quantum(3), yield_now), &probe, 3);
        let expected = expected.unwrap();
        for stop in 1..=probe.calls() {
            let probe = Arc::new(SimulationCheckpointProbe::new(Some(stop)));
            let controlled = cx.with_checkpoint_probe(Arc::clone(&probe));
            let (result, _) = drive(call.execute_sealed_cooperative(&controlled, &graph,
                opt.execution_limits, memory(), quantum(3), yield_now), &probe, 3);
            assert!(matches!(result, Err(FnxSealedExecutionError::Cancelled(_))));
            assert_eq!(probe.calls(), stop);
        }
        let mut guards = 0;
        call.execute_sealed_cooperative_with_checkpoint(&cx, &graph, opt.execution_limits,
            memory(), quantum(3), yield_now, || { guards += 1; Ok(()) }).await.unwrap();
        for stop in 1..=guards {
            let mut seen = 0;
            let result = call.execute_sealed_cooperative_with_checkpoint(&cx, &graph,
                opt.execution_limits, memory(), quantum(3), yield_now, || {
                    seen += 1;
                    if seen == stop { Err(SealedProjectionError::Guard(Box::new(LiveRefusal(stop)))) }
                    else { Ok(()) }
                }).await;
            live_refusal(result.unwrap_err(), stop);
            assert_eq!(seen, stop);
        }
        for stop in 0..=pending {
            let revoked = Cell::new(false);
            let probe = Arc::new(SimulationCheckpointProbe::new(None));
            let controlled = cx.with_checkpoint_probe(Arc::clone(&probe));
            let waker = Waker::from(Arc::new(Wakes::default()));
            let mut task = Context::from_waker(&waker);
            let mut future = Box::pin(call.execute_sealed_cooperative_with_checkpoint(&controlled,
                &graph, opt.execution_limits, memory(), quantum(3), yield_now, || {
                    if revoked.get() { Err(SealedProjectionError::Guard(Box::new(LiveRefusal(stop)))) }
                    else { Ok(()) }
                }));
            for _ in 0..stop { assert!(future.as_mut().poll(&mut task).is_pending()); }
            if stop > 0 {
                let before = probe.calls();
                revoked.set(true);
                let Poll::Ready(result) = future.as_mut().poll(&mut task) else {
                    panic!("revoked heap repair resumed another slice");
                };
                live_refusal(result.unwrap_err(), stop);
                assert_eq!(probe.calls(), before + 1);
            }
            drop(future);
            // Independently abandon the operation in its suspended state,
            // rather than treating a completed error as a dropped future test.
            let mut abandoned = Box::pin(call.execute_sealed_cooperative(&cx, &graph,
                opt.execution_limits, memory(), quantum(3), yield_now));
            for _ in 0..stop { assert!(abandoned.as_mut().poll(&mut task).is_pending()); }
            drop(abandoned);
            assert_eq!(contexts.outstanding_obligations(), 0);
            assert_eq!(db.read_session().unwrap().partition_root(), original);
        }
        assert_eq!(call.execute_sealed_cooperative(&cx, &graph, opt.execution_limits,
            memory(), quantum(7), yield_now).await.unwrap(), expected);
    });
}

#[test]
fn historical_full_width_sources_and_hidden_reciprocal_history_preserve_numeric_bits() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        let (db, original) = database(&contexts.commit()).await;
        for at in [original, db.frontier().unwrap()] {
            for direction in [Directedness::Directed, Directedness::Reversed, Directedness::Undirected] {
                let mut opt = options(direction); opt.as_of = Some(at);
                let graph = projection(&db, &cx, opt).await;
                for source in [VId(0), VId(u128::MAX)] { for strict in [false, true] {
                    let call = call(source, None, strict);
                    let expected = call.execute_sealed(&cx, &graph, opt.execution_limits, memory()).unwrap();
                    let mut previous = None;
                    for q in [1, 257] {
                        let actual = call.execute_sealed_cooperative(&cx, &graph,
                            opt.execution_limits, memory(), quantum(q), yield_now).await.unwrap();
                        compare_execution(&actual, &expected);
                        if let Some(previous) = previous { assert_eq!(actual, previous); }
                        previous = Some(actual);
                    }
                } }
            }
        }
    });
}
