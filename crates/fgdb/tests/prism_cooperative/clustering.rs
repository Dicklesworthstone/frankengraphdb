//! Cooperative topology kernels over real immutable Chronicle/Strata sources.
//! Reuse the parent harness so raw history, actual Pending/wake behavior and
//! certificate parity are checked at the same seam as the other kernels.

use super::*;
use fgdb_prism::{AdapterPath, FnxValue};

#[test]
fn topology_calls_preserve_history_reductions_aliases_and_certificates_across_quanta() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        let (mut db, original) = database(&contexts.commit()).await;
        // Close a triangle only in the new snapshot; retain the existing tail,
        // self-loop, reciprocal/parallel edges and 700 excluded incidences.
        // Signed weights are legal for these unweighted topology procedures.
        let mut batch = WriteBatch::new(RelationId(1));
        batch.create_vertex(VId(3), vec![LabelId(1)], vec![]);
        batch.add_edge(
            EId(7),
            VId(2),
            VId(0),
            vec![(PropertyKeyId(1), CanonicalScalar::Int(-4))],
        );
        db.write(&contexts.commit(), batch).await.unwrap();
        let current = db.frontier().unwrap();
        let view = db.read_session().unwrap();
        for at in [original, current] {
            for reduction in [
                ParallelEdgePolicy::Sum,
                ParallelEdgePolicy::Minimum,
                ParallelEdgePolicy::Maximum,
                ParallelEdgePolicy::CollapseUnit,
            ] {
                for loops in [SelfLoopPolicy::Keep, SelfLoopPolicy::Drop] {
                    let mut opt = options(Directedness::Undirected);
                    opt.as_of = Some(at);
                    opt.projection.parallel_edges = reduction;
                    opt.projection.self_loops = loops;
                    let graph = projection(&db, &cx, opt).await;
                    let decoded = view
                        .prism_projection_at(
                            &cx,
                            at,
                            opt.selection,
                            opt.projection,
                            opt.projection_limits,
                            opt.source_limits,
                        )
                        .unwrap();
                    for text in [
                        "CALL fnx.triangles() YIELD triangles AS total,vertex AS id",
                        "CALL fnx.clustering_coefficient() YIELD score AS coefficient,vertex AS id",
                    ] {
                        let call = FnxCallSpec::bind(text, &FnxParameters::new()).unwrap();
                        assert!(call.supports_cooperative_sealed_execution());
                        let expected = call
                            .execute_sealed(&cx, &graph, opt.execution_limits, memory())
                            .unwrap();
                        let independent = call
                            .execute(&decoded, opt.execution_limits, || cx.checkpoint())
                            .unwrap();
                        assert_eq!(expected.rows, independent.rows);
                        assert_eq!(
                            expected.certificate.result_digest,
                            independent.certificate.result_digest
                        );
                        // Make the oracle's nonzero case explicit: a traversal
                        // that accidentally drops the new closing edge must fail.
                        for row in &expected.rows {
                            let fgdb_prism::FnxValue::Vertex(VId(id)) = row[1] else {
                                panic!("native vertex identity");
                            };
                            if matches!(call.algorithm(), FnxAlgorithm::Triangles) {
                                let count = u64::from(at == current && id <= 2);
                                assert_eq!(row[0], fgdb_prism::FnxValue::Integer(count));
                            }
                        }
                        let mut previous = None;
                        for q in [1, 7, 257] {
                            let probe = Arc::new(SimulationCheckpointProbe::new(None));
                            let controlled = cx.with_checkpoint_probe(Arc::clone(&probe));
                            let (actual, pending) = drive(
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
                            let actual = actual.unwrap();
                            compare_execution(&actual, &expected);
                            assert_eq!(actual.certificate.adapter, AdapterPath::CompressedCursor);
                            if q == 1 {
                                assert!(pending > 700, "excluded/history work must yield");
                            }
                            if let Some(previous) = &previous {
                                assert_eq!(&actual, previous);
                            }
                            previous = Some(actual);
                        }
                    }
                }
            }
        }
        assert_eq!(contexts.outstanding_obligations(), 0);
    });
}

