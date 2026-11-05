//! Complete decoded CALLs and the actual compressed-row/encoding drivers.
//! The standalone state tests also exhaust every four-vertex digraph; these
//! tests cover owner admission, stable identities, fallibility and delivery.

use super::*;
use crate::{
    GraphView, ParallelEdgePolicy, ProjectionEdge, ProjectionLimits, ProjectionSpec,
    SelfLoopPolicy, SnapshotBinding, SnapshotGraphView,
};
use fgdb_types::{CommitSeq, EId, VId, ids::ObjectId};
use std::cell::Cell;

fn limits(rows: usize) -> FnxExecutionLimits {
    FnxExecutionLimits {
        max_iterations: 0,
        max_result_rows: rows,
        max_estimated_work: usize::MAX,
    }
}

fn memory() -> FnxMemoryLimits {
    FnxMemoryLimits {
        max_kernel_workspace_bytes: usize::MAX,
        max_result_bytes: usize::MAX,
    }
}

fn projection(
    vertices: &[VId],
    pairs: &[(usize, usize)],
    direction: Directedness,
) -> SnapshotGraphView {
    let edges: Vec<_> = pairs
        .iter()
        .enumerate()
        .map(|(index, &(source, target))| ProjectionEdge {
            eid: EId(index as u128),
            source: vertices[source],
            target: vertices[target],
            weight: -(index as f64 + 1.0), // unweighted traversal ignores signed finite weights
        })
        .collect();
    SnapshotGraphView::build(
        SnapshotBinding {
            root: ObjectId([0x6d; 32]),
            as_of: CommitSeq(9),
        },
        vertices,
        &edges,
        ProjectionSpec {
            directedness: direction,
            parallel_edges: ParallelEdgePolicy::Sum,
            self_loops: SelfLoopPolicy::Keep,
        },
        ProjectionLimits {
            max_vertices: vertices.len(),
            max_input_edges: edges.len(),
            max_adjacency_entries: 2 * edges.len(),
            max_workspace_bytes: 1 << 28,
        },
    )
    .unwrap()
}

struct Fixture {
    population: usize,
    source: usize,
    row: Vec<usize>,
    canonical: bool,
    calls: Cell<usize>,
    fail_at: Cell<usize>,
    corrupt_at: Cell<Option<(usize, usize)>>,
}

impl Fixture {
    fn new(population: usize, source: usize, row: Vec<usize>, canonical: bool) -> Self {
        assert!(source < population);
        assert!(row.iter().all(|&target| target < population));
        if canonical {
            assert!(row.windows(2).all(|pair| pair[0] < pair[1]));
        }
        Self {
            population,
            source,
            row,
            canonical,
            calls: Cell::new(0),
            fail_at: Cell::new(usize::MAX),
            corrupt_at: Cell::new(None),
        }
    }

    fn reset(&self, fail_at: usize) {
        self.calls.set(0);
        self.fail_at.set(fail_at);
    }

    fn step(&self) -> Result<usize> {
        let at = self.calls.get() + 1;
        self.calls.set(at);
        if at == self.fail_at.get() {
            Err(SealedProjectionError::UnknownOrdinal(at).into())
        } else {
            Ok(at)
        }
    }
}

struct Row<'a> {
    owner: &'a Fixture,
    targets: std::slice::Iter<'a, usize>,
}

impl Cursor for Row<'_> {
    fn next(&mut self) -> Result<Option<(usize, f64)>> {
        let at = self.owner.step()?;
        Ok(self.targets.next().map(|&target| {
            let target = self
                .owner
                .corrupt_at
                .get()
                .filter(|&(stop, _)| stop == at)
                .map_or(target, |(_, target)| target);
            (target, -17.0)
        }))
    }
}

