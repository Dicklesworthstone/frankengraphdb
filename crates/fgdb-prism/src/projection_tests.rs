//! Admission, cancellation and semantic laws for BOTH projection entrypoints.
use super::*;
use std::cell::Cell;
use std::collections::BTreeMap;
use std::convert::Infallible;

thread_local! {
    static INCUMBENT_DIRECTORY: Cell<bool> = const { Cell::new(false) };
}

pub(super) fn incumbent_directory() -> bool {
    INCUMBENT_DIRECTORY.with(Cell::get)
}

fn with_incumbent_directory<T>(enabled: bool, work: impl FnOnce() -> T) -> T {
    struct Reset(bool);
    impl Drop for Reset {
        fn drop(&mut self) {
            INCUMBENT_DIRECTORY.with(|mode| mode.set(self.0));
        }
    }
    let _reset = Reset(INCUMBENT_DIRECTORY.with(|mode| mode.replace(enabled)));
    work()
}

fn permuted_identity(mut value: u128) -> u128 {
    // A reversible full-width permutation for fixtures, not an engine ID
    // generator. Every operation is bijective over the entire u128 domain.
    for factor in [
        0x9e37_79b9_7f4a_7c15_d1b5_4a32_d192_ed03,
        0xda94_2042_e4dd_58b5_a24b_aed4_963e_e407,
        0x94d0_49bb_1331_11eb_bf58_476d_1ce4_e5b9,
    ] {
        value =
            value.wrapping_mul(factor).rotate_left(47) ^ 0xdeca_fbad_1234_5678_9abc_def0_1024_6813;
    }
    value
}

fn directory_vertices(n: usize, shape: usize) -> Vec<VId> {
    (0..n)
        .map(|index| {
            let value = index as u128;
            VId(match shape {
                0 => permuted_identity(value),
                // The engine's four-round 63-bit Feistel shape, using a
                // fixed public fixture key and no database key material.
                1 => {
                    let (mut left, mut right) = ((index as u64 >> 32) as u32, index as u32);
                    for round in 0u8..4 {
                        let width = if round.is_multiple_of(2) { 31 } else { 32 };
                        let mut input = [0u8; 5];
                        input[0] = round;
                        input[1..].copy_from_slice(&right.to_be_bytes());
                        let mut hash = Hasher::new_keyed(&[19; 32]);
                        hash.update(&input);
                        let digest = hash.finalize();
                        let f = u32::from_be_bytes(digest.0[..4].try_into().unwrap())
                            & (u32::MAX >> (32 - width));
                        (left, right) = (right, left ^ f);
                    }
                    (u128::from(left) << 32) | u128::from(right)
                }
                2 => (0xcafeu128 << 112) | (value * 257),
                3 => (value << 64) | 17,
                // One outlier makes almost every vertex share one bucket.
                4 if index + 1 == n => u128::MAX,
                _ => value,
            })
        })
        .collect()
}

#[test]
fn prefix_directory_matches_full_width_search_across_thresholds_and_empty_buckets() {
    for n in [0, 1, 2, 31, 511, 512, 513, 1024, 4096] {
        for shape in 0..5 {
            let mut vertices = directory_vertices(n, shape);
            vertices.sort_unstable();
            let mut directory = SortedVertexLookup::new(&vertices);
            for index in 1..n {
                directory.include(index);
            }
            directory.finish();
            let borrowed = SortedVertexLookup::from_sorted(&vertices);
            assert_eq!(directory.prefix.is_some(), n >= INDEXED_DIRECTORY_MIN);
            assert_eq!(borrowed.prefix.is_some(), directory.prefix.is_some());
            if let Some(prefix) = &directory.prefix {
                assert_eq!(prefix.boundaries[0], 0);
                assert_eq!(prefix.boundaries[256], n);
                assert!(prefix.boundaries.windows(2).all(|pair| pair[0] <= pair[1]));
                let borrowed_prefix = borrowed.prefix.as_ref().unwrap();
                assert_eq!(borrowed_prefix.shift, prefix.shift);
                assert_eq!(borrowed_prefix.boundaries, prefix.boundaries);
            }
            let mut queries = vec![VId(0), VId(u128::MAX)];
            for &vertex in &vertices {
                queries.extend([
                    vertex,
                    VId(vertex.0.wrapping_sub(1)),
                    VId(vertex.0.wrapping_add(1)),
                ]);
            }
            if let Some(first) = vertices.first() {
                queries.extend((0..128).map(|bit| VId(first.0 ^ (1u128 << bit))));
            }
            for query in queries {
                let eid = EId((1u128 << 127) | 31);
                assert_eq!(
                    directory.checked_ordinal(eid, query),
                    ordinal(&vertices, eid, query),
                    "n={n} shape={shape} query={query:?}"
                );
                assert_eq!(
                    borrowed.ordinal(query),
                    vertices.binary_search(&query).ok(),
                    "borrowed n={n} shape={shape} query={query:?}"
                );
                assert_eq!(
                    borrowed.contains(query),
                    vertices.binary_search(&query).is_ok()
                );
            }
        }
    }
}

#[test]
fn sorted_vertex_lookup_accepts_duplicates_and_rejects_absent_full_width_ids() {
    for n in [0, 1, 2, 31, 511, 512, 513, 1024] {
        for shape in 0..5 {
            let population = directory_vertices(n, shape);
            let mut vertices: Vec<_> = (0..n).map(|index| population[index / 3]).collect();
            vertices.sort_unstable();
            for same_identity in [false, true] {
                if same_identity {
                    vertices.fill(VId(0xfedc_ba98_7654_3210_1234_5678_9abc_def0));
                }
                let lookup = SortedVertexLookup::from_sorted(&vertices);
                let mut queries = vec![VId(0), VId(u128::MAX)];
                for &vertex in &vertices {
                    queries.extend([
                        vertex,
                        VId(vertex.0.wrapping_sub(1)),
                        VId(vertex.0.wrapping_add(1)),
                    ]);
                }
                if let Some(first) = vertices.first() {
                    queries.extend((0..128).map(|bit| VId(first.0 ^ (1u128 << bit))));
                }
                for query in queries {
                    let expected = vertices.binary_search(&query).is_ok();
                    assert_eq!(lookup.contains(query), expected);
                    let actual = lookup.ordinal(query);
                    assert_eq!(actual.is_some(), expected);
                    if let Some(index) = actual {
                        assert_eq!(vertices[index], query);
                    }
                }
            }
        }
    }
}