#[test]
fn topology_admission_matches_exact_sync_work_workspace_rows_and_result_bytes() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        let mut db = small_database(&contexts.commit(), 3, true).await;
        let mut batch = WriteBatch::new(RelationId(1));
        batch.add_edge(
            EId(3),
            VId(2),
            VId(0),
            vec![(PropertyKeyId(1), CanonicalScalar::Int(-1))],
        );
        db.write(&contexts.commit(), batch).await.unwrap();
        let opt = options(Directedness::Undirected);
        let graph = projection(&db, &cx, opt).await;
        for call in [
            FnxCallSpec::triangles(),
            FnxCallSpec::clustering_coefficient(),
        ] {
            let expected = call
                .execute_sealed(&cx, &graph, opt.execution_limits, memory())
                .unwrap();
            let columns = call.outputs().len() * size_of::<String>()
                + call
                    .outputs()
                    .iter()
                    .map(|column| column.name.len())
                    .sum::<usize>();
            let bytes = columns
                + graph.node_count()
                    * (size_of::<Vec<fgdb_prism::FnxValue>>()
                        + call.outputs().len() * size_of::<fgdb_prism::FnxValue>());
            let exact = FnxExecutionLimits {
                max_iterations: 0,
                max_result_rows: graph.node_count(),
                max_estimated_work: expected.certificate.estimated_work,
            };
            let mem = FnxMemoryLimits {
                max_kernel_workspace_bytes: expected.certificate.kernel_workspace_bytes,
                max_result_bytes: bytes,
            };
            let actual = call
                .execute_sealed_cooperative(&cx, &graph, exact, mem, quantum(1), yield_now)
                .await
                .unwrap();
            compare_execution(&actual, &expected);
            for (resource, limits, memory) in [
                (
                    "estimated work",
                    FnxExecutionLimits {
                        max_estimated_work: exact.max_estimated_work - 1,
                        ..exact
                    },
                    mem,
                ),
                (
                    "kernel workspace bytes",
                    exact,
                    FnxMemoryLimits {
                        max_kernel_workspace_bytes: mem.max_kernel_workspace_bytes - 1,
                        ..mem
                    },
                ),
                (
                    "result rows",
                    FnxExecutionLimits {
                        max_result_rows: 2,
                        ..exact
                    },
                    mem,
                ),
                (
                    "result bytes",
                    exact,
                    FnxMemoryLimits {
                        max_result_bytes: bytes - 1,
                        ..mem
                    },
                ),
            ] {
                let error = call
                    .execute_sealed_cooperative(&cx, &graph, limits, memory, quantum(1), yield_now)
                    .await
                    .unwrap_err();
                assert!(matches!(
                    error,
                    FnxSealedExecutionError::Execution(FnxExecutionError::LimitExceeded {
                        resource: actual, ..
                    }) if actual == resource
                ));
            }
        }
        assert_eq!(contexts.outstanding_obligations(), 0);
    });
}

#[test]
fn topology_empty_and_isolated_populations_yield_during_scalar_and_result_walks() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        let opt = options(Directedness::Undirected);
        for n in [0, 1, 512] {
            let db = small_database(&contexts.commit(), n, false).await;
            let graph = projection(&db, &cx, opt).await;
            for call in [
                FnxCallSpec::triangles(),
                FnxCallSpec::clustering_coefficient(),
            ] {
                let expected = call
                    .execute_sealed(&cx, &graph, opt.execution_limits, memory())
                    .unwrap();
                let probe = Arc::new(SimulationCheckpointProbe::new(None));
                let controlled = cx.with_checkpoint_probe(Arc::clone(&probe));
                let (actual, pending) = drive(
                    call.execute_sealed_cooperative(
                        &controlled,
                        &graph,
                        opt.execution_limits,
                        memory(),
                        quantum(1),
                        yield_now,
                    ),
                    &probe,
                    1,
                );
                let actual = actual.unwrap();
                compare_execution(&actual, &expected);
                assert_eq!(actual.rows.len(), n);
                if n != 0 {
                    assert!(pending >= n * 4, "edge-free scalar walks must cooperate");
                }
            }
        }
        assert_eq!(contexts.outstanding_obligations(), 0);
    });
}