impl Rows for Fixture {
    type Cursor<'a> = Row<'a>;
    fn node_count(&self) -> usize {
        self.population
    }
    fn degree(&self, source: usize) -> Option<usize> {
        (source < self.population).then_some(if source == self.source {
            self.row.len()
        } else {
            0
        })
    }
    fn has_validated_adjacency(&self) -> bool {
        true
    }
    fn has_canonical_adjacency(&self) -> bool {
        self.canonical
    }
    fn open(&self, source: usize) -> Result<Self::Cursor<'_>> {
        assert_eq!(
            source, self.source,
            "a shallow query must only open its source row"
        );
        self.step()?;
        Ok(Row {
            owner: self,
            targets: self.row.iter(),
        })
    }
}

#[test]
fn shallow_bfs_all_small_directions_sources_and_encoders_match_independent_paths() {
    let vertices = [VId(0), VId(17), VId(u128::MAX)];
    for mask in 0_u16..512 {
        let pairs: Vec<_> = (0..9)
            .filter(|bit| mask & (1 << bit) != 0)
            .map(|bit| (bit / 3, bit % 3))
            .collect();
        for direction in [
            Directedness::Directed,
            Directedness::Reversed,
            Directedness::Undirected,
        ] {
            // Independent dense all-pairs closure over the raw input mask.
            let mut hops = [[4usize; 3]; 3];
            for (vertex, row) in hops.iter_mut().enumerate() {
                row[vertex] = 0;
            }
            for &(a, b) in &pairs {
                if direction != Directedness::Reversed {
                    hops[a][b] = hops[a][b].min(1);
                }
                if direction != Directedness::Directed {
                    hops[b][a] = hops[b][a].min(1);
                }
            }
            for via in 0..3 {
                for a in 0..3 {
                    for b in 0..3 {
                        hops[a][b] = hops[a][b].min(hops[a][via] + hops[via][b]);
                    }
                }
            }
            let graph = projection(&vertices, &pairs, direction);
            for source in 0..3 {
                for cutoff in [0, 1] {
                    let call = FnxCallSpec::single_source_shortest_path_length(
                        vertices[source],
                        Some(cutoff),
                    );
                    let expected: Vec<_> = (0..3)
                        .filter(|&target| hops[source][target] <= cutoff)
                        .map(|target| {
                            vec![
                                FnxValue::Vertex(vertices[target]),
                                FnxValue::Integer(hops[source][target] as u64),
                            ]
                        })
                        .collect();
                    let actual = call
                        .execute(&graph, limits(3), || Ok::<(), Infallible>(()))
                        .unwrap();
                    assert_eq!(
                        actual.rows, expected,
                        "mask={mask} {direction:?} source={source} cutoff={cutoff}"
                    );
                    let incumbent = shallow_bfs::with_dense(|| {
                        call.execute(&graph, limits(3), || Ok::<(), Infallible>(()))
                            .unwrap()
                    });
                    assert_eq!(actual.rows, incumbent.rows);
                    assert_eq!(
                        actual.certificate.result_digest,
                        incumbent.certificate.result_digest
                    );
                    assert_ne!(
                        actual.certificate.execution_kernel,
                        incumbent.certificate.execution_kernel
                    );
                    assert_eq!(actual.certificate.witness.nodes_touched, 1);
                    let fixture = Fixture::new(
                        3,
                        source,
                        graph.neighbors_indices(source).unwrap().to_vec(),
                        true,
                    );
                    for delivery in [Delivery::Collected, Delivery::Retained] {
                        fixture.reset(usize::MAX);
                        let admission =
                            ResultAdmission::for_delivery(&call, limits(3), memory(), delivery)
                                .unwrap();
                        let (output, bytes) = shallow_bfs_run(
                            &fixture,
                            source,
                            Some(cutoff),
                            &admission,
                            usize::MAX,
                            &mut || Ok(()),
                        )
                        .unwrap();
                        assert_eq!(output.row_count, expected.len());
                        assert_eq!(output.values.scan_len(usize::MAX), expected.len());
                        assert_eq!(bytes, actual.certificate.kernel_workspace_bytes);
                        let mut encoded =
                            EncodedRows::new(&call, output.row_count, &admission).unwrap();
                        for index in 0..output.values.scan_len(3) {
                            encoded
                                .push_vertices(&call, &vertices, &output.values, index, &mut || {
                                    Ok(())
                                })
                                .unwrap();
                        }
                        assert_eq!(encoded.emitted, expected.len());
                        assert_eq!(
                            encoded.result_hash.finalize(),
                            actual.certificate.result_digest
                        );
                        match delivery {
                            Delivery::Collected => {
                                assert_eq!(encoded.rows.as_ref(), Some(&expected))
                            }
                            Delivery::Retained => assert!(encoded.rows.is_none()),
                        }
                        assert_eq!(
                            fixture.calls.get(),
                            if cutoff == 0 {
                                0
                            } else {
                                fixture.row.len() + 2
                            }
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn shallow_bfs_each_finite_work_model_survives_overflow_without_masking_other_errors() {
    for (local, prior, expected) in [
        (Ok(7), Some(19), 7),
        (Ok(19), Some(7), 7),
        (Ok(7), None, 7),
        (Err(shallow_bfs::Error::SizeOverflow), Some(7), 7),
    ] {
        assert_eq!(
            crate::execute::shallow_admission_work::<Infallible>(local, prior).unwrap(),
            expected
        );
    }
    assert!(matches!(
        crate::execute::shallow_admission_work::<Infallible>(
            Err(shallow_bfs::Error::SizeOverflow),
            None
        ),
        Err(ExecutionError::SizeOverflow)
    ));
    assert!(matches!(
        crate::execute::shallow_admission_work::<Infallible>(
            Err(shallow_bfs::Error::InvalidRow),
            Some(7)
        ),
        Err(ExecutionError::InvalidUpstreamResult)
    ));
    assert_eq!(shallow_bfs_admission_work(Ok(7), Ok(19)).unwrap(), 7);
    assert_eq!(shallow_bfs_admission_work(Ok(19), Ok(7)).unwrap(), 7);
    for reverse in [false, true] {
        let overflow = Err(ExecutionError::SizeOverflow.into());
        let (a, b) = if reverse {
            (overflow, Ok(7))
        } else {
            (Ok(7), overflow)
        };
        assert_eq!(shallow_bfs_admission_work(a, b).unwrap(), 7);
        let malformed = Err(ExecutionError::InvalidUpstreamResult.into());
        let (a, b) = if reverse {
            (malformed, Ok(7))
        } else {
            (Ok(7), malformed)
        };
        assert!(matches!(
            shallow_bfs_admission_work(a, b),
            Err(Error::Execution(ExecutionError::InvalidUpstreamResult))
        ));
        let source = Err(SealedProjectionError::UnknownOrdinal(17).into());
        let overflow = Err(ExecutionError::SizeOverflow.into());
        let (a, b) = if reverse {
            (source, overflow)
        } else {
            (overflow, source)
        };
        assert!(matches!(
            shallow_bfs_admission_work(a, b),
            Err(Error::Source(SealedProjectionError::UnknownOrdinal(17)))
        ));
    }
    assert!(matches!(
        shallow_bfs_admission_work(
            Err(ExecutionError::SizeOverflow.into()),
            Err(ExecutionError::SizeOverflow.into())
        ),
        Err(Error::Execution(ExecutionError::SizeOverflow))
    ));
}

#[test]
fn shallow_bfs_stars_and_single_vertices_keep_exact_incumbent_work_allowances() {
    for n in [1, 2, 129] {
        let vertices: Vec<_> = (0..n)
            .map(|index| VId((index as u128) << 64 | 17))
            .collect();
        for loops in [false, true] {
            let pairs: Vec<_> = (0..n)
                .filter(|&target| target != 0 || loops)
                .map(|target| (0, target))
                .collect();
            let graph = projection(&vertices, &pairs, Directedness::Directed);
            let legacy = n + pairs.len();
            let exact = FnxExecutionLimits {
                max_estimated_work: legacy,
                ..limits(n)
            };
            for cutoff in [0, 1] {
                for fields in [
                    "vertex,distance",
                    "distance AS hops,vertex AS id",
                    "vertex",
                    "distance",
                ] {
                    let call = FnxCallSpec::bind(&format!("CALL fnx.single_source_shortest_path_length({}, {cutoff}) YIELD {fields}", vertices[0].0), &FnxParameters::new()).unwrap();
                    let incumbent = shallow_bfs::with_dense(|| {
                        call.execute(&graph, exact, || Ok::<(), Infallible>(()))
                            .unwrap()
                    });
                    assert_eq!(incumbent.certificate.estimated_work, legacy);
                    let actual = call
                        .execute(&graph, exact, || Ok::<(), Infallible>(()))
                        .unwrap();
                    assert_eq!(actual.rows, incumbent.rows);
                    assert_eq!(
                        actual.certificate.result_digest,
                        incumbent.certificate.result_digest
                    );
                    assert!(actual.certificate.estimated_work <= legacy);
                    let denied = FnxExecutionLimits {
                        max_estimated_work: actual.certificate.estimated_work - 1,
                        ..exact
                    };
                    assert!(matches!(
                        call.execute(&graph, denied, || Ok::<(), Infallible>(())),
                        Err(ExecutionError::LimitExceeded {
                            resource: "estimated work",
                            ..
                        })
                    ));
                }
            }
        }
    }
}

#[test]
fn shallow_bfs_large_population_limits_and_all_delivery_paths_visit_only_the_result() {
    let n = 100_003;
    let source = n / 2;
    let vertices: Vec<_> = (0..n)
        .map(|index| VId((index as u128) << 64 | 17))
        .collect();
    let graph = projection(
        &vertices,
        &[(source, 1), (source, source), (source, n - 1), (source, 1)],
        Directedness::Directed,
    );
    for cutoff in [0, 1] {
        for fields in [
            "vertex,distance",
            "distance AS hops,vertex AS id",
            "vertex",
            "distance",
        ] {
            let call = FnxCallSpec::bind(
                &format!(
                    "CALL fnx.single_source_shortest_path_length({}, {cutoff}) YIELD {fields}",
                    vertices[source].0
                ),
                &FnxParameters::new(),
            )
            .unwrap();
            let reached = if cutoff == 0 { 1 } else { 3 };
            let bounded = FnxExecutionLimits {
                max_estimated_work: 1000,
                ..limits(reached)
            };
            let checks = Cell::new(0);
            let actual = call
                .execute(&graph, bounded, || {
                    checks.set(checks.get() + 1);
                    Ok::<(), usize>(())
                })
                .unwrap();
            let expected = shallow_bfs::with_dense(|| {
                call.execute(&graph, limits(n), || Ok::<(), usize>(()))
                    .unwrap()
            });
            assert_eq!(actual.rows, expected.rows);
            assert_eq!(
                actual.certificate.result_digest,
                expected.certificate.result_digest
            );
            assert!(actual.certificate.estimated_work < 1000);
            assert_eq!(
                actual.certificate.kernel_workspace_bytes,
                (reached - 1) * size_of::<usize>()
            );
            assert_eq!(checks.get(), reached + if cutoff == 0 { 0 } else { 3 } + 5);
            let mut mapped = Vec::new();
            assert_eq!(
                call.execute_into(&graph, bounded, || Ok::<(), usize>(()), &mut mapped)
                    .unwrap(),
                actual.certificate
            );
            assert_eq!(mapped, actual.rows);
            let mut cursor = call
                .execute_rows(&graph, bounded, || Ok::<(), usize>(()))
                .unwrap();
            assert_eq!(cursor.certificate(), &actual.certificate);
            assert_eq!(cursor.remaining(), reached);
            assert_eq!(
                cursor.max_integer(),
                actual
                    .rows
                    .iter()
                    .flatten()
                    .filter_map(|value| match value {
                        FnxValue::Integer(value) => Some(*value),
                        _ => None,
                    })
                    .max()
            );
            let mut row = Vec::new();
            let mut pulls = 0;
            for (index, expected) in actual.rows.iter().enumerate() {
                assert!(matches!(
                    cursor.next_row(&mut row, || Err(17)),
                    Err(FnxExecutionError::Cancelled(17))
                ));
                assert_eq!(cursor.remaining(), reached - index);
                assert!(row.is_empty());
                assert!(
                    cursor
                        .next_row(&mut row, || {
                            pulls += 1;
                            Ok::<(), usize>(())
                        })
                        .unwrap()
                );
                assert_eq!(&row, expected);
            }
            assert!(
                !cursor
                    .next_row(&mut row, || -> std::result::Result<(), usize> {
                        panic!("completed sparse output has no population tail")
                    })
                    .unwrap()
            );
            assert_eq!(pulls, reached);
            assert_eq!(cursor.remaining(), 0);
            assert!(row.is_empty());
            for stop in 1..=checks.get() {
                let observed = Cell::new(0);
                assert!(matches!(call.execute_rows(&graph, bounded, || {
                    observed.set(observed.get() + 1);
                    if observed.get() == stop { Err(stop) } else { Ok(()) }
                }), Err(FnxExecutionError::Cancelled(at)) if at == stop));
                assert_eq!(observed.get(), stop);
            }
            let denied = FnxExecutionLimits {
                max_estimated_work: actual.certificate.estimated_work - 1,
                ..bounded
            };
            assert!(matches!(
                call.execute(&graph, denied, || Ok::<(), usize>(())),
                Err(FnxExecutionError::LimitExceeded {
                    resource: "estimated work",
                    ..
                })
            ));
        }
    }
    let missing = FnxCallSpec::single_source_shortest_path_length(VId(u128::MAX), Some(0));
    for cap in [0, 1, n] {
        for work in [0, 1, usize::MAX] {
            let limits = FnxExecutionLimits {
                max_estimated_work: work,
                ..limits(cap)
            };
            let current = format!(
                "{:?}",
                missing.execute(&graph, limits, || Ok::<(), Infallible>(()))
            );
            let prior = shallow_bfs::with_dense(|| {
                format!(
                    "{:?}",
                    missing.execute(&graph, limits, || Ok::<(), Infallible>(()))
                )
            });
            assert_eq!(current, prior, "unknown-source refusal precedence");
        }
    }
}

#[test]
fn shallow_bfs_drains_admitted_source_eof_and_preserves_every_fault_and_cut() {
    // All three vertices are reached before the final source-loop incidence.
    let fixture = Fixture::new(3, 2, vec![0, 1, 2], true);
    let call = FnxCallSpec::single_source_shortest_path_length(VId(2), Some(1));
    let admission = ResultAdmission::new(&call, limits(3), memory()).unwrap();
    let mut checks = 0;
    let (output, bytes) = shallow_bfs_run(
        &fixture,
        2,
        Some(1),
        &admission,
        2 * size_of::<usize>(),
        &mut || {
            checks += 1;
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(output.row_count, 3);
    assert_eq!(bytes, 2 * size_of::<usize>());
    assert_eq!(output.witness.edges_scanned, 3);
    assert_eq!(
        fixture.calls.get(),
        5,
        "open, each incidence, and fallible EOF"
    );
    for stop in 1..=5 {
        fixture.reset(stop);
        assert!(
            matches!(shallow_bfs_run(&fixture, 2, Some(1), &admission, usize::MAX, &mut || Ok(())), Err(Error::Source(SealedProjectionError::UnknownOrdinal(at))) if at == stop)
        );
        assert_eq!(fixture.calls.get(), stop);
    }
    for stop in 1..=checks {
        fixture.reset(usize::MAX);
        let mut seen = 0;
        assert!(
            matches!(shallow_bfs_run(&fixture, 2, Some(1), &admission, usize::MAX, &mut || {
            seen += 1;
            if seen == stop { Err(SealedProjectionError::UnknownOrdinal(stop).into()) } else { Ok(()) }
        }), Err(Error::Source(SealedProjectionError::UnknownOrdinal(at))) if at == stop)
        );
        assert_eq!(seen, stop);
    }
    for bad_target in [0, 3, usize::MAX] {
        fixture.reset(usize::MAX);
        fixture.corrupt_at.set(Some((4, bad_target)));
        assert!(matches!(
            shallow_bfs_run(&fixture, 2, Some(1), &admission, usize::MAX, &mut || Ok(())),
            Err(Error::Execution(ExecutionError::InvalidUpstreamResult))
        ));
        assert_eq!(
            fixture.calls.get(),
            4,
            "malformed final incidence cannot disappear after full discovery"
        );
    }
    fixture.corrupt_at.set(None);
    fixture.reset(1);
    let (output, bytes) =
        shallow_bfs_run(&fixture, 2, Some(0), &admission, 0, &mut || Ok(())).unwrap();
    assert_eq!(output.row_count, 1);
    assert_eq!(bytes, 0);
    assert_eq!(fixture.calls.get(), 0, "cutoff zero never opens a row");
    let empty = Fixture::new(3, 2, vec![], true);
    empty.reset(2);
    assert!(matches!(
        shallow_bfs_run(&empty, 2, Some(1), &admission, 0, &mut || Ok(())),
        Err(Error::Source(SealedProjectionError::UnknownOrdinal(2)))
    ));
}

#[test]
fn shallow_bfs_bounds_only_rows_keep_general_order_duplicates_and_exact_fallback() {
    let fixture = Fixture::new(4, 2, vec![3, 1, 3, 2, 0], false);
    let call = FnxCallSpec::single_source_shortest_path_length(VId(2), Some(1));
    for cap in 0..=4 {
        let admission = ResultAdmission::new(&call, limits(cap), memory()).unwrap();
        for stop in 1..=7 {
            fixture.reset(stop);
            let mut prior_checks = 0;
            let prior = bfs(&fixture, 2, Some(1), &admission, &mut || {
                prior_checks += 1;
                Ok(())
            });
            let prior_pulls = fixture.calls.get();
            fixture.reset(stop);
            let mut checks = 0;
            let actual = shallow_bfs_run(&fixture, 2, Some(1), &admission, usize::MAX, &mut || {
                checks += 1;
                Ok(())
            });
            assert_eq!(checks, prior_checks);
            assert_eq!(fixture.calls.get(), prior_pulls);
            match (actual, prior) {
                (Ok((actual, bytes)), Ok(prior)) => {
                    assert_eq!(bytes, 8 * size_of::<usize>());
                    assert_eq!(actual.row_count, prior.row_count);
                    assert_eq!(actual.witness, prior.witness);
                    let (KernelValues::Distances(actual), KernelValues::Distances(prior)) =
                        (actual.values, prior.values)
                    else {
                        panic!("general provider must retain dense fallback");
                    };
                    assert_eq!(actual, prior);
                }
                (Err(actual), Err(prior)) => {
                    assert_eq!(format!("{actual:?}"), format!("{prior:?}"))
                }
                _ => panic!("fallback success/refusal changed"),
            }
        }
    }
}

#[test]
fn shallow_bfs_result_and_workspace_caps_refuse_before_retention_at_the_true_prefix() {
    let fixture = Fixture::new(4, 2, vec![0, 2, 3], true);
    let call = FnxCallSpec::single_source_shortest_path_length(VId(2), Some(1));
    let shape = ResultAdmission::new(&call, limits(4), memory()).unwrap();
    for row_cap in 0..=3 {
        for byte_cap in 0..=3 {
            fixture.reset(usize::MAX);
            let memory = FnxMemoryLimits {
                max_result_bytes: shape.column_bytes + byte_cap * shape.row_bytes,
                ..memory()
            };
            let admission = ResultAdmission::new(&call, limits(row_cap), memory).unwrap();
            let result =
                shallow_bfs_run(&fixture, 2, Some(1), &admission, usize::MAX, &mut || Ok(()));
            let cap = row_cap.min(byte_cap);
            if cap == 3 {
                let (output, bytes) = result.unwrap();
                assert_eq!(output.row_count, 3);
                assert_eq!(bytes, 2 * size_of::<usize>());
                assert_eq!(fixture.calls.get(), 5);
            } else {
                let expected = if row_cap <= byte_cap {
                    "result rows"
                } else {
                    "result bytes"
                };
                assert!(
                    matches!(result, Err(Error::Execution(ExecutionError::LimitExceeded { resource, .. })) if resource == expected)
                );
                assert_eq!(
                    fixture.calls.get(),
                    match cap {
                        0 => 0,
                        1 => 2,
                        _ => 4,
                    }
                );
            }
        }
    }
    fixture.reset(usize::MAX);
    let admission = ResultAdmission::new(&call, limits(3), memory()).unwrap();
    assert!(matches!(
        shallow_bfs_run(
            &fixture,
            2,
            Some(1),
            &admission,
            2 * size_of::<usize>() - 1,
            &mut || Ok(())
        ),
        Err(Error::Execution(ExecutionError::LimitExceeded {
            resource: "kernel workspace bytes",
            ..
        }))
    ));
    assert_eq!(
        fixture.calls.get(),
        0,
        "workspace refusal precedes opening the source"
    );
}

/// Same-process complete warm CALL (including result/certificate creation,
/// retained consumption and cleanup); projection construction is untimed.
#[test]
#[ignore = "manual paired complete warm CALL; no timing pass threshold"]
fn measure_shallow_bfs_complete_call_against_live_dense_incumbent() {
    use std::hint::black_box;
    use std::time::Instant;
    let n = 200_003;
    let source = n / 2;
    let vertices: Vec<_> = (0..n).map(|index| VId((index as u128) << 64 | 7)).collect();
    let graph = projection(
        &vertices,
        &[(source, 1), (source, source), (source, n - 1)],
        Directedness::Directed,
    );
    for cutoff in [0, 1] {
        let call = FnxCallSpec::single_source_shortest_path_length(vertices[source], Some(cutoff));
        let sample = |dense| {
            let run = || {
                let start = Instant::now();
                let mut cursor = call
                    .execute_rows(black_box(&graph), limits(n), || Ok::<(), Infallible>(()))
                    .unwrap();
                let digest = black_box(cursor.certificate().result_digest);
                black_box(cursor.certificate().digest);
                let workspace = cursor.certificate().kernel_workspace_bytes;
                let mut hash = Hasher::new();
                let mut row = Vec::new();
                let mut count = 0;
                while cursor
                    .next_row(&mut row, || Ok::<(), Infallible>(()))
                    .unwrap()
                {
                    for &value in &row {
                        hash_result_value(&mut hash, black_box(value));
                    }
                    count += 1;
                }
                let consumed = black_box(hash.finalize());
                drop(cursor);
                drop(row);
                (start.elapsed(), digest, consumed, workspace, count)
            };
            if dense {
                shallow_bfs::with_dense(run)
            } else {
                run()
            }
        };
        let prior = sample(true);
        let current = sample(false);
        assert_eq!(
            (prior.1, prior.2, prior.4),
            (current.1, current.2, current.4)
        );
        let mut old = Vec::new();
        let mut new = Vec::new();
        for pair in 0..7 {
            let (a, b) = if pair % 2 == 0 {
                (sample(true), sample(false))
            } else {
                let b = sample(false);
                (sample(true), b)
            };
            assert_eq!((a.1, a.2, a.4), (b.1, b.2, b.4));
            old.push(a.0);
            new.push(b.0);
        }
        old.sort_unstable();
        new.sort_unstable();
        println!(
            "scope=warm_decoded_call_to_consumed_retained_result vertices={n} cutoff={cutoff} rows={} pairs=7 alternating_order=true prior_median_us={} sparse_median_us={} prior_workspace={} sparse_workspace={} timing_gate=false projection_construction_excluded=true debug_assertions={}",
            current.4,
            old[3].as_micros(),
            new[3].as_micros(),
            prior.3,
            current.3,
            cfg!(debug_assertions)
        );
    }
}