#[test]
fn retained_directory_is_optional_and_charged_at_capacity() {
    let bytes = size_of::<OrdinalPrefix>();
    for n in [0, 1, 511, 512, 513, 4096] {
        let vertices: Vec<_> = (0..n).map(|index| VId(index as u128)).collect();
        for budget in [0, bytes - 1, bytes, bytes * 2, usize::MAX] {
            let retained = OrdinalPrefix::reserve_retained(&vertices, budget);
            assert_eq!(
                retained.len(),
                usize::from(n >= INDEXED_DIRECTORY_MIN && budget >= bytes),
                "n={n} budget={budget}"
            );
            assert!(retained.capacity() <= budget / bytes);
            if retained.is_empty() {
                assert_eq!(retained.capacity(), 0);
            }
        }
        with_incumbent_directory(true, || {
            let retained = OrdinalPrefix::reserve_retained(&vertices, usize::MAX);
            assert!(retained.is_empty());
            assert_eq!(retained.capacity(), 0);
        });
    }
}

#[test]
fn retained_directory_reuses_full_width_bounds_after_population_moves() {
    for n in [512, 513, 4096] {
        for shape in 0..5 {
            let mut vertices = directory_vertices(n, shape);
            vertices.sort_unstable();
            let mut retained =
                OrdinalPrefix::reserve_retained(&vertices, size_of::<OrdinalPrefix>());
            // The metadata retains no borrowed slice. Owned projections may
            // move their population and share this single directory with rows.
            let owned = vertices.clone();
            drop(vertices);
            let prefix = retained.first_mut().unwrap();
            for (index, &vertex) in owned.iter().enumerate() {
                prefix.include(vertex, index);
            }
            prefix.finish();
            let public_lookup = SortedVertexLookup::from_sorted(&owned);
            let mut queries = vec![VId(0), VId(u128::MAX)];
            for &vertex in &owned {
                queries.extend([
                    vertex,
                    VId(vertex.0.wrapping_sub(1)),
                    VId(vertex.0.wrapping_add(1)),
                ]);
            }
            queries.extend((0..128).map(|bit| VId(owned[0].0 ^ (1u128 << bit))));
            // Repeated row-sized scans borrow the completed bounds; absent
            // endpoints and different shared high prefixes must still fail.
            for row in queries.chunks(31) {
                let prefix = retained.first().unwrap();
                for &query in row {
                    let expected = owned.binary_search(&query).ok();
                    assert_eq!(
                        prefix.ordinal(&owned, query),
                        expected,
                        "n={n} shape={shape} query={query:?}"
                    );
                    assert_eq!(public_lookup.ordinal(query), expected);
                }
            }
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct ProjectionContents {
    binding: SnapshotBinding,
    spec: ProjectionSpec,
    digest: Digest,
    vertices: Vec<VId>,
    names: Vec<[u8; 32]>,
    outgoing: (Vec<usize>, Vec<usize>),
    incoming: Option<(Vec<usize>, Vec<usize>)>,
    weight_bits: Vec<u64>,
    first_negative: Option<usize>,
    counts_and_bytes: [usize; 6],
}

fn contents(view: SnapshotGraphView) -> ProjectionContents {
    let graph = &*view.0;
    ProjectionContents {
        binding: graph.binding,
        spec: graph.spec,
        digest: graph.digest,
        vertices: graph.vertices.clone(),
        names: graph.names.clone(),
        outgoing: (
            graph.outgoing.offsets.clone(),
            graph.outgoing.neighbors.clone(),
        ),
        incoming: graph
            .incoming
            .as_ref()
            .map(|rows| (rows.offsets.clone(), rows.neighbors.clone())),
        weight_bits: graph
            .weights
            .iter()
            .map(|weight| weight.to_bits())
            .collect(),
        first_negative: graph.first_negative_weight_visit,
        counts_and_bytes: [
            graph.max_out_degree,
            graph.max_weak_degree,
            graph.edges,
            graph.input_edges,
            view.charged_workspace_bytes(),
            view.charged_resident_bytes(),
        ],
    }
}

fn directory_build(
    vertices: &[VId],
    edges: &[ProjectionEdge],
    spec: ProjectionSpec,
    limits: ProjectionLimits,
    owned: bool,
    incumbent: bool,
    stop: Option<usize>,
) -> (
    Result<ProjectionContents, ProjectionBuildError<usize>>,
    usize,
) {
    let mut units = 0;
    let result = with_incumbent_directory(incumbent, || {
        let checkpoint = || {
            units += 1;
            if stop == Some(units) {
                Err(units)
            } else {
                Ok(())
            }
        };
        if owned {
            SnapshotGraphView::build_owned_with_checkpoint(
                binding(),
                vertices.to_vec(),
                edges.to_vec(),
                spec,
                limits,
                checkpoint,
            )
        } else {
            SnapshotGraphView::build_with_checkpoint(
                binding(),
                vertices,
                edges,
                spec,
                limits,
                checkpoint,
            )
        }
    });
    (result.map(contents), units)
}

#[test]
fn indexed_projection_keeps_every_policy_error_digest_row_and_capacity_charge() {
    let vertices = directory_vertices(513, 0);
    let edges = [
        edge(1, vertices[0].0, vertices[1].0, 1e16),
        edge(2, vertices[0].0, vertices[1].0, -1e16),
        edge(3, vertices[0].0, vertices[1].0, 1.0),
        edge(4, vertices[1].0, vertices[0].0, -0.0),
        edge(5, vertices[2].0, vertices[2].0, -1.0),
        edge(6, vertices[3].0, vertices[4].0, 0.125),
    ];
    let admission = ProjectionLimits {
        max_vertices: 1024,
        ..limits()
    };
    for directedness in [
        Directedness::Directed,
        Directedness::Reversed,
        Directedness::Undirected,
    ] {
        for parallel_edges in [
            ParallelEdgePolicy::Reject,
            ParallelEdgePolicy::CollapseUnit,
            ParallelEdgePolicy::Minimum,
            ParallelEdgePolicy::Maximum,
            ParallelEdgePolicy::Sum,
        ] {
            for self_loops in [
                SelfLoopPolicy::Reject,
                SelfLoopPolicy::Drop,
                SelfLoopPolicy::Keep,
            ] {
                let spec = ProjectionSpec {
                    directedness,
                    parallel_edges,
                    self_loops,
                };
                for owned in [false, true] {
                    assert_eq!(
                        directory_build(&vertices, &edges, spec, admission, owned, false, None),
                        directory_build(&vertices, &edges, spec, admission, owned, true, None),
                        "{spec:?} owned={owned}"
                    );
                }
            }
        }
    }
    let mut duplicate_vertices = vertices.clone();
    duplicate_vertices.push(vertices[0]);
    let mut duplicate_edges = edges.to_vec();
    duplicate_edges.push(edges[0]);
    let missing_source = edge(0, 0, u128::MAX, 1.0);
    let missing_target = edge(0, vertices[0].0, u128::MAX, 1.0);
    let non_finite = edge(0, vertices[0].0, vertices[1].0, f64::NAN);
    let overflow = [
        edge(0, vertices[0].0, vertices[1].0, f64::MAX),
        edge(1, vertices[0].0, vertices[1].0, f64::MAX),
    ];
    for (vertices, edges) in [
        (&duplicate_vertices[..], &edges[..]),
        (&vertices[..], &duplicate_edges[..]),
        (&vertices[..], std::slice::from_ref(&missing_source)),
        (&vertices[..], std::slice::from_ref(&missing_target)),
        (&vertices[..], std::slice::from_ref(&non_finite)),
        (&vertices[..], &overflow[..]),
    ] {
        let expected = directory_build(vertices, edges, spec(), admission, true, true, None);
        assert!(expected.0.is_err());
        assert_eq!(
            directory_build(vertices, edges, spec(), admission, true, false, None),
            expected
        );
    }
}

#[test]
fn indexed_projection_preserves_every_cancellation_and_limit_refusal_prefix() {
    let mut vertices = directory_vertices(512, 2);
    vertices.sort_unstable();
    let edges = [
        edge(1, vertices[0].0, vertices[511].0, 1.0),
        edge(2, vertices[511].0, vertices[0].0, -2.0),
    ];
    let admission = ProjectionLimits {
        max_vertices: vertices.len(),
        ..limits()
    };
    for owned in [false, true] {
        let (_, total) = directory_build(&vertices, &edges, spec(), admission, owned, true, None);
        for stop in 1..=total + 1 {
            assert_eq!(
                directory_build(
                    &vertices,
                    &edges,
                    spec(),
                    admission,
                    owned,
                    false,
                    Some(stop)
                ),
                directory_build(
                    &vertices,
                    &edges,
                    spec(),
                    admission,
                    owned,
                    true,
                    Some(stop)
                ),
                "owned={owned} stop={stop}"
            );
        }
        let (result, _) = directory_build(&vertices, &edges, spec(), admission, owned, true, None);
        let workspace = result.unwrap().counts_and_bytes[4];
        for cap in [
            ProjectionLimits {
                max_vertices: 511,
                ..admission
            },
            ProjectionLimits {
                max_input_edges: 1,
                ..admission
            },
            ProjectionLimits {
                max_adjacency_entries: 1,
                ..admission
            },
            ProjectionLimits {
                max_workspace_bytes: 0,
                ..admission
            },
            ProjectionLimits {
                max_workspace_bytes: workspace - 1,
                ..admission
            },
            ProjectionLimits {
                max_workspace_bytes: workspace,
                ..admission
            },
        ] {
            assert_eq!(
                directory_build(&vertices, &edges, spec(), cap, owned, false, None),
                directory_build(&vertices, &edges, spec(), cap, owned, true, None)
            );
        }
    }
}

/// Paired maintenance comparison of complete cold projection construction,
/// both CSR-face/weight consumption and destruction. Input fixture generation
/// is outside the interval, as it is upstream of this builder in production.
/// This is not durable database/server latency or a competitor comparison.
#[test]
#[ignore = "manual paired cold projection measurement; optimized, single test thread"]
fn prefix_directory_paired_cold_build_performance() {
    use std::hint::black_box;
    use std::time::Instant;

    fn consume(view: &SnapshotGraphView) -> (Digest, u128, usize) {
        let mut consumed = 0u128;
        for node in 0..view.node_count() {
            consumed = consumed.rotate_left(7) ^ view.vertex_id(node).unwrap().0;
            let (targets, weights) = view.projected_row(node).unwrap();
            for (&target, weight) in targets.iter().zip(weights) {
                consumed = consumed.rotate_left(11) ^ target as u128 ^ weight.to_bits() as u128;
            }
            for &source in view.in_neighbors_indices(node).unwrap() {
                consumed = consumed.rotate_left(13) ^ source as u128;
            }
        }
        black_box((view.digest(), consumed, view.charged_workspace_bytes()))
    }

    for (name, n, fanout, identity_shape) in [
        ("tiny", 32usize, 2usize, 0usize),
        ("below_threshold", 511, 4, 0),
        ("road_feistel63", 4096, 2, 1),
        ("many_edges_feistel63", 4096, 32, 1),
        ("large_sparse_full128", 65536, 8, 0),
        ("shared_high_prefix", 4096, 8, 2),
        ("single_outlier_skew", 4096, 8, 4),
    ] {
        let vertices = directory_vertices(n, identity_shape);
        let edges: Vec<_> = (0..n)
            .flat_map(|source| {
                (1..=fanout).map(move |offset| (source, (source * 31 + offset * 127) % n))
            })
            .enumerate()
            .map(|(index, (source, target))| ProjectionEdge {
                eid: EId(permuted_identity(index as u128 + 999999)),
                source: vertices[source],
                target: vertices[target],
                weight: (index % 7) as f64,
            })
            .collect();
        let admission = ProjectionLimits {
            max_vertices: n,
            max_input_edges: edges.len(),
            max_adjacency_entries: 2 * edges.len(),
            max_workspace_bytes: usize::MAX,
        };
        for canonical in [true, false] {
            let mut vertices = vertices.clone();
            let mut edges = edges.clone();
            if canonical {
                vertices.sort_unstable();
                edges.sort_unstable_by_key(|edge| edge.eid);
            }
            let projection_spec = ProjectionSpec {
                parallel_edges: ParallelEdgePolicy::Minimum,
                ..spec()
            };
            // Full representation equality is checked separately from the
            // consumed checksum, including incoming rows and exact weight bits.
            assert_eq!(
                directory_build(
                    &vertices,
                    &edges,
                    projection_spec,
                    admission,
                    false,
                    true,
                    None
                ),
                directory_build(
                    &vertices,
                    &edges,
                    projection_spec,
                    admission,
                    false,
                    false,
                    None
                )
            );
            for trial in 0..7 {
                let mut pair = [None, None];
                for incumbent in [trial % 2 == 0, trial % 2 != 0] {
                    let mut units = 0usize;
                    let (elapsed, output) = with_incumbent_directory(incumbent, || {
                        let start = Instant::now();
                        let output = {
                            let view = SnapshotGraphView::build_with_checkpoint(
                                binding(),
                                black_box(&vertices),
                                black_box(&edges),
                                projection_spec,
                                admission,
                                || {
                                    units += 1;
                                    Ok::<(), Infallible>(())
                                },
                            )
                            .unwrap();
                            consume(&view)
                        };
                        (start.elapsed().as_nanos(), output)
                    });
                    pair[usize::from(incumbent)] = Some((units, output));
                    eprintln!(
                        "projection_pair fixture={name} vertices={n} edges={} canonical={canonical} trial={trial} incumbent={incumbent} elapsed_ns={elapsed} units={units} workspace_bytes={} consumed={}",
                        edges.len(),
                        output.2,
                        output.1,
                    );
                }
                assert_eq!(
                    pair[0], pair[1],
                    "{name} canonical={canonical} trial={trial}"
                );
            }
        }
    }
}

fn binding() -> SnapshotBinding {
    SnapshotBinding {
        root: ObjectId([7; 32]),
        as_of: CommitSeq(31),
    }
}
fn limits() -> ProjectionLimits {
    ProjectionLimits {
        max_vertices: 100,
        max_input_edges: 100,
        max_adjacency_entries: 200,
        max_workspace_bytes: 1 << 20,
    }
}
fn spec() -> ProjectionSpec {
    ProjectionSpec {
        directedness: Directedness::Directed,
        parallel_edges: ParallelEdgePolicy::Sum,
        self_loops: SelfLoopPolicy::Keep,
    }
}
fn edge(eid: u128, source: u128, target: u128, weight: f64) -> ProjectionEdge {
    ProjectionEdge {
        eid: EId(eid),
        source: VId(source),
        target: VId(target),
        weight,
    }
}
fn fixture() -> (Vec<VId>, Vec<ProjectionEdge>) {
    (
        vec![VId(u128::MAX), VId(17), VId(0), VId(91)],
        vec![
            edge(9, 17, 17, 0.0),
            edge(8, 17, u128::MAX, 2.0),
            edge(3, 0, 17, 1.0),
            edge(1, 0, 17, 1e16),
            edge(7, u128::MAX, 0, -0.0),
            edge(2, 0, 17, -1e16),
        ],
    )
}

#[test]
fn cached_component_degrees_match_final_rows_in_every_projection_direction() {
    let vertices: Vec<_> = (0..17).map(|i| VId((i as u128) << 64 | 19)).collect();
    // Outgoing and incoming maxima belong to different vertices. The combined
    // maximum must therefore come from complete rows, not the sum of maxima.
    // Raw duplicates and loops exercise reduction and both loop policies.
    let mut pairs: Vec<_> = (1..=5).map(|i| (0, i)).collect();
    pairs.extend((8..=12).map(|i| (i, 7)));
    pairs.extend([(0, 1), (0, 1), (2, 2), (7, 7)]);
    let edges: Vec<_> = pairs
        .iter()
        .enumerate()
        .map(|(i, &(source, target))| ProjectionEdge {
            eid: EId((i as u128) << 64 | 23),
            source: vertices[source],
            target: vertices[target],
            weight: 1.0,
        })
        .collect();
    for directedness in [
        Directedness::Directed,
        Directedness::Reversed,
        Directedness::Undirected,
    ] {
        for parallel_edges in [
            ParallelEdgePolicy::Sum,
            ParallelEdgePolicy::Minimum,
            ParallelEdgePolicy::Maximum,
            ParallelEdgePolicy::CollapseUnit,
        ] {
            for self_loops in [SelfLoopPolicy::Keep, SelfLoopPolicy::Drop] {
                let spec = ProjectionSpec {
                    directedness,
                    parallel_edges,
                    self_loops,
                };
                let borrowed =
                    SnapshotGraphView::build(binding(), &vertices, &edges, spec, limits()).unwrap();
                let owned = SnapshotGraphView::build_owned_with_checkpoint(
                    binding(),
                    vertices.clone(),
                    edges.clone(),
                    spec,
                    limits(),
                    || Ok::<(), Infallible>(()),
                )
                .unwrap();
                let outgoing = (0..vertices.len())
                    .map(|v| borrowed.neighbors_indices(v).unwrap().len())
                    .max()
                    .unwrap();
                let combined = (0..vertices.len())
                    .map(|v| {
                        borrowed.neighbors_indices(v).unwrap().len()
                            + borrowed.in_neighbors_indices(v).unwrap().len()
                    })
                    .max()
                    .unwrap();
                for view in [&borrowed, &owned, &borrowed.clone()] {
                    assert_eq!(view.component_degree_bound(false), outgoing, "{spec:?}");
                    assert_eq!(view.component_degree_bound(true), combined, "{spec:?}");
                    assert_eq!(view.digest(), borrowed.digest());
                }
            }
        }
    }
    for vertices in [&[][..], &vertices[..]] {
        let view = SnapshotGraphView::build(binding(), vertices, &[], spec(), limits()).unwrap();
        assert_eq!(view.component_degree_bound(false), 0);
        assert_eq!(view.component_degree_bound(true), 0);
    }
}

fn scanned_first_negative_visit(view: &SnapshotGraphView) -> Option<usize> {
    let mut visits = 0;
    for node in 0..view.node_count() {
        visits += 1;
        for &weight in view.projected_row(node).unwrap().1 {
            visits += 1;
            assert!(weight.is_finite());
            if weight < 0.0 {
                return Some(visits);
            }
        }
    }
    None
}

#[test]
fn cached_negative_weight_visit_matches_final_reduction_and_csr_order() {
    let vertices = [VId(u128::MAX), VId(17), VId(0), VId(91)];
    // EId order differs from outgoing row order. Sum and Maximum remove raw
    // negatives; Minimum retains them. A dropped negative loop must disappear.
    let edges = [
        edge(9, u128::MAX, 17, -4.0),
        edge(8, 0, 17, -3.0),
        edge(3, 0, 17, 5.0),
        edge(2, 91, 91, -1.0),
        edge(1, u128::MAX, 17, 4.0),
        edge(7, 17, 0, -0.0),
    ];
    for directedness in [
        Directedness::Directed,
        Directedness::Reversed,
        Directedness::Undirected,
    ] {
        for parallel_edges in [
            ParallelEdgePolicy::Minimum,
            ParallelEdgePolicy::Maximum,
            ParallelEdgePolicy::Sum,
            ParallelEdgePolicy::CollapseUnit,
        ] {
            for self_loops in [SelfLoopPolicy::Keep, SelfLoopPolicy::Drop] {
                let spec = ProjectionSpec {
                    directedness,
                    parallel_edges,
                    self_loops,
                };
                let view =
                    SnapshotGraphView::build(binding(), &vertices, &edges, spec, limits()).unwrap();
                let expected = scanned_first_negative_visit(&view);
                assert_eq!(view.first_negative_weight_visit(), expected, "{spec:?}");
                assert_eq!(view.clone().first_negative_weight_visit(), expected);
                if parallel_edges == ParallelEdgePolicy::CollapseUnit
                    || (self_loops == SelfLoopPolicy::Drop
                        && parallel_edges != ParallelEdgePolicy::Minimum)
                {
                    assert_eq!(expected, None, "raw negatives must not poison {spec:?}");
                }
            }
        }
    }
}

#[test]
fn cached_negative_weight_visit_counts_isolates_and_crosses_meter_runs() {
    let vertices: Vec<_> = (0..80).map(VId).collect();
    let mut edges: Vec<_> = (1..=65).map(|i| edge(i, 0, i, 1.0)).collect();
    edges.push(edge(99, 79, 78, -1.0));
    for directedness in [
        Directedness::Directed,
        Directedness::Reversed,
        Directedness::Undirected,
    ] {
        let spec = ProjectionSpec {
            directedness,
            ..spec()
        };
        let view = SnapshotGraphView::build(binding(), &vertices, &edges, spec, limits()).unwrap();
        let expected = scanned_first_negative_visit(&view);
        assert!(expected.unwrap() > crate::FNX_METER_RUN);
        assert_eq!(view.first_negative_weight_visit(), expected);
    }
    for vertices in [&[][..], &vertices[..]] {
        let view = SnapshotGraphView::build(binding(), vertices, &[], spec(), limits()).unwrap();
        assert_eq!(view.first_negative_weight_visit(), None);
    }
}

#[test]
fn checkpointed_sort_matches_std_on_every_small_permutation_and_duplicate_keys() {
    fn permutations(values: &mut [usize], start: usize) {
        if start == values.len() {
            let mut actual = values.to_vec();
            let mut expected = actual.clone();
            expected.sort_unstable();
            sort_by_key(
                &mut actual,
                |value| *value,
                &mut || Ok::<(), Infallible>(()),
            )
            .unwrap();
            assert_eq!(actual, expected);
            // Duplicate keys, an important admission-error input shape.
            let mut actual: Vec<_> = values.iter().map(|value| value / 2).collect();
            let mut expected = actual.clone();
            expected.sort_unstable();
            sort_by_key(
                &mut actual,
                |value| *value,
                &mut || Ok::<(), Infallible>(()),
            )
            .unwrap();
            assert_eq!(actual, expected);
            return;
        }
        for i in start..values.len() {
            values.swap(start, i);
            permutations(values, start + 1);
            values.swap(start, i);
        }
    }
    for n in 0..=8 {
        permutations(&mut (0..n).collect::<Vec<_>>(), 0);
    }
    let mut ordered: Vec<_> = (0..4096).collect();
    let mut checks = 0;
    sort_by_key(&mut ordered, |value| *value, &mut || {
        checks += 1;
        Ok::<(), Infallible>(())
    })
    .unwrap();
    assert_eq!(checks, ordered.len(), "ordered input must stay linear");
}

#[test]
fn cancellation_interrupts_every_heap_sift_and_never_resumes() {
    let input = [9, 1, 8, 2, 7, 3, 6, 4, 5, 0];
    let mut checks = 0;
    sort_by_key(&mut input.to_vec(), |value| *value, &mut || {
        checks += 1;
        Ok::<(), &'static str>(())
    })
    .unwrap();
    assert!(checks > input.len() * 2);
    for stop in 1..=checks {
        let mut observed = 0;
        let error = sort_by_key(&mut input.to_vec(), |value| *value, &mut || {
            observed += 1;
            if observed == stop {
                Err("cancel")
            } else {
                Ok(())
            }
        })
        .unwrap_err();
        assert_eq!(error, ProjectionBuildError::Cancelled("cancel"));
        assert_eq!(observed, stop);
    }
}

/// Deterministic xorshift values for the larger sort laws.
fn scrambled(n: usize, seed: u64) -> Vec<u64> {
    let mut x = seed | 1;
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        })
        .collect()
}

#[test]
fn checkpointed_sort_matches_std_past_the_insertion_cutoff_on_adversarial_shapes() {
    for n in [17, 18, 33, 100, 1000, 5000] {
        let shapes: [Vec<u64>; 8] = [
            scrambled(n, n as u64),
            (0..n as u64).rev().collect(),
            (0..n as u64).map(|i| i.min(n as u64 - i)).collect(),
            (0..n as u64).map(|i| i % 7).collect(),
            scrambled(n, 3).into_iter().map(|value| value % 3).collect(),
            (0..n as u64)
                .map(|i| if i == 0 { n as u64 } else { i })
                .collect(),
            (0..n as u64)
                .map(|i| if i + 1 == n as u64 { 0 } else { i + 1 })
                .collect(),
            (0..n as u64).map(|i| (i * 7919) % n as u64).collect(),
        ];
        for (shape, input) in shapes.into_iter().enumerate() {
            let mut expected = input.clone();
            expected.sort_unstable();
            let mut actual = input.clone();
            sort_by_key(
                &mut actual,
                |value| *value,
                &mut || Ok::<(), Infallible>(()),
            )
            .unwrap();
            assert_eq!(actual, expected, "n={n} shape={shape}");
            // The depth-limit fallback alone must also sort every shape.
            let mut fallback = input;
            introsort(&mut fallback, &|value: &u64| *value, 0, &mut || {
                Ok::<(), Infallible>(())
            })
            .unwrap();
            assert_eq!(fallback, expected, "heap fallback n={n} shape={shape}");
        }
    }
}

#[test]
fn cancellation_interrupts_every_partition_step_and_never_resumes() {
    let input = scrambled(200, 11);
    let mut checks = 0;
    sort_by_key(&mut input.clone(), |value| *value, &mut || {
        checks += 1;
        Ok::<(), &'static str>(())
    })
    .unwrap();
    // More than one partition pass over the input must have polled.
    assert!(checks > input.len() * 3, "{checks} checks");
    for stop in 1..=checks {
        let mut observed = 0;
        let error = sort_by_key(&mut input.clone(), |value| *value, &mut || {
            observed += 1;
            if observed == stop {
                Err("cancel")
            } else {
                Ok(())
            }
        })
        .unwrap_err();
        assert_eq!(error, ProjectionBuildError::Cancelled("cancel"));
        assert_eq!(observed, stop);
    }
}

#[test]
fn every_projection_checkpoint_discards_partial_state_for_borrowed_and_owned_inputs() {
    let (vertices, edges) = fixture();
    for direction in [
        Directedness::Directed,
        Directedness::Reversed,
        Directedness::Undirected,
    ] {
        for loops in [SelfLoopPolicy::Keep, SelfLoopPolicy::Drop] {
            let spec = ProjectionSpec {
                directedness: direction,
                self_loops: loops,
                ..spec()
            };
            for owned in [false, true] {
                let mut checks = 0;
                let checkpoint = || {
                    checks += 1;
                    Ok::<(), &'static str>(())
                };
                if owned {
                    SnapshotGraphView::build_owned_with_checkpoint(
                        binding(),
                        vertices.clone(),
                        edges.clone(),
                        spec,
                        limits(),
                        checkpoint,
                    )
                    .unwrap();
                } else {
                    SnapshotGraphView::build_with_checkpoint(
                        binding(),
                        &vertices,
                        &edges,
                        spec,
                        limits(),
                        checkpoint,
                    )
                    .unwrap();
                }
                assert!(
                    checks > 100,
                    "validation, hashing and assembly must all checkpoint"
                );
                for stop in 1..=checks {
                    let mut observed = 0;
                    let checkpoint = || {
                        observed += 1;
                        if observed == stop {
                            Err("cancel")
                        } else {
                            Ok(())
                        }
                    };
                    let result = if owned {
                        SnapshotGraphView::build_owned_with_checkpoint(
                            binding(),
                            vertices.clone(),
                            edges.clone(),
                            spec,
                            limits(),
                            checkpoint,
                        )
                    } else {
                        SnapshotGraphView::build_with_checkpoint(
                            binding(),
                            &vertices,
                            &edges,
                            spec,
                            limits(),
                            checkpoint,
                        )
                    };
                    assert_eq!(
                        result.unwrap_err(),
                        ProjectionBuildError::Cancelled("cancel")
                    );
                    assert_eq!(observed, stop);
                }
            }
        }
    }
    assert_eq!(
        (vertices, edges),
        fixture(),
        "borrowed inputs must never mutate"
    );
}

#[test]
fn owned_projection_retains_the_vertex_allocation_and_charges_spare_capacity() {
    let mut vertices = Vec::with_capacity(32);
    vertices.extend([VId(17), VId(0)]);
    let mut edges = Vec::with_capacity(64);
    edges.push(edge(9, 0, 17, 2.0));
    let pointer = vertices.as_ptr();
    let input_bytes =
        vertices.capacity() * size_of::<VId>() + edges.capacity() * size_of::<ProjectionEdge>();
    let expected = input_bytes
        + 2 * 32
        + 2 * (2 * 2 + 1) * size_of::<usize>()
        + 2 * size_of::<usize>()
        + size_of::<f64>();
    let graph = SnapshotGraphView::build_owned_with_checkpoint(
        binding(),
        vertices,
        edges,
        spec(),
        ProjectionLimits {
            max_workspace_bytes: expected,
            ..limits()
        },
        || Ok::<(), Infallible>(()),
    )
    .unwrap();
    assert_eq!(
        graph.vertex_ids().as_ptr(),
        pointer,
        "do not clone transferred vertices"
    );
    assert_eq!(graph.vertex_ids(), &[VId(0), VId(17)]);
    assert_eq!(graph.charged_workspace_bytes(), expected);
    // Held after the build: the transferred vertices with their spare
    // capacity, the names, both faces' offsets and neighbors, and the weight.
    // The edge staging is not.
    assert_eq!(
        graph.charged_resident_bytes(),
        32 * size_of::<VId>() + 2 * 32 + 2 * (3 + 1) * size_of::<usize>() + size_of::<f64>()
    );
    let shared = graph.clone();
    assert!(graph.shares_cache_with(&shared));
    assert!(std::ptr::eq(
        graph.projected_row(0).unwrap().0,
        shared.projected_row(0).unwrap().0
    ));

    // Even EMPTY vectors can carry substantial backing stores. Length-only
    // admission would hide this memory and perform later allocations anyway.
    let vertices = Vec::<VId>::with_capacity(32);
    let mut checks = 0;
    let error = SnapshotGraphView::build_owned_with_checkpoint(
        binding(),
        vertices,
        vec![],
        spec(),
        ProjectionLimits {
            max_workspace_bytes: 0,
            ..limits()
        },
        || {
            checks += 1;
            Ok::<(), Infallible>(())
        },
    )
    .unwrap_err();
    assert!(matches!(
        error,
        ProjectionBuildError::Projection(ProjectionError::LimitExceeded {
            resource: "workspace bytes",
            ..
        })
    ));
    assert_eq!(
        checks, 1,
        "refuse transferred capacity before sorting/assembly"
    );
}

#[test]
fn all_three_node_topologies_match_an_independent_multigraph_reducer() {
    let vertices = [VId(0), VId(17), VId(u128::MAX - 1), VId(u128::MAX)];
    for mask in 0..512 {
        let mut edges = Vec::new();
        for bit in 0..9 {
            if mask & (1 << bit) != 0 {
                for copy in 0..2 {
                    edges.push(ProjectionEdge {
                        eid: EId((bit * 2 + copy) as u128),
                        source: vertices[bit / 3],
                        target: vertices[bit % 3],
                        weight: (copy + 1) as f64,
                    });
                }
            }
        }
        for direction in [
            Directedness::Directed,
            Directedness::Reversed,
            Directedness::Undirected,
        ] {
            for loops in [SelfLoopPolicy::Keep, SelfLoopPolicy::Drop] {
                for parallel in [
                    ParallelEdgePolicy::CollapseUnit,
                    ParallelEdgePolicy::Minimum,
                    ParallelEdgePolicy::Maximum,
                    ParallelEdgePolicy::Sum,
                ] {
                    let spec = ProjectionSpec {
                        directedness: direction,
                        self_loops: loops,
                        parallel_edges: parallel,
                    };
                    let mut reference: BTreeMap<(VId, VId), f64> = BTreeMap::new();
                    for edge in &edges {
                        let (mut source, mut target) = (edge.source, edge.target);
                        if source == target && loops == SelfLoopPolicy::Drop {
                            continue;
                        }
                        if direction == Directedness::Reversed
                            || (direction == Directedness::Undirected && target < source)
                        {
                            std::mem::swap(&mut source, &mut target);
                        }
                        let weight = if parallel == ParallelEdgePolicy::CollapseUnit {
                            1.0
                        } else {
                            edge.weight
                        };
                        reference
                            .entry((source, target))
                            .and_modify(|old| {
                                *old = match parallel {
                                    ParallelEdgePolicy::Minimum => old.min(weight),
                                    ParallelEdgePolicy::Maximum => old.max(weight),
                                    ParallelEdgePolicy::Sum => *old + weight,
                                    _ => 1.0,
                                };
                            })
                            .or_insert(weight);
                    }
                    let graph = SnapshotGraphView::build_with_checkpoint(
                        binding(),
                        &vertices,
                        &edges,
                        spec,
                        limits(),
                        || Ok::<(), Infallible>(()),
                    )
                    .unwrap();
                    let mut reordered = edges.clone();
                    reordered.reverse();
                    let owned = SnapshotGraphView::build_owned_with_checkpoint(
                        binding(),
                        vertices.iter().copied().rev().collect(),
                        reordered,
                        spec,
                        limits(),
                        || Ok::<(), Infallible>(()),
                    )
                    .unwrap();
                    assert_eq!(graph.digest(), owned.digest());
                    assert_eq!(graph.edge_count(), reference.len());
                    assert_eq!(graph.input_edge_count(), edges.len());
                    assert_eq!(graph.node_count(), 4); // includes an isolated vertex
                    for (s, &source) in vertices.iter().enumerate() {
                        let mut out = Vec::new();
                        let mut incoming = Vec::new();
                        for (t, &target) in vertices.iter().enumerate() {
                            let get = |a, b| {
                                reference.get(&(a, b)).copied().or_else(|| {
                                    (direction == Directedness::Undirected)
                                        .then(|| reference.get(&(b, a)).copied())
                                        .flatten()
                                })
                            };
                            let expected = get(source, target);
                            assert_eq!(
                                graph.projected_weight(s, t).map(f64::to_bits),
                                expected.map(f64::to_bits)
                            );
                            assert_eq!(
                                owned.projected_weight(s, t).map(f64::to_bits),
                                expected.map(f64::to_bits)
                            );
                            if expected.is_some() {
                                out.push(t);
                            }
                            if get(target, source).is_some() {
                                incoming.push(t);
                            }
                        }
                        assert_eq!(graph.neighbors_indices(s).unwrap(), out.as_slice());
                        assert_eq!(graph.in_neighbors_indices(s).unwrap(), incoming.as_slice());
                    }
                }
            }
        }
    }
}

#[test]
fn csr_scatter_preserves_skewed_rows_loops_and_isolated_sparse_identities() {
    // Distinct high bits with identical low bits catch accidental narrowing;
    // the two extremes and directory holes also exercise the source walk.
    let mut vertices: Vec<_> = (0..64)
        .map(|i| VId((1u128 << 120) + ((i as u128) << 64) + 17))
        .collect();
    vertices[0] = VId(0);
    vertices[63] = VId(u128::MAX);
    let mut edges = Vec::new();
    let mut push = |s: usize, t: usize, weight| {
        edges.push(ProjectionEdge {
            eid: EId((1u128 << 96) + edges.len() as u128),
            source: vertices[s],
            target: vertices[t],
            weight,
        });
    };
    for target in 1..63 {
        if target % 5 != 0 {
            push(7, target, 1.0 + (target % 11) as f64 / 4.0);
            push(target, 29, -((target % 7) as f64) / 8.0);
            push(58, target, 2.0);
            push(target, 7, 3.0);
        }
    }
    push(7, 7, -0.0);
    push(62, 62, 5.0);
    push(1, 61, 1e16);
    push(1, 61, -1e16);
    push(1, 61, 1.0);
    let admission = ProjectionLimits {
        max_input_edges: edges.len(),
        max_adjacency_entries: edges.len() * 2,
        ..limits()
    };
    for direction in [
        Directedness::Directed,
        Directedness::Reversed,
        Directedness::Undirected,
    ] {
        for loops in [SelfLoopPolicy::Keep, SelfLoopPolicy::Drop] {
            let spec = ProjectionSpec {
                directedness: direction,
                self_loops: loops,
                ..spec()
            };
            // An independent stable-ID map reduces in ascending EId order.
            // It neither assigns ordinals nor shares the builder's scatter.
            let mut reference = BTreeMap::<(VId, VId), f64>::new();
            for edge in &edges {
                let (mut source, mut target) = (edge.source, edge.target);
                if source == target && loops == SelfLoopPolicy::Drop {
                    continue;
                }
                if direction == Directedness::Reversed
                    || (direction == Directedness::Undirected && target < source)
                {
                    std::mem::swap(&mut source, &mut target);
                }
                let value = reference.entry((source, target)).or_insert(0.0);
                *value += edge.weight;
                if *value == 0.0 {
                    *value = 0.0;
                }
            }
            let mut rows = vec![BTreeMap::<VId, u64>::new(); vertices.len()];
            let mut incoming = vec![Vec::new(); vertices.len()];
            for (s, &source) in vertices.iter().enumerate() {
                for (t, &target) in vertices.iter().enumerate() {
                    let get = |a, b| {
                        reference.get(&(a, b)).or_else(|| {
                            (direction == Directedness::Undirected)
                                .then(|| reference.get(&(b, a)))
                                .flatten()
                        })
                    };
                    if let Some(weight) = get(source, target) {
                        rows[s].insert(target, weight.to_bits());
                    }
                    if get(target, source).is_some() {
                        incoming[s].push(t);
                    }
                }
            }
            let graph =
                SnapshotGraphView::build(binding(), &vertices, &edges, spec, admission).unwrap();
            let owned = SnapshotGraphView::build_owned_with_checkpoint(
                binding(),
                vertices.iter().copied().rev().collect(),
                edges.iter().copied().rev().collect(),
                spec,
                admission,
                || Ok::<(), Infallible>(()),
            )
            .unwrap();
            assert_eq!(graph.digest(), owned.digest());
            assert_eq!(graph.edge_count(), reference.len());
            assert_eq!(graph.input_edge_count(), edges.len());
            for graph in [&graph, &owned] {
                assert_eq!(graph.vertex_ids(), vertices);
                for s in 0..vertices.len() {
                    let (neighbors, weights) = graph.projected_row(s).unwrap();
                    let actual: Vec<_> = neighbors
                        .iter()
                        .zip(weights)
                        .map(|(&t, weight)| (vertices[t], weight.to_bits()))
                        .collect();
                    let expected: Vec<_> = rows[s].iter().map(|(&t, &w)| (t, w)).collect();
                    assert_eq!(actual, expected, "{direction:?} {loops:?} source={s}");
                    assert_eq!(graph.in_neighbors_indices(s).unwrap(), incoming[s]);
                }
            }
        }
    }
}

#[test]
fn degree_buffers_are_reused_with_exact_peak_admission_on_every_face() {
    for direction in [
        Directedness::Directed,
        Directedness::Reversed,
        Directedness::Undirected,
    ] {
        let vertices = vec![VId(0), VId(17), VId(91), VId(u128::MAX)];
        let edges = vec![edge(1, 0, 17, 2.0), edge(2, 17, 17, 3.0)];
        let spec = ProjectionSpec {
            directedness: direction,
            ..spec()
        };
        let faces = if direction == Directedness::Undirected {
            1
        } else {
            2
        };
        let arcs = if faces == 1 { 3 } else { 2 };
        let expected = vertices.capacity() * size_of::<VId>()
            + edges.capacity() * size_of::<ProjectionEdge>()
            + vertices.len() * 32
            + faces * (2 * vertices.len() + 1) * size_of::<usize>()
            + arcs * faces * size_of::<usize>()
            + arcs * size_of::<f64>();
        let admission = ProjectionLimits {
            max_workspace_bytes: expected,
            ..limits()
        };
        let graph = SnapshotGraphView::build_owned_with_checkpoint(
            binding(),
            vertices.clone(),
            edges.clone(),
            spec,
            admission,
            || Ok::<(), Infallible>(()),
        )
        .unwrap();
        assert_eq!(graph.charged_workspace_bytes(), expected);
        let error = SnapshotGraphView::build_owned_with_checkpoint(
            binding(),
            vertices,
            edges,
            spec,
            ProjectionLimits {
                max_workspace_bytes: expected - 1,
                ..admission
            },
            || Ok::<(), Infallible>(()),
        )
        .unwrap_err();
        assert_eq!(
            error,
            ProjectionBuildError::Projection(ProjectionError::LimitExceeded {
                resource: "workspace bytes",
                limit: expected - 1,
                observed: expected,
            })
        );
    }
}

#[test]
fn empty_and_fully_dropped_graphs_never_read_a_source_ordinal() {
    for direction in [
        Directedness::Directed,
        Directedness::Reversed,
        Directedness::Undirected,
    ] {
        let spec = ProjectionSpec {
            directedness: direction,
            self_loops: SelfLoopPolicy::Drop,
            ..spec()
        };
        for (vertices, edges) in [
            (vec![], vec![]),
            (vec![VId(0), VId(u128::MAX)], vec![]),
            (
                vec![VId(0), VId(u128::MAX)],
                vec![
                    edge(1, 0, 0, f64::NAN),
                    edge(2, u128::MAX, u128::MAX, f64::NEG_INFINITY),
                ],
            ),
        ] {
            let borrowed =
                SnapshotGraphView::build(binding(), &vertices, &edges, spec, limits()).unwrap();
            let owned = SnapshotGraphView::build_owned_with_checkpoint(
                binding(),
                vertices.clone(),
                edges.clone(),
                spec,
                limits(),
                || Ok::<(), Infallible>(()),
            )
            .unwrap();
            assert_eq!(borrowed.digest(), owned.digest());
            for graph in [borrowed, owned] {
                assert_eq!(graph.vertex_ids(), vertices);
                assert_eq!(graph.input_edge_count(), edges.len());
                assert_eq!(graph.edge_count(), 0);
                assert_eq!(graph.outgoing_offsets(), vec![0; vertices.len() + 1]);
                let (offsets, neighbors) = graph.incoming_face();
                assert_eq!(offsets, vec![0; vertices.len() + 1]);
                assert!(neighbors.is_empty());
                for s in 0..vertices.len() {
                    assert_eq!(graph.projected_row(s).unwrap(), (&[][..], &[][..]));
                }
            }
        }
        // Dropping a loop must not bypass the existing endpoint admission.
        assert_eq!(
            SnapshotGraphView::build(binding(), &[], &[edge(1, 0, 0, f64::NAN)], spec, limits())
                .unwrap_err(),
            ProjectionError::MissingEndpoint {
                edge: EId(1),
                vertex: VId(0),
            }
        );
    }
}

#[test]
fn eid_order_fixes_floating_reduction_and_source_identity_stays_bound() {
    let (vertices, mut edges) = fixture();
    let graph = SnapshotGraphView::build(binding(), &vertices, &edges, spec(), limits()).unwrap();
    // ((1e16 + -1e16) + 1) differs from (1e16 + (-1e16 + 1)).
    assert_eq!(
        graph.projected_weight(0, 1).unwrap().to_bits(),
        1.0f64.to_bits()
    );
    let original = graph.digest();
    for _ in 0..edges.len() {
        edges.rotate_left(1);
        assert_eq!(
            SnapshotGraphView::build(binding(), &vertices, &edges, spec(), limits())
                .unwrap()
                .digest(),
            original
        );
    }
    let mut changed = binding();
    changed.as_of = CommitSeq(changed.as_of.0 + 1);
    assert_ne!(
        SnapshotGraphView::build(changed, &vertices, &edges, spec(), limits())
            .unwrap()
            .digest(),
        original
    );
    edges[0].eid = EId(100);
    assert_ne!(
        SnapshotGraphView::build(binding(), &vertices, &edges, spec(), limits())
            .unwrap()
            .digest(),
        original
    );
}

#[test]
fn cancellation_and_validation_errors_are_not_interchangeable() {
    let vertices = [VId(0), VId(0)];
    assert_eq!(
        SnapshotGraphView::build(binding(), &vertices, &[], spec(), limits()).unwrap_err(),
        ProjectionError::DuplicateVertex(VId(0))
    );
    assert_eq!(
        SnapshotGraphView::build_with_checkpoint(
            binding(),
            &vertices,
            &[],
            spec(),
            limits(),
            || Err("stop")
        )
        .unwrap_err(),
        ProjectionBuildError::Cancelled("stop")
    );
    let vertices = [VId(0), VId(17)];
    let invalid = [
        (
            vec![edge(1, 0, 91, 1.0)],
            ProjectionError::MissingEndpoint {
                edge: EId(1),
                vertex: VId(91),
            },
        ),
        (
            vec![edge(1, 0, 17, 1.0), edge(1, 17, 0, 1.0)],
            ProjectionError::DuplicateEdge(EId(1)),
        ),
        (
            vec![edge(1, 0, 17, f64::NAN)],
            ProjectionError::NonFiniteWeight(EId(1)),
        ),
        (
            vec![edge(1, 0, 17, f64::MAX), edge(2, 0, 17, f64::MAX)],
            ProjectionError::WeightOverflow {
                source: VId(0),
                target: VId(17),
            },
        ),
    ];
    for (edges, expected) in invalid {
        assert_eq!(
            SnapshotGraphView::build(binding(), &vertices, &edges, spec(), limits()).unwrap_err(),
            expected
        );
        assert_eq!(
            SnapshotGraphView::build_owned_with_checkpoint(
                binding(),
                vertices.to_vec(),
                edges,
                spec(),
                limits(),
                || Ok::<(), Infallible>(())
            )
            .unwrap_err(),
            ProjectionBuildError::Projection(expected)
        );
    }
}

#[test]
fn discarded_weights_and_self_loops_do_not_become_observable() {
    let vertices = [VId(0), VId(17)];
    let edges = [edge(1, 0, 0, f64::NAN), edge(2, 0, 17, f64::NEG_INFINITY)];
    let spec = ProjectionSpec {
        self_loops: SelfLoopPolicy::Drop,
        parallel_edges: ParallelEdgePolicy::CollapseUnit,
        ..spec()
    };
    let graph = SnapshotGraphView::build_with_checkpoint(
        binding(),
        &vertices,
        &edges,
        spec,
        limits(),
        || Ok::<(), Infallible>(()),
    )
    .unwrap();
    assert_eq!(graph.edge_count(), 1);
    assert_eq!(graph.projected_weight(0, 1), Some(1.0));
    assert_eq!(graph.projected_weight(0, 0), None);
}