#[test]
fn topology_bitmaps_keep_cross_word_marks_and_exact_counts_across_yields() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let n = 193usize;
        let clique = [0, 1, 63, 64, 65, 127, 128, 191, 192];
        let edges: Vec<_> = clique
            .iter()
            .enumerate()
            .flat_map(|(i, &source)| clique[i + 1..].iter().map(move |&target| (source, target)))
            .chain([
                (2, 62),
                (62, 126),
                (126, 2),
                (63, 129),
                (129, 190),
                (190, 63),
            ])
            .chain([(3, 64), (64, 64), (4, 4)])
            .collect();
        let db = directed_database(&contexts.commit(), n, &edges).await;
        let cx = contexts.query();
        let mut opt = options(Directedness::Undirected);
        opt.projection.self_loops = SelfLoopPolicy::Keep;
        let graph = projection(&db, &cx, opt).await;
        // Enumerate unordered triples directly; no retained-cost orientation,
        // bitset, touched-word list or source-rescan logic enters this oracle.
        let mut adjacent = vec![vec![false; n]; n];
        for &(source, target) in &edges {
            if source != target {
                adjacent[source][target] = true;
                adjacent[target][source] = true;
            }
        }
        let mut counts = vec![0u64; n];
        for a in 0..n {
            for b in a + 1..n {
                for c in b + 1..n {
                    if adjacent[a][b] && adjacent[b][c] && adjacent[c][a] {
                        counts[a] += 1;
                        counts[b] += 1;
                        counts[c] += 1;
                    }
                }
            }
        }
        assert_eq!(counts[63], 29);
        assert_eq!(counts[64], 28);
        assert_eq!(counts[192], 28);
        for call in [
            FnxCallSpec::triangles(),
            FnxCallSpec::clustering_coefficient(),
        ] {
            let coefficients = matches!(call.algorithm(), FnxAlgorithm::ClusteringCoefficient);
            let expected = call
                .execute_sealed(&cx, &graph, opt.execution_limits, memory())
                .unwrap();
            let rows: Vec<_> = (0..n)
                .map(|vertex| {
                    let value = if coefficients {
                        let degree = adjacent[vertex].iter().filter(|&&present| present).count();
                        FnxValue::Score(if degree < 2 {
                            0.0
                        } else {
                            (2 * counts[vertex]) as f64 / (degree * (degree - 1)) as f64
                        })
                    } else {
                        FnxValue::Integer(counts[vertex])
                    };
                    vec![FnxValue::Vertex(directed_identity(vertex, n)), value]
                })
                .collect();
            assert_eq!(expected.rows, rows);
            let counting = n
                * (size_of::<u64>() + size_of::<usize>() * if coefficients { 2 } else { 1 })
                + n.div_ceil(64) * (size_of::<u64>() + size_of::<usize>());
            let conversion = if coefficients {
                n * (size_of::<usize>() + size_of::<u64>() + size_of::<f64>())
            } else {
                0
            };
            assert_eq!(
                expected.certificate.kernel_workspace_bytes,
                counting.max(conversion)
            );
            for q in [1, 7, 65] {
                let probe = Arc::new(SimulationCheckpointProbe::new(None));
                let controlled = cx.with_checkpoint_probe(Arc::clone(&probe));
                let (actual, pending) = drive(
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
                compare_execution(&actual.unwrap(), &expected);
                assert!(pending > 0);
            }
        }
        assert_eq!(contexts.outstanding_obligations(), 0);
    });
}

#[test]
fn topology_sorted_row_bounds_preserve_history_and_cooperative_results() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        let d = 17usize;
        let n = 2 * d;
        for reverse_input in [false, true] {
            let mut edges: Vec<_> = (0..d)
                .flat_map(|source| (d..n).map(move |target| (source, target)))
                .collect();
            if reverse_input {
                edges.reverse();
                for edge in &mut edges {
                    *edge = (edge.1, edge.0);
                }
            }
            let mut db = directed_database(&contexts.commit(), n, &edges).await;
            let original = db.frontier().unwrap();
            for changed in [false, true] {
                if changed {
                    let mut closing = WriteBatch::new(RelationId(1));
                    closing.add_edge(
                        EId((d * d) as u128),
                        directed_identity(0, n),
                        directed_identity(1, n),
                        vec![(PropertyKeyId(1), CanonicalScalar::Int(-1))],
                    );
                    db.write(&contexts.commit(), closing).await.unwrap();
                    for weight in [-2, -3, -4] {
                        let mut history = WriteBatch::new(RelationId(1));
                        history.set_edge_property(
                            EId(0),
                            PropertyKeyId(1),
                            Some(CanonicalScalar::Int(weight)),
                        );
                        db.write(&contexts.commit(), history).await.unwrap();
                    }
                }
                let current = db.frontier().unwrap();
                let cuts = if changed {
                    vec![original, current]
                } else {
                    vec![original]
                };
                for at in cuts {
                    let mut opt = options(Directedness::Undirected);
                    opt.as_of = Some(at);
                    let graph = projection(&db, &cx, opt).await;
                    // Enumerate triples independently of retained history,
                    // cost orientation, bitmaps and the sorted-row cutoff.
                    let mut adjacent = vec![vec![false; n]; n];
                    for &(source, target) in &edges {
                        adjacent[source][target] = true;
                        adjacent[target][source] = true;
                    }
                    if at != original {
                        adjacent[0][1] = true;
                        adjacent[1][0] = true;
                    }
                    let mut counts = vec![0u64; n];
                    for a in 0..n {
                        for b in a + 1..n {
                            for c in b + 1..n {
                                if adjacent[a][b] && adjacent[b][c] && adjacent[c][a] {
                                    counts[a] += 1;
                                    counts[b] += 1;
                                    counts[c] += 1;
                                }
                            }
                        }
                    }
                    assert_eq!(counts[0], if at == original { 0 } else { d as u64 });
                    for call in [
                        FnxCallSpec::triangles(),
                        FnxCallSpec::clustering_coefficient(),
                    ] {
                        let coefficients =
                            matches!(call.algorithm(), FnxAlgorithm::ClusteringCoefficient);
                        let expected = call
                            .execute_sealed(&cx, &graph, opt.execution_limits, memory())
                            .unwrap();
                        let rows: Vec<_> = (0..n)
                            .map(|vertex| {
                                let degree =
                                    adjacent[vertex].iter().filter(|&&present| present).count();
                                let value = if coefficients {
                                    FnxValue::Score(
                                        (2 * counts[vertex]) as f64
                                            / (degree * (degree - 1)) as f64,
                                    )
                                } else {
                                    FnxValue::Integer(counts[vertex])
                                };
                                vec![FnxValue::Vertex(directed_identity(vertex, n)), value]
                            })
                            .collect();
                        assert_eq!(expected.rows, rows);
                        if !changed {
                            // All retained costs are equal before the edits.
                            // Observe the actual closed kernel's visits, not
                            // a separate timing or counting implementation.
                            assert_eq!(expected.certificate.witness.edges_scanned, 3 * d * d);
                        }
                        for q in [1, 7, 65] {
                            let probe = Arc::new(SimulationCheckpointProbe::new(None));
                            let controlled = cx.with_checkpoint_probe(Arc::clone(&probe));
                            let (actual, pending) = drive(
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
                            compare_execution(&actual.unwrap(), &expected);
                            assert!(pending > 0);
                        }
                    }
                }
            }
        }
        assert_eq!(contexts.outstanding_obligations(), 0);
    });
}

fn directed_identity(vertex: usize, n: usize) -> VId {
    VId(u128::MAX - (n - 1 - vertex) as u128)
}

async fn directed_database(cx: &CommitCx, n: usize, edges: &[(usize, usize)]) -> Database<MemVfs> {
    let mut db = Database::<MemVfs>::open_memory(
        cx,
        DatabaseKeys::new(
            [0xd1; 32],
            DatabaseSecurityNamespaceId([0xd2; 32]),
            [0xd3; 32],
        ),
    )
    .await
    .unwrap();
    if n == 0 {
        return db;
    }
    let mut batch = WriteBatch::new(RelationId(1));
    for vertex in 0..n {
        batch.create_vertex(directed_identity(vertex, n), vec![LabelId(1)], vec![]);
    }
    for (id, &(source, target)) in edges.iter().enumerate() {
        batch.add_edge(
            EId(id as u128),
            directed_identity(source, n),
            directed_identity(target, n),
            vec![(PropertyKeyId(1), CanonicalScalar::Int(-1))],
        );
    }
    db.write(cx, batch).await.unwrap();
    db
}

// Enumerate ordered pairs of distinct weak neighbors directly. This deliberately
// has no cost orientation, stamps, two-face merge or triangle enumeration.
fn directed_reference(n: usize, edges: &[(usize, usize)]) -> Vec<Vec<FnxValue>> {
    let mut adjacent = vec![vec![false; n]; n];
    for &(source, target) in edges {
        adjacent[source][target] = true;
    }
    (0..n)
        .map(|source| {
            let neighbors: Vec<_> = (0..n)
                .filter(|&target| {
                    source != target && (adjacent[source][target] || adjacent[target][source])
                })
                .collect();
            let mut count = 0usize;
            for &left in &neighbors {
                for &right in &neighbors {
                    count += usize::from(left != right && adjacent[left][right]);
                }
            }
            let score = if neighbors.len() < 2 {
                0.0
            } else {
                count as f64 / (neighbors.len() * (neighbors.len() - 1)) as f64
            };
            vec![
                FnxValue::Vertex(directed_identity(source, n)),
                FnxValue::Score(score),
            ]
        })
        .collect()
}

#[test]
fn directed_clustering_matches_independent_ordered_pairs_across_topologies_and_quanta() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        let possible = [(0, 1), (0, 2), (1, 0), (1, 2), (2, 0), (2, 1)];
        let call = FnxCallSpec::directed_clustering_coefficient();
        assert!(call.supports_cooperative_sealed_execution());
        for mask in 0u8..64 {
            let mut edges = vec![(0, 0), (2, 2)];
            for (bit, &edge) in possible.iter().enumerate() {
                if mask & (1 << bit) != 0 {
                    // Parallel arcs reduce to one directed adjacency. Reciprocal
                    // arcs remain two different ordered pairs in the oracle.
                    edges.extend([edge, edge]);
                }
            }
            let expected = directed_reference(4, &edges);
            let db = directed_database(&contexts.commit(), 4, &edges).await;
            for direction in [Directedness::Directed, Directedness::Reversed] {
                let mut opt = options(direction);
                opt.projection.parallel_edges = ParallelEdgePolicy::CollapseUnit;
                let graph = projection(&db, &cx, opt).await;
                let sync = call
                    .execute_sealed(&cx, &graph, opt.execution_limits, memory())
                    .unwrap();
                assert_eq!(sync.rows, expected, "mask={mask}, {direction:?}");
                let mut previous = None;
                for q in [1, 7, 257] {
                    let probe = Arc::new(SimulationCheckpointProbe::new(None));
                    let controlled = cx.with_checkpoint_probe(Arc::clone(&probe));
                    let (actual, pending) = drive(
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
                    let actual = actual.unwrap();
                    compare_execution(&actual, &sync);
                    assert_eq!(actual.rows, expected, "mask={mask}, q={q}, {direction:?}");
                    assert!(pending > 0, "reverse construction must offer scheduling");
                    if let Some(previous) = &previous {
                        assert_eq!(&actual, previous);
                    }
                    previous = Some(actual);
                }
            }
        }
        assert_eq!(contexts.outstanding_obligations(), 0);
    });
}

#[test]
fn directed_clustering_preserves_exact_admission_and_empty_or_isolated_graph_laws() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        let edges = [(0, 1), (1, 0), (1, 2), (2, 0), (2, 3)];
        let db = directed_database(&contexts.commit(), 4, &edges).await;
        let call = FnxCallSpec::directed_clustering_coefficient();
        for direction in [Directedness::Directed, Directedness::Reversed] {
            let opt = options(direction);
            let graph = projection(&db, &cx, opt).await;
            let sync = call
                .execute_sealed(&cx, &graph, opt.execution_limits, memory())
                .unwrap();
            let bytes = call.outputs().len() * size_of::<String>()
                + call
                    .outputs()
                    .iter()
                    .map(|column| column.name.len())
                    .sum::<usize>()
                + 4 * (size_of::<Vec<FnxValue>>() + call.outputs().len() * size_of::<FnxValue>());
            let limits = FnxExecutionLimits {
                max_iterations: 0,
                max_result_rows: 4,
                max_estimated_work: sync.certificate.estimated_work,
            };
            let mem = FnxMemoryLimits {
                max_kernel_workspace_bytes: sync.certificate.kernel_workspace_bytes,
                max_result_bytes: bytes,
            };
            let exact = call
                .execute_sealed_cooperative(&cx, &graph, limits, mem, quantum(1), yield_now)
                .await
                .unwrap();
            compare_execution(&exact, &sync);
            // Four vertices cannot trigger the optional ordinal prefix, so a
            // one-byte-short workspace cannot trade that directory for space.
            for (limits, mem) in [
                (
                    FnxExecutionLimits {
                        max_result_rows: 3,
                        ..limits
                    },
                    mem,
                ),
                (
                    FnxExecutionLimits {
                        max_estimated_work: limits.max_estimated_work - 1,
                        ..limits
                    },
                    mem,
                ),
                (
                    limits,
                    FnxMemoryLimits {
                        max_result_bytes: bytes - 1,
                        ..mem
                    },
                ),
                (
                    limits,
                    FnxMemoryLimits {
                        max_kernel_workspace_bytes: mem.max_kernel_workspace_bytes - 1,
                        ..mem
                    },
                ),
            ] {
                let sync = call.execute_sealed(&cx, &graph, limits, mem).unwrap_err();
                let cooperative = call
                    .execute_sealed_cooperative(&cx, &graph, limits, mem, quantum(1), yield_now)
                    .await
                    .unwrap_err();
                assert_eq!(format!("{cooperative:?}"), format!("{sync:?}"));
            }
        }
        for n in [0, 1, 512] {
            let db = directed_database(&contexts.commit(), n, &[]).await;
            let opt = options(Directedness::Directed);
            let graph = projection(&db, &cx, opt).await;
            let mut limits = opt.execution_limits;
            let mut mem = memory();
            if n == 0 {
                limits.max_result_rows = 0;
                limits.max_estimated_work = 0;
                mem.max_kernel_workspace_bytes = 0;
            }
            let expected = call.execute_sealed(&cx, &graph, limits, mem).unwrap();
            let probe = Arc::new(SimulationCheckpointProbe::new(None));
            let controlled = cx.with_checkpoint_probe(Arc::clone(&probe));
            let (actual, pending) = drive(
                call.execute_sealed_cooperative(
                    &controlled,
                    &graph,
                    limits,
                    mem,
                    quantum(1),
                    yield_now,
                ),
                &probe,
                1,
            );
            let actual = actual.unwrap();
            compare_execution(&actual, &expected);
            assert_eq!(actual.rows, directed_reference(n, &[]));
            assert!(
                pending >= 4 * n,
                "scalar and reverse-directory work must yield"
            );
        }
        let opt = options(Directedness::Undirected);
        let graph = projection(&db, &cx, opt).await;
        assert!(matches!(
            call.execute_sealed_cooperative(
                &cx,
                &graph,
                opt.execution_limits,
                memory(),
                quantum(1),
                yield_now
            )
            .await,
            Err(FnxSealedExecutionError::Execution(
                FnxExecutionError::GraphKind { .. }
            ))
        ));
        assert_eq!(contexts.outstanding_obligations(), 0);
    });
}

#[test]
fn directed_clustering_observes_every_live_refusal_and_suspended_drop() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        let db = directed_database(&contexts.commit(), 3, &[(0, 1), (1, 0), (1, 2), (2, 0)]).await;
        let anchor = db.read_session().unwrap().partition_root();
        let call = FnxCallSpec::directed_clustering_coefficient();
        for direction in [Directedness::Directed, Directedness::Reversed] {
            let opt = options(direction);
            let graph = projection(&db, &cx, opt).await;
            let probe = Arc::new(SimulationCheckpointProbe::new(None));
            let controlled = cx.with_checkpoint_probe(Arc::clone(&probe));
            let guards = Cell::new(0usize);
            let (expected, pending) = drive(
                call.execute_sealed_cooperative_with_checkpoint(
                    &controlled,
                    &graph,
                    opt.execution_limits,
                    memory(),
                    quantum(3),
                    yield_now,
                    || {
                        guards.set(guards.get() + 1);
                        Ok(())
                    },
                ),
                &probe,
                3,
            );
            let expected = expected.unwrap();
            for stop in 1..=probe.calls() {
                let refusal = Arc::new(SimulationCheckpointProbe::new(Some(stop)));
                let controlled = cx.with_checkpoint_probe(Arc::clone(&refusal));
                let (actual, _) = drive(
                    call.execute_sealed_cooperative(
                        &controlled,
                        &graph,
                        opt.execution_limits,
                        memory(),
                        quantum(3),
                        yield_now,
                    ),
                    &refusal,
                    3,
                );
                assert!(matches!(actual, Err(FnxSealedExecutionError::Cancelled(_))));
                assert_eq!(refusal.calls(), stop);
            }
            for stop in 1..=guards.get() {
                let mut seen = 0;
                let error = call
                    .execute_sealed_cooperative_with_checkpoint(
                        &cx,
                        &graph,
                        opt.execution_limits,
                        memory(),
                        quantum(3),
                        yield_now,
                        || {
                            seen += 1;
                            if seen == stop {
                                Err(SealedProjectionError::Guard(Box::new(LiveRefusal(stop))))
                            } else {
                                Ok(())
                            }
                        },
                    )
                    .await
                    .unwrap_err();
                live_refusal(error, stop);
                assert_eq!(seen, stop);
            }
            let waker = Waker::from(Arc::new(Wakes::default()));
            let mut task = Context::from_waker(&waker);
            for stop in 0..=pending {
                let revoked = Cell::new(false);
                let mut future = Box::pin(call.execute_sealed_cooperative_with_checkpoint(
                    &cx,
                    &graph,
                    opt.execution_limits,
                    memory(),
                    quantum(3),
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
                    panic!("revocation must stop before resuming a source or counter step");
                };
                live_refusal(result.unwrap_err(), stop);
                drop(future);
                let mut abandoned = Box::pin(call.execute_sealed_cooperative(
                    &cx,
                    &graph,
                    opt.execution_limits,
                    memory(),
                    quantum(3),
                    yield_now,
                ));
                for _ in 0..stop {
                    assert!(abandoned.as_mut().poll(&mut task).is_pending());
                }
                drop(abandoned);
                assert_eq!(contexts.outstanding_obligations(), 0);
                assert_eq!(db.read_session().unwrap().partition_root(), anchor);
            }
            assert_eq!(
                call.execute_sealed_cooperative(
                    &cx,
                    &graph,
                    opt.execution_limits,
                    memory(),
                    quantum(7),
                    yield_now,
                )
                .await
                .unwrap(),
                expected
            );
        }
    });
}

#[test]
fn historical_directed_clustering_hosts_preserve_reductions_and_yield_inside_hidden_hubs() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        let (mut db, original) = database(&contexts.commit()).await;
        let mut batch = WriteBatch::new(RelationId(1));
        batch.add_edge(
            EId(7),
            VId(2),
            VId(0),
            vec![(PropertyKeyId(1), CanonicalScalar::Int(-4))],
        );
        db.write(&contexts.commit(), batch).await.unwrap();
        let current = db.frontier().unwrap();
        let view = db.read_session().unwrap();
        let text =
            "CALL fnx.directed_clustering_coefficient() YIELD score AS coefficient, vertex AS id";
        let call = FnxCallSpec::bind(text, &FnxParameters::new()).unwrap();
        for direction in [Directedness::Directed, Directedness::Reversed] {
            for at in [original, current] {
                for reduction in [
                    ParallelEdgePolicy::Sum,
                    ParallelEdgePolicy::Minimum,
                    ParallelEdgePolicy::Maximum,
                    ParallelEdgePolicy::CollapseUnit,
                ] {
                    for loops in [SelfLoopPolicy::Keep, SelfLoopPolicy::Drop] {
                        let mut opt = options(direction);
                        opt.as_of = Some(at);
                        opt.projection.parallel_edges = reduction;
                        opt.projection.self_loops = loops;
                        let graph = projection(&db, &cx, opt).await;
                        let decoded = view
                            .prism_projection_at(
                                &cx,
                                at,
                                opt.selection,
                                opt.projection,
                                opt.projection_limits,
                                opt.source_limits,
                            )
                            .unwrap();
                        let independent = call
                            .execute(&decoded, opt.execution_limits, || cx.checkpoint())
                            .unwrap();
                        let sync = call
                            .execute_sealed(&cx, &graph, opt.execution_limits, memory())
                            .unwrap();
                        assert_eq!(sync.rows, independent.rows);
                        assert_eq!(
                            sync.certificate.result_digest,
                            independent.certificate.result_digest
                        );
                        assert_eq!(
                            sync.rows[0][0],
                            FnxValue::Score(if at == current { 0.5 } else { 0.0 })
                        );
                        let mut previous = None;
                        for q in [1, 7] {
                            let probe = Arc::new(SimulationCheckpointProbe::new(None));
                            let controlled = cx.with_checkpoint_probe(Arc::clone(&probe));
                            let (actual, pending) = drive(
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
                            let actual = actual.unwrap();
                            compare_execution(&actual, &sync);
                            assert_eq!(actual.certificate.adapter, AdapterPath::CompressedCursor);
                            assert_eq!(actual.columns, vec!["coefficient", "id"]);
                            assert_eq!(actual.rows.len(), 4);
                            assert!(
                                actual
                                    .rows
                                    .iter()
                                    .all(|row| row[1] != FnxValue::Vertex(VId(99)))
                            );
                            if q == 1 {
                                assert!(
                                    pending > 700,
                                    "hidden history and both-face scans must yield"
                                );
                            }
                            if let Some(previous) = &previous {
                                assert_eq!(&actual, previous);
                            }
                            previous = Some(actual);
                        }
                        let hosted = db
                            .call_fnx_sealed_cooperative(
                                &cx,
                                text,
                                &FnxParameters::new(),
                                opt,
                                memory(),
                                SealedLimits::default(),
                                quantum(7),
                            )
                            .await
                            .unwrap();
                        assert_eq!(Some(hosted.analytics), previous);
                        assert_eq!(hosted.selection, opt.selection);
                    }
                }
            }
        }
        assert_eq!(contexts.outstanding_obligations(), 0);
    });
}
