use fgdb_prism::*;
use fgdb_types::{CommitSeq, EId, VId, ids::ObjectId};
use fnx_classes::{Graph, digraph::DiGraph};
use std::collections::BTreeMap;
use std::convert::Infallible;

fn graph(vertices: &[VId], edges: &[ProjectionEdge], direction: Directedness) -> SnapshotGraphView {
    SnapshotGraphView::build(
        SnapshotBinding {
            root: ObjectId([8; 32]),
            as_of: CommitSeq(13),
        },
        vertices,
        edges,
        ProjectionSpec {
            directedness: direction,
            parallel_edges: ParallelEdgePolicy::Sum,
            self_loops: SelfLoopPolicy::Keep,
        },
        ProjectionLimits {
            max_vertices: vertices.len(),
            max_input_edges: edges.len(),
            max_adjacency_entries: edges.len() * 2,
            max_workspace_bytes: 1 << 28,
        },
    )
    .unwrap()
}
fn limits() -> FnxExecutionLimits {
    // Traversals are not iterative convergence algorithms.
    FnxExecutionLimits {
        max_iterations: 0,
        max_result_rows: 100_000,
        max_estimated_work: 1 << 24,
    }
}
fn execute(call: &FnxCallSpec, graph: &SnapshotGraphView) -> FnxResult {
    call.execute(graph, limits(), || Ok::<(), Infallible>(()))
        .unwrap()
}
fn bind(text: &str) -> FnxCallSpec {
    FnxCallSpec::bind(text, &FnxParameters::new()).unwrap()
}
fn distances(result: FnxResult) -> BTreeMap<VId, u64> {
    result
        .rows
        .into_iter()
        .map(|row| match row.as_slice() {
            [FnxValue::Vertex(vertex), FnxValue::Integer(distance)] => (*vertex, *distance),
            _ => panic!("typed distance row required"),
        })
        .collect()
}
fn labels(result: FnxResult) -> BTreeMap<VId, VId> {
    result
        .rows
        .into_iter()
        .map(|row| match row.as_slice() {
            [FnxValue::Vertex(vertex), FnxValue::Vertex(component)] => (*vertex, *component),
            _ => panic!("typed component row required"),
        })
        .collect()
}
fn oracle_labels(components: Vec<Vec<String>>, graph: &SnapshotGraphView) -> BTreeMap<VId, VId> {
    let mut result = BTreeMap::new();
    for component in components {
        let members: Vec<_> = component
            .iter()
            .map(|name| {
                graph
                    .vertex_id(graph.get_node_index(name).unwrap())
                    .unwrap()
            })
            .collect();
        let minimum = *members.iter().min().unwrap();
        for member in members {
            assert!(result.insert(member, minimum).is_none());
        }
    }
    result
}

#[test]
fn bfs_compact_workspace_preserves_long_paths_cutoffs_and_public_cursor_values() {
    // Cross both byte and u16 hop ranges; identities use the full VId domain.
    // A two-vertex unreachable prefix distinguishes absence from source zero.
    let n = 65_539;
    let source = 2;
    let vertices: Vec<_> = (0..n)
        .map(|index| VId((index as u128) << 64 | 17))
        .collect();
    let edges: Vec<_> = (1..n)
        .map(|target| ProjectionEdge {
            eid: EId((target as u128) << 64 | 29),
            source: vertices[target - 1],
            target: vertices[target],
            weight: -1.0, // unweighted BFS must still ignore weights
        })
        .collect();
    let view = graph(&vertices, &edges, Directedness::Directed);
    for cutoff in [Some(0), Some(usize::MAX), None] {
        let call = FnxCallSpec::single_source_shortest_path_length(vertices[source], cutoff);
        let mut meter = ComponentMeter::default();
        let mut cursor = call.execute_rows(&view, limits(), &mut meter).unwrap();
        let reached = if cutoff == Some(0) { 1 } else { n - source };
        assert_eq!(cursor.remaining(), reached);
        assert_eq!(
            cursor.certificate().kernel_workspace_bytes,
            2 * n * size_of::<usize>()
        );
        assert!(cursor.certificate().bfs_decision_card.is_none());
        assert_eq!(cursor.certificate().witness.nodes_touched, reached);
        assert_eq!(cursor.certificate().witness.edges_scanned, reached - 1);
        assert_eq!(meter.observed, 3 * n + reached + (reached - 1) + 3);
        assert_eq!(cursor.max_integer(), Some((reached - 1) as u64));
        let mut row = Vec::new();
        let mut pulled = 0;
        let mut checks = 0;
        while cursor
            .next_row(&mut row, || {
                checks += 1;
                Ok::<(), Infallible>(())
            })
            .unwrap()
        {
            assert_eq!(
                row,
                [
                    FnxValue::Vertex(vertices[source + pulled]),
                    FnxValue::Integer(pulled as u64),
                ]
            );
            pulled += 1;
        }
        assert_eq!(pulled, reached);
        assert_eq!(checks, n);
        assert_eq!(cursor.remaining(), 0);
        assert!(row.is_empty());
    }
}

#[derive(Default)]
struct ComponentMeter {
    observed: usize,
    calls: usize,
    largest_run: usize,
    full_runs: usize,
    stop: Option<usize>,
    stopped: bool,
}

impl FnxMeter<usize> for &mut ComponentMeter {
    fn poll(&mut self) -> Result<(), usize> {
        assert!(!self.stopped, "no meter call may follow a refusal");
        Ok(())
    }

    fn unit(&mut self) -> Result<(), usize> {
        self.units(1)
    }

    fn units(&mut self, n: usize) -> Result<(), usize> {
        assert!(!self.stopped, "no meter call may follow a refusal");
        assert!((1..=FNX_METER_RUN).contains(&n));
        self.calls += 1;
        self.largest_run = self.largest_run.max(n);
        self.full_runs += usize::from(n == FNX_METER_RUN);
        if let Some(stop) = self.stop
            && self.observed + n >= stop
        {
            self.observed = stop;
            self.stopped = true;
            return Err(stop);
        }
        self.observed += n;
        Ok(())
    }
}

#[test]
fn component_pull_public_calls_keep_labels_limits_and_every_refusal() {
    let n = 35;
    let vertices: Vec<_> = (0..n).map(|i| VId((i as u128) << 96 | 23)).collect();
    let mut pairs = Vec::new();
    for a in 2..31 {
        for b in 2..31 {
            if a != b {
                pairs.push((a, b));
            }
        }
    }
    pairs.extend([(31, 30), (30, 32), (33, 34), (2, 2), (31, 30)]);
    let edges: Vec<_> = pairs
        .iter()
        .enumerate()
        .map(|(i, &(a, b))| ProjectionEdge {
            eid: EId((i as u128) << 80 | 37),
            source: vertices[a],
            target: vertices[b],
            weight: -3.0,
        })
        .collect();
    for direction in [
        Directedness::Directed,
        Directedness::Reversed,
        Directedness::Undirected,
    ] {
        let view = graph(&vertices, &edges, direction);
        let call = if direction == Directedness::Undirected {
            FnxCallSpec::connected_components()
        } else {
            FnxCallSpec::weakly_connected_components()
        };
        let mut complete = ComponentMeter::default();
        let result = call.execute(&view, limits(), &mut complete).unwrap();
        let expected: BTreeMap<_, _> = vertices
            .iter()
            .enumerate()
            .map(|(i, &id)| {
                (
                    id,
                    vertices[if i < 2 {
                        i
                    } else if i < 33 {
                        2
                    } else {
                        33
                    }],
                )
            })
            .collect();
        assert_eq!(labels(result.clone()), expected);
        let certificate = &result.certificate;
        let card = certificate
            .component_decision_card
            .as_ref()
            .expect("dense pull");
        assert!(certificate.bfs_decision_card.is_none());
        assert_eq!(card.fallback, "component_bfs");
        assert!(card.pulls.iter().any(|pull| pull.root == 2));
        let witness = &certificate.witness;
        assert!(witness.nodes_touched + witness.edges_scanned <= certificate.estimated_work);
        assert_eq!(
            complete.observed,
            4 * n + witness.nodes_touched + witness.edges_scanned + 3 + card.pulls.len()
        );
        assert!(
            certificate.kernel_workspace_bytes
                >= 2 * n * size_of::<usize>()
                    + card.pulls.capacity() * size_of::<ComponentPullDecision>()
        );

        // Reconstruct the entire public certificate independently of either
        // streaming card helper, including the absent BFS and present CC tags.
        let text = |bytes: &mut Vec<u8>, value: &str| {
            bytes.extend_from_slice(&(value.len() as u128).to_le_bytes());
            bytes.extend_from_slice(value.as_bytes());
        };
        let mut bytes = b"fgdb:prism:call-certificate:v5".to_vec();
        bytes.extend_from_slice(&certificate.registry_version.to_le_bytes());
        bytes.extend_from_slice(certificate.implementation_revision.as_bytes());
        text(&mut bytes, certificate.execution_kernel);
        bytes.extend_from_slice(&certificate.kernel_source_digest.0);
        text(&mut bytes, certificate.numeric_profile);
        bytes.extend_from_slice(&certificate.projection_digest.0);
        bytes.extend_from_slice(&certificate.call_digest.0);
        bytes.extend_from_slice(&certificate.result_digest.0);
        text(&mut bytes, certificate.adapter.as_str());
        for number in [
            certificate.vertices,
            certificate.edges,
            certificate.input_edges,
            certificate.estimated_work,
            certificate.kernel_workspace_bytes,
            witness.nodes_touched,
            witness.edges_scanned,
            witness.queue_peak,
        ] {
            bytes.extend_from_slice(&(number as u128).to_le_bytes());
        }
        text(&mut bytes, &witness.algorithm);
        text(&mut bytes, &witness.complexity_claim);
        bytes.extend_from_slice(&[0, 1]);
        bytes.extend_from_slice(&card.policy_version.to_le_bytes());
        text(&mut bytes, card.fallback);
        for number in [
            card.dwell_frontiers,
            card.pull_cost_multiplier,
            card.pulls.len(),
        ] {
            bytes.extend_from_slice(&(number as u128).to_le_bytes());
        }
        for pull in &card.pulls {
            for number in [
                pull.root,
                pull.frontier_vertices,
                pull.frontier_incidences,
                pull.unseen_incidences,
                pull.vertices,
                pull.benefit_lower,
                pull.benefit_upper,
                pull.candidate_visits,
                pull.incidence_visits,
            ] {
                bytes.extend_from_slice(&(number as u128).to_le_bytes());
            }
        }
        assert_eq!(certificate.digest, fgdb_crypto::hash(&bytes));
        let exact = FnxExecutionLimits {
            max_result_rows: n,
            max_estimated_work: certificate.estimated_work,
            ..limits()
        };
        assert_eq!(
            call.execute(&view, exact, || Ok::<(), usize>(())).unwrap(),
            result
        );
        for limited in [
            FnxExecutionLimits {
                max_result_rows: n - 1,
                ..exact
            },
            FnxExecutionLimits {
                max_estimated_work: exact.max_estimated_work - 1,
                ..exact
            },
        ] {
            assert!(matches!(
                call.execute(&view, limited, || Ok::<(), usize>(())),
                Err(FnxExecutionError::LimitExceeded { .. })
            ));
        }
        for stop in 1..=complete.observed {
            let mut bulk = ComponentMeter {
                stop: Some(stop),
                ..ComponentMeter::default()
            };
            assert!(
                matches!(call.execute(&view, exact, &mut bulk), Err(FnxExecutionError::Cancelled(at)) if at == stop)
            );
            assert_eq!(bulk.observed, stop);
            let mut observed = 0;
            assert!(matches!(call.execute(&view, exact, || {
                observed += 1;
                if observed == stop { Err(stop) } else { Ok(()) }
            }), Err(FnxExecutionError::Cancelled(at)) if at == stop));
            assert_eq!(observed, stop);
        }
    }
}

#[test]
fn bfs_runs_preserve_distances_witnesses_and_every_work_refusal() {
    // A hub followed by another level exercises full runs, remainders, loops,
    // duplicate incidences, and a wide frontier whose rows mostly revisit
    // vertices. Cutoffs must leave the deeper part of the graph unobserved.
    let width = 2 * FNX_METER_RUN + 3;
    let vertices: Vec<_> = (0..width + 3)
        .map(|index| VId((index as u128) << 64 | 19))
        .collect();
    let mut edges = Vec::new();
    for target in 0..width {
        for (source, target) in [(0, target), (target, width), (width, target)] {
            edges.push(ProjectionEdge {
                eid: EId(edges.len() as u128),
                source: vertices[source],
                target: vertices[target],
                weight: -1.0,
            });
        }
    }
    edges.push(ProjectionEdge {
        eid: EId(edges.len() as u128),
        source: vertices[width],
        target: vertices[width + 1],
        weight: -1.0,
    });
    for direction in [
        Directedness::Directed,
        Directedness::Reversed,
        Directedness::Undirected,
    ] {
        let view = graph(&vertices, &edges, direction);
        for cutoff in [None, Some(0), Some(1), Some(2)] {
            let call = FnxCallSpec::single_source_shortest_path_length(vertices[0], cutoff);
            let mut scalar_units = 0;
            let scalar = call
                .execute(&view, limits(), || {
                    scalar_units += 1;
                    Ok::<(), usize>(())
                })
                .unwrap();
            let mut meter = ComponentMeter::default();
            let batched = call.execute(&view, limits(), &mut meter).unwrap();
            assert_eq!(batched, scalar, "{direction:?} {cutoff:?}");
            assert_eq!(meter.observed, scalar_units);
            assert!(meter.calls < scalar_units);
            assert_eq!(meter.largest_run, FNX_METER_RUN);
            let reached = batched.rows.len();
            let witness = &batched.certificate.witness;
            // This graph selects pull only at the undirected wide frontier,
            // when the cutoff allows its successor. Pull truthfully includes
            // every candidate vertex visit and one certificate-hash unit per
            // recorded decision; scalar and batched meters observe the same
            // physical traversal and exact cancellation ordinal.
            let pulls = batched
                .certificate
                .bfs_decision_card
                .as_ref()
                .map(|card| card.pulls.as_slice())
                .unwrap_or(&[]);
            assert_eq!(
                !pulls.is_empty(),
                direction == Directedness::Undirected && (cutoff.is_none() || cutoff == Some(2))
            );
            let candidate_visits: usize = pulls.iter().map(|pull| pull.candidate_visits).sum();
            assert_eq!(witness.nodes_touched, reached + candidate_visits);
            assert!(
                witness.nodes_touched + witness.edges_scanned <= batched.certificate.estimated_work
            );
            assert_eq!(
                scalar_units,
                3 * vertices.len()
                    + witness.nodes_touched
                    + witness.edges_scanned
                    + 3
                    + pulls.len()
            );
            for stop in 1..=scalar_units {
                let mut meter = ComponentMeter {
                    stop: Some(stop),
                    ..ComponentMeter::default()
                };
                let result = call.execute(&view, limits(), &mut meter);
                assert!(matches!(result, Err(FnxExecutionError::Cancelled(at)) if at == stop));
                assert_eq!(meter.observed, stop);
                assert!(meter.stopped);
            }
        }
    }
}

#[test]
fn bfs_batching_does_not_charge_past_the_first_disallowed_discovery() {
    let n = 2 * FNX_METER_RUN + 5;
    let vertices: Vec<_> = (0..n).map(|index| VId(index as u128)).collect();
    // Source 0 first visits itself, then every other vertex in ID order.
    // The back edges force visits to already discovered vertices even after
    // an exact n-row allowance has been exhausted.
    let mut edges = Vec::new();
    for target in 0..n {
        for (source, target) in [(0, target), (target, 0)] {
            edges.push(ProjectionEdge {
                eid: EId(edges.len() as u128),
                source: vertices[source],
                target: vertices[target],
                weight: 1.0,
            });
        }
    }
    let view = graph(&vertices, &edges, Directedness::Directed);
    let call = FnxCallSpec::single_source_shortest_path_length(vertices[0], None);
    for row_limit in 1..n {
        let mut meter = ComponentMeter::default();
        let result = call.execute(
            &view,
            FnxExecutionLimits {
                max_result_rows: row_limit,
                ..limits()
            },
            &mut meter,
        );
        assert!(matches!(result, Err(FnxExecutionError::LimitExceeded {
            resource: "result rows", requested, limit,
        }) if requested == row_limit + 1 && limit == row_limit));
        // Initial call + degree pass + distance init +
        // popped source + loop + first row_limit discoveries (the last fails).
        let exact = 2 * n + row_limit + 3;
        assert_eq!(meter.observed, exact, "row allowance {row_limit}");
        // When the work budget crosses that same arc, its refusal must win.
        let mut meter = ComponentMeter {
            stop: Some(exact),
            ..ComponentMeter::default()
        };
        let result = call.execute(
            &view,
            FnxExecutionLimits {
                max_result_rows: row_limit,
                ..limits()
            },
            &mut meter,
        );
        assert!(matches!(result, Err(FnxExecutionError::Cancelled(at)) if at == exact));
        assert_eq!(meter.observed, exact);
    }
    let mut meter = ComponentMeter::default();
    let result = call
        .execute(
            &view,
            FnxExecutionLimits {
                max_result_rows: n,
                ..limits()
            },
            &mut meter,
        )
        .unwrap();
    assert_eq!(result.rows.len(), n);
    assert_eq!(result.certificate.witness.edges_scanned, 2 * n - 1);
    assert!(meter.full_runs >= 2 * (n / FNX_METER_RUN));
}

#[test]
fn component_runs_keep_every_work_refusal_and_witness_on_hubs() {
    // Two disconnected hubs cross the run boundary on both directed faces.
    // Full-width IDs, loops, duplicate edges and an isolate retain their
    // projection and canonical-label semantics under the same execution.
    let width = 2 * FNX_METER_RUN + 3;
    let mut vertices: Vec<_> = (0..2 * width)
        .map(|index| VId((index as u128) << 64 | 17))
        .collect();
    vertices.push(VId(u128::MAX));
    let mut edges = Vec::new();
    for root in [0, width] {
        for offset in 1..width {
            let (source, target) = if offset % 2 == 0 {
                (root, root + offset)
            } else {
                (root + offset, root)
            };
            edges.push(ProjectionEdge {
                eid: EId(edges.len() as u128),
                source: vertices[source],
                target: vertices[target],
                weight: -1.0,
            });
        }
        edges.push(ProjectionEdge {
            eid: EId(edges.len() as u128),
            source: vertices[root],
            target: vertices[root],
            weight: -1.0,
        });
    }
    edges.push(ProjectionEdge {
        eid: EId(edges.len() as u128),
        source: edges[0].source,
        target: edges[0].target,
        weight: -1.0,
    });
    let expected: BTreeMap<_, _> = vertices
        .iter()
        .enumerate()
        .map(|(index, &vertex)| {
            let component = if index < 2 * width {
                vertices[index / width * width]
            } else {
                vertex
            };
            (vertex, component)
        })
        .collect();
    for (call, direction) in [
        (
            FnxCallSpec::connected_components(),
            Directedness::Undirected,
        ),
        (
            FnxCallSpec::weakly_connected_components(),
            Directedness::Directed,
        ),
        (
            FnxCallSpec::weakly_connected_components(),
            Directedness::Reversed,
        ),
    ] {
        let view = graph(&vertices, &edges, direction);
        let mut unit_count = 0;
        let unit_result = call
            .execute(&view, limits(), || {
                unit_count += 1;
                Ok::<(), usize>(())
            })
            .unwrap();
        let mut meter = ComponentMeter::default();
        let run_result = call.execute(&view, limits(), &mut meter).unwrap();
        // Equality covers every row, result digest, certificate and witness.
        assert_eq!(run_result, unit_result, "{direction:?}");
        assert_eq!(meter.observed, unit_count);
        assert!(meter.calls < unit_count);
        assert_eq!(meter.largest_run, FNX_METER_RUN);
        assert!(meter.full_runs > vertices.len() / FNX_METER_RUN);
        let arcs: usize = (0..vertices.len())
            .map(|vertex| view.neighbors_indices(vertex).unwrap().len())
            .sum();
        let visits = arcs * if view.is_directed() { 2 } else { 1 };
        let witness = &run_result.certificate.witness;
        assert_eq!(witness.nodes_touched, vertices.len());
        assert_eq!(witness.edges_scanned, visits);
        assert_eq!(witness.queue_peak, width - 1);
        // Admission degree pass, labels, roots, reached vertices and output
        // rows each charge n units, plus arcs and three call boundaries.
        assert_eq!(unit_count, 5 * vertices.len() + visits + 3);
        assert_eq!(labels(run_result), expected);

        for stop in 1..=unit_count {
            let mut observed = 0;
            let unit_refusal = call.execute(&view, limits(), || {
                assert!(observed < stop, "no callback may follow a refusal");
                observed += 1;
                if observed == stop { Err(stop) } else { Ok(()) }
            });
            let mut meter = ComponentMeter {
                stop: Some(stop),
                ..ComponentMeter::default()
            };
            let run_refusal = call.execute(&view, limits(), &mut meter);
            assert!(matches!(unit_refusal, Err(FnxExecutionError::Cancelled(at)) if at == stop));
            assert!(matches!(run_refusal, Err(FnxExecutionError::Cancelled(at)) if at == stop));
            assert_eq!(observed, stop);
            assert_eq!(meter.observed, stop);
            assert!(meter.stopped);
        }
    }
}

#[test]
fn scc_public_runs_preserve_complete_results_admission_and_every_refusal() {
    let width = 2 * FNX_METER_RUN + 3;
    let n = width + 4;
    let vertices: Vec<_> = (0..n)
        .map(|vertex| VId((vertex as u128) << 64 | 31))
        .collect();
    let mut pairs = vec![(0, 0), (0, 1), (0, 1)];
    for vertex in 1..width {
        pairs.extend([(0, vertex), (vertex, 0), (vertex, (vertex + 1) % width)]);
    }
    // A one-way bridge separates two SCCs; the final vertex remains isolated.
    pairs.extend([
        (0, width),
        (width, width + 1),
        (width + 1, width + 2),
        (width + 2, width),
    ]);
    let edges: Vec<_> = pairs
        .iter()
        .enumerate()
        .map(|(eid, &(source, target))| ProjectionEdge {
            eid: EId((eid as u128) << 64 | 43),
            source: vertices[source],
            target: vertices[target],
            weight: -3.0,
        })
        .collect();
    let expected: BTreeMap<_, _> = vertices
        .iter()
        .enumerate()
        .map(|(vertex, &id)| {
            let minimum = if vertex < width {
                0
            } else if vertex < n - 1 {
                width
            } else {
                vertex
            };
            (id, vertices[minimum])
        })
        .collect();
    let call = FnxCallSpec::strongly_connected_components();
    for direction in [Directedness::Directed, Directedness::Reversed] {
        let view = graph(&vertices, &edges, direction);
        let mut scalar_units = 0;
        let scalar = call
            .execute(&view, limits(), || {
                scalar_units += 1;
                Ok::<(), usize>(())
            })
            .unwrap();
        let mut bulk = ComponentMeter::default();
        let actual = call.execute(&view, limits(), &mut bulk).unwrap();
        assert_eq!(actual, scalar, "every row and certificate field");
        assert_eq!(bulk.observed, scalar_units);
        assert!(bulk.calls < scalar_units);
        assert!(bulk.full_runs > n / FNX_METER_RUN);
        assert_eq!(bulk.largest_run, FNX_METER_RUN);
        let arcs: usize = (0..n)
            .map(|vertex| view.neighbors_indices(vertex).unwrap().len())
            .sum();
        assert_eq!(actual.certificate.witness.nodes_touched, 2 * n);
        assert_eq!(actual.certificate.witness.edges_scanned, 2 * arcs);
        assert_eq!(scalar_units, 8 * n + 2 * arcs + 3);
        assert_eq!(labels(actual.clone()), expected);

        for stop in 1..=scalar_units {
            let mut observed = 0;
            let scalar = call.execute(&view, limits(), || {
                assert!(observed < stop, "no scalar callback after refusal");
                observed += 1;
                if observed == stop { Err(stop) } else { Ok(()) }
            });
            let mut bulk = ComponentMeter {
                stop: Some(stop),
                ..ComponentMeter::default()
            };
            let candidate = call.execute(&view, limits(), &mut bulk);
            assert!(matches!(scalar, Err(FnxExecutionError::Cancelled(at)) if at == stop));
            assert!(matches!(candidate, Err(FnxExecutionError::Cancelled(at)) if at == stop));
            assert_eq!(observed, stop);
            assert_eq!(bulk.observed, stop);
            assert!(bulk.stopped);
        }
        for (row_limit, work_limit, resource, requested, units) in [
            (n - 1, usize::MAX, "result rows", n, 1),
            (
                n,
                actual.certificate.estimated_work - 1,
                "estimated work",
                actual.certificate.estimated_work,
                n + 1,
            ),
        ] {
            let mut meter = ComponentMeter::default();
            let refusal = call.execute(
                &view,
                FnxExecutionLimits {
                    max_result_rows: row_limit,
                    max_estimated_work: work_limit,
                    ..limits()
                },
                &mut meter,
            );
            assert!(matches!(refusal, Err(FnxExecutionError::LimitExceeded {
                resource: actual_resource, requested: actual_requested, ..
            }) if actual_resource == resource && actual_requested == requested));
            assert_eq!(
                meter.observed, units,
                "refuse before SCC allocation and work"
            );
        }
    }
}

#[test]
fn bfs_runs_preserve_discovery_refusals_and_every_work_prefix() {
    // A hub crosses two run boundaries, followed by a cycle and an isolate.
    // Duplicate edges and self loops must not consume result-row capacity.
    let n = 2 * FNX_METER_RUN + 7;
    let vertices: Vec<_> = (0..n)
        .map(|index| VId((index as u128) << 64 | 17))
        .collect();
    let mut pairs: Vec<_> = (1..n - 3).map(|target| (0, target)).collect();
    pairs.extend([
        (0, 0),
        (0, 1),
        (n - 4, n - 3),
        (n - 3, n - 2),
        (n - 2, n - 4),
        (n - 2, 1),
    ]);
    let edges: Vec<_> = pairs
        .iter()
        .enumerate()
        .map(|(eid, &(source, target))| ProjectionEdge {
            eid: EId(eid as u128),
            source: vertices[source],
            target: vertices[target],
            weight: -1.0,
        })
        .collect();

    for direction in [
        Directedness::Directed,
        Directedness::Reversed,
        Directedness::Undirected,
    ] {
        let view = graph(&vertices, &edges, direction);
        // An independent map/queue walk of the INPUT pairs pins both the
        // answer and the exact checkpoint preceding a row-limit refusal.
        // Comparing two meters alone would miss both prepaying past it.
        let mut adjacency = vec![std::collections::BTreeSet::new(); n];
        for &(source, target) in &pairs {
            match direction {
                Directedness::Directed => {
                    adjacency[source].insert(target);
                }
                Directedness::Reversed => {
                    adjacency[target].insert(source);
                }
                Directedness::Undirected => {
                    adjacency[source].insert(target);
                    adjacency[target].insert(source);
                }
            }
        }
        let source = if direction == Directedness::Reversed {
            n - 2
        } else {
            0
        };
        for cutoff in [Some(0), Some(1), None] {
            for cap in [0, 1, 2, 63, 64, 65, n - 4, n - 2, n - 1, n, n + 1] {
                let mut expected = BTreeMap::new();
                let mut refused = (cap == 0).then_some(1);
                let mut units = 1 + n; // call entry and degree admission
                if refused.is_none() {
                    units += n; // distance initialization
                    expected.insert(source, 0usize);
                    let mut queue = std::collections::VecDeque::from([source]);
                    'walk: while let Some(vertex) = queue.pop_front() {
                        units += 1;
                        let depth = expected[&vertex];
                        if cutoff.is_some_and(|limit| depth >= limit) {
                            continue;
                        }
                        for &neighbor in &adjacency[vertex] {
                            units += 1;
                            if expected.contains_key(&neighbor) {
                                continue;
                            }
                            if expected.len() == cap {
                                refused = Some(cap + 1);
                                break 'walk;
                            }
                            expected.insert(neighbor, depth + 1);
                            queue.push_back(neighbor);
                        }
                    }
                    if refused.is_none() {
                        units += n + 2; // result conversion and boundaries
                    }
                }
                let call =
                    FnxCallSpec::single_source_shortest_path_length(vertices[source], cutoff);
                let limits = FnxExecutionLimits {
                    max_result_rows: cap,
                    ..limits()
                };
                let mut meter = ComponentMeter::default();
                let result = call.execute(&view, limits, &mut meter);
                assert_eq!(meter.observed, units, "{direction:?} {cutoff:?} cap={cap}");
                if let Some(requested) = refused {
                    assert!(matches!(result, Err(FnxExecutionError::LimitExceeded {
                        resource: "result rows", requested: actual, ..
                    }) if actual == requested));
                } else {
                    let result = result.unwrap();
                    let expected: BTreeMap<_, _> = expected
                        .iter()
                        .map(|(&vertex, &depth)| (vertices[vertex], depth as u64))
                        .collect();
                    assert_eq!(distances(result.clone()), expected);
                    let mut scalar_units = 0;
                    let scalar = call
                        .execute(&view, limits, || {
                            scalar_units += 1;
                            Ok::<(), usize>(())
                        })
                        .unwrap();
                    assert_eq!(result, scalar); // every certificate field too
                    assert_eq!(scalar_units, units);
                    assert!(meter.calls < units);
                    if direction == Directedness::Directed && cutoff.is_none() && cap >= n {
                        assert!(
                            meter.full_runs >= 4,
                            "initialization and hub arcs both batch"
                        );
                    }
                }

                // Include the first ordinal AFTER a native refusal. It must
                // stay a row refusal, never become a later work cancellation.
                for stop in 1..=units + 1 {
                    let mut meter = ComponentMeter {
                        stop: Some(stop),
                        ..ComponentMeter::default()
                    };
                    let result = call.execute(&view, limits, &mut meter);
                    if stop <= units {
                        assert!(
                            matches!(result, Err(FnxExecutionError::Cancelled(at)) if at == stop)
                        );
                        assert_eq!(meter.observed, stop);
                    } else {
                        assert_eq!(meter.observed, units);
                        if let Some(requested) = refused {
                            assert!(matches!(result, Err(FnxExecutionError::LimitExceeded {
                                resource: "result rows", requested: actual, ..
                            }) if actual == requested));
                        } else {
                            assert!(result.is_ok());
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn every_three_vertex_projection_matches_pinned_fnx_traversals() {
    let vertices = [VId(0), VId(17), VId(u128::MAX)];
    for mask in 0u16..512 {
        let edges: Vec<_> = (0..9)
            .filter(|bit| mask & (1 << bit) != 0)
            .map(|bit| ProjectionEdge {
                eid: EId(bit as u128),
                source: vertices[bit / 3],
                target: vertices[bit % 3],
                // Connectivity ignores projected weights, including negative ones.
                weight: -1.0,
            })
            .collect();
        for direction in [
            Directedness::Directed,
            Directedness::Reversed,
            Directedness::Undirected,
        ] {
            let view = graph(&vertices, &edges, direction);
            let mut directed = DiGraph::strict();
            let mut undirected = Graph::strict();
            for name in view.nodes_ordered() {
                let _ = directed.add_node(name);
                let _ = undirected.add_node(name);
            }
            for source in 0..3 {
                for &target in view.neighbors_indices(source).unwrap() {
                    let from = view.get_node_name(source).unwrap();
                    let to = view.get_node_name(target).unwrap();
                    directed.add_edge(from, to).unwrap();
                    if source <= target {
                        undirected.add_edge(from, to).unwrap();
                    }
                }
            }
            for source in 0..3 {
                for cutoff in [None, Some(0), Some(1), Some(2)] {
                    let name = view.get_node_name(source).unwrap();
                    let oracle = if view.is_directed() {
                        fnx_algorithms::single_source_shortest_path_length_directed(
                            &directed, name, cutoff,
                        )
                    } else {
                        fnx_algorithms::single_source_shortest_path_length(
                            &undirected,
                            name,
                            cutoff,
                        )
                    };
                    let expected: BTreeMap<_, _> = oracle
                        .into_iter()
                        .map(|(name, depth)| {
                            (
                                view.vertex_id(view.get_node_index(&name).unwrap()).unwrap(),
                                depth as u64,
                            )
                        })
                        .collect();
                    let actual = execute(
                        &FnxCallSpec::single_source_shortest_path_length(vertices[source], cutoff),
                        &view,
                    );
                    assert!(
                        actual
                            .rows
                            .windows(2)
                            .all(|pair| match (&pair[0][0], &pair[1][0]) {
                                (FnxValue::Vertex(a), FnxValue::Vertex(b)) => a < b,
                                _ => false,
                            })
                    );
                    assert_eq!(
                        distances(actual),
                        expected,
                        "mask={mask}, {direction:?}, source={source}, cutoff={cutoff:?}"
                    );
                }
            }
            if view.is_directed() {
                assert_eq!(
                    labels(execute(&FnxCallSpec::weakly_connected_components(), &view)),
                    oracle_labels(
                        fnx_algorithms::weakly_connected_components(&directed),
                        &view
                    ),
                    "WCC mask={mask} {direction:?}"
                );
                assert_eq!(
                    labels(execute(
                        &FnxCallSpec::strongly_connected_components(),
                        &view
                    )),
                    oracle_labels(
                        fnx_algorithms::strongly_connected_components(&directed),
                        &view
                    ),
                    "SCC mask={mask} {direction:?}"
                );
            } else {
                assert_eq!(
                    labels(execute(&FnxCallSpec::connected_components(), &view)),
                    oracle_labels(
                        fnx_algorithms::connected_components(&undirected).components,
                        &view
                    ),
                    "CC mask={mask}"
                );
            }
        }
    }
}

#[test]
fn full_width_source_parameters_literals_defaults_and_yield_are_bound() {
    let vertex = VId(u128::MAX);
    let mut parameters = FnxParameters::new();
    parameters.insert("source".to_owned(), FnxArgument::Vertex(vertex));
    let call = FnxCallSpec::bind(
        "CALL fnx.single_source_shortest_path_length($source)",
        &parameters,
    )
    .unwrap();
    let literal = bind(&format!(
        "CALL fnx.single_source_shortest_path_length({},NULL) YIELD *",
        vertex.0
    ));
    assert_eq!(call, literal);
    assert_eq!(
        call,
        FnxCallSpec::single_source_shortest_path_length(vertex, None)
    );
    assert!(call.options().is_none());
    parameters.insert("source".to_owned(), FnxArgument::Vertex(VId(0)));
    assert_eq!(
        call.algorithm(),
        FnxAlgorithm::SingleSourceShortestPathLength {
            source: vertex,
            cutoff: None
        }
    );
    assert_ne!(
        call.digest(),
        FnxCallSpec::single_source_shortest_path_length(VId(0), None).digest()
    );
    assert_ne!(
        call.digest(),
        FnxCallSpec::single_source_shortest_path_length(vertex, Some(0)).digest()
    );
    let alias = bind(&format!(
        "CALL fnx.single_source_shortest_path_length({},0) YIELD distance AS hops,vertex AS id;",
        vertex.0
    ));
    let result = execute(
        &alias,
        &graph(&[VId(0), vertex], &[], Directedness::Directed),
    );
    assert_eq!(result.columns, vec!["hops", "id"]);
    assert_eq!(
        result.rows,
        vec![vec![FnxValue::Integer(0), FnxValue::Vertex(vertex)]]
    );
    let component = bind("CALL fnx.strongly_connected_components() YIELD component AS group_id");
    assert_eq!(
        execute(&component, &graph(&[vertex], &[], Directedness::Directed)).rows,
        vec![vec![FnxValue::Vertex(vertex)]]
    );
}

#[test]
fn registered_signatures_reject_cross_schema_fields_wrong_types_and_missing_arguments() {
    for text in [
        "CALL fnx.single_source_shortest_path_length()",
        "CALL fnx.single_source_shortest_path_length(-1)",
        "CALL fnx.single_source_shortest_path_length(1.0)",
        "CALL fnx.single_source_shortest_path_length(NULL)",
        "CALL fnx.single_source_shortest_path_length(true)",
        "CALL fnx.single_source_shortest_path_length(0,-1)",
        "CALL fnx.single_source_shortest_path_length(0,1.0)",
        "CALL fnx.single_source_shortest_path_length(0,NULL,1)",
        "CALL fnx.single_source_shortest_path_length(0) YIELD score",
        "CALL fnx.single_source_shortest_path_length(0) YIELD vertex AS x,distance AS x",
        "CALL fnx.single_source_shortest_path_length(340282366920938463463374607431768211456)",
        "CALL fnx.connected_components(1)",
        "CALL fnx.connected_components() YIELD distance",
        "CALL fnx.weakly_connected_components() YIELD component,component",
        "CALL fnx.strongly_connected_components() YIELD score",
        "CALL fnx.pagerank() YIELD component",
    ] {
        assert!(
            FnxCallSpec::bind(text, &FnxParameters::new()).is_err(),
            "{text}"
        );
    }
    assert_eq!(
        FnxCallSpec::bind(
            "CALL fnx.single_source_shortest_path_length()",
            &FnxParameters::new()
        )
        .unwrap_err()
        .kind,
        FnxBindErrorKind::MissingArgument("source")
    );
    let signature = FnxSignatureRegistry::lookup("fnx.single_source_shortest_path_length").unwrap();
    assert_eq!(signature.parameters[0].default, None);
    assert_eq!(signature.parameters[1].default, Some(FnxArgument::Null));
    for signature in FnxSignatureRegistry::signatures() {
        assert_eq!(signature.graph_input_arity, 1);
        assert_eq!(
            signature.implementation,
            FnxImplementationClass::InCoreDecodedCache
        );
        assert!(!signature.execution_kernel.is_empty());
    }
}

#[test]
fn incompatible_directions_and_missing_source_fail_without_a_result() {
    let directed = graph(&[VId(0)], &[], Directedness::Directed);
    let undirected = graph(&[VId(0)], &[], Directedness::Undirected);
    for (call, view, required) in [
        (
            FnxCallSpec::connected_components(),
            &directed,
            FnxGraphKind::Undirected,
        ),
        (
            FnxCallSpec::weakly_connected_components(),
            &undirected,
            FnxGraphKind::Directed,
        ),
        (
            FnxCallSpec::strongly_connected_components(),
            &undirected,
            FnxGraphKind::Directed,
        ),
    ] {
        match call.execute(view, limits(), || Ok::<(), Infallible>(())) {
            Err(FnxExecutionError::GraphKind { required: actual }) => assert_eq!(actual, required),
            other => panic!("graph kind refusal required: {other:?}"),
        }
    }
    assert!(matches!(
        FnxCallSpec::single_source_shortest_path_length(VId(u128::MAX), None).execute(
            &directed,
            limits(),
            || Ok::<(), Infallible>(())
        ),
        Err(FnxExecutionError::UnknownSource(VId(u128::MAX)))
    ));
}

#[test]
fn empty_graphs_and_isolates_are_preserved_by_every_component_kernel() {
    for (call, direction) in [
        (
            FnxCallSpec::connected_components(),
            Directedness::Undirected,
        ),
        (
            FnxCallSpec::weakly_connected_components(),
            Directedness::Directed,
        ),
        (
            FnxCallSpec::strongly_connected_components(),
            Directedness::Directed,
        ),
    ] {
        let empty = execute(&call, &graph(&[], &[], direction));
        assert!(empty.rows.is_empty());
        assert_eq!(empty.certificate.estimated_work, 0);
        assert_eq!(empty.certificate.kernel_workspace_bytes, 0);
        let isolated = graph(&[VId(u128::MAX), VId(0)], &[], direction);
        assert_eq!(
            labels(execute(&call, &isolated)),
            BTreeMap::from([(VId(0), VId(0)), (VId(u128::MAX), VId(u128::MAX))])
        );
    }
}

#[test]
fn result_and_work_limits_are_enforced_without_convergence_limits_on_traversals() {
    let view = graph(
        &[VId(0), VId(1), VId(2)],
        &[ProjectionEdge {
            eid: EId(0),
            source: VId(0),
            target: VId(1),
            weight: 1.0,
        }],
        Directedness::Directed,
    );
    let isolated = FnxCallSpec::single_source_shortest_path_length(VId(2), None);
    let one_row = FnxExecutionLimits {
        max_result_rows: 1,
        ..limits()
    };
    assert_eq!(
        isolated
            .execute(&view, one_row, || Ok::<(), Infallible>(()))
            .unwrap()
            .rows
            .len(),
        1
    );
    let reachable = FnxCallSpec::single_source_shortest_path_length(VId(0), None);
    assert!(matches!(
        reachable.execute(&view, one_row, || Ok::<(), Infallible>(())),
        Err(FnxExecutionError::LimitExceeded {
            resource: "result rows",
            requested: 2,
            ..
        })
    ));
    let cutoff = FnxCallSpec::single_source_shortest_path_length(VId(0), Some(0));
    assert_eq!(
        cutoff
            .execute(&view, one_row, || Ok::<(), Infallible>(()))
            .unwrap()
            .rows
            .len(),
        1
    );
    for (call, work) in [
        (reachable, 4),
        (FnxCallSpec::weakly_connected_components(), 5),
        (FnxCallSpec::strongly_connected_components(), 8),
    ] {
        let cap = FnxExecutionLimits {
            max_estimated_work: work - 1,
            ..limits()
        };
        assert!(matches!(
            call.execute(&view, cap, || Ok::<(), Infallible>(())),
            Err(FnxExecutionError::LimitExceeded {
                resource: "estimated work",
                ..
            })
        ));
        let cap = FnxExecutionLimits {
            max_estimated_work: work,
            ..limits()
        };
        let result = call
            .execute(&view, cap, || Ok::<(), Infallible>(()))
            .unwrap();
        assert_eq!(result.certificate.estimated_work, work);
        assert!(
            result.certificate.witness.nodes_touched + result.certificate.witness.edges_scanned
                <= work
        );
    }
}

#[test]
fn all_traversal_checkpoints_abort_without_exposing_partial_rows() {
    let vertices = [VId(0), VId(1), VId(2), VId(3)];
    let edges: Vec<_> = [(0, 1), (1, 0), (1, 2), (2, 2)]
        .into_iter()
        .enumerate()
        .map(|(i, (s, t))| ProjectionEdge {
            eid: EId(i as u128),
            source: VId(s),
            target: VId(t),
            weight: 0.0,
        })
        .collect();
    for (call, direction) in [
        (
            FnxCallSpec::single_source_shortest_path_length(VId(0), None),
            Directedness::Directed,
        ),
        (
            FnxCallSpec::connected_components(),
            Directedness::Undirected,
        ),
        (
            FnxCallSpec::weakly_connected_components(),
            Directedness::Directed,
        ),
        (
            FnxCallSpec::strongly_connected_components(),
            Directedness::Directed,
        ),
    ] {
        let view = graph(&vertices, &edges, direction);
        let mut total = 0;
        let complete = call
            .execute(&view, limits(), || {
                total += 1;
                Ok::<(), &'static str>(())
            })
            .unwrap();
        assert!(total > 20);
        for stop in 1..=total {
            let mut observed = 0;
            let result = call.execute(&view, limits(), || {
                observed += 1;
                if observed == stop {
                    Err("cancelled")
                } else {
                    Ok(())
                }
            });
            assert!(matches!(
                result,
                Err(FnxExecutionError::Cancelled("cancelled"))
            ));
            assert_eq!(observed, stop);
        }
        assert_eq!(complete, execute(&call, &view.clone()));
        assert_eq!(complete.certificate.numeric_profile, FNX_DISCRETE_PROFILE);
        assert_eq!(
            complete.certificate.execution_kernel,
            call.signature().execution_kernel
        );
    }
}

#[test]
fn strong_components_handle_a_deep_chain_and_large_cycle_without_recursion() {
    let n = 25_000;
    let vertices: Vec<_> = (0..n).map(|i| VId(i as u128)).collect();
    let mut edges: Vec<_> = (1..n)
        .map(|i| ProjectionEdge {
            eid: EId(i as u128),
            source: vertices[i - 1],
            target: vertices[i],
            weight: 1.0,
        })
        .collect();
    let call = FnxCallSpec::strongly_connected_components();
    let chain = execute(&call, &graph(&vertices, &edges, Directedness::Directed));
    assert_eq!(chain.certificate.witness.nodes_touched, 2 * n);
    assert_eq!(chain.certificate.witness.edges_scanned, 2 * (n - 1));
    assert!(
        labels(chain)
            .into_iter()
            .all(|(vertex, component)| vertex == component)
    );
    edges.push(ProjectionEdge {
        eid: EId(0),
        source: vertices[n - 1],
        target: vertices[0],
        weight: 1.0,
    });
    let cycle = execute(&call, &graph(&vertices, &edges, Directedness::Directed));
    assert!(labels(cycle).values().all(|&component| component == VId(0)));
}

fn cdlp_result(view: &SnapshotGraphView, max_iter: usize) -> FnxResult {
    FnxCallSpec::cdlp(max_iter)
        .execute(
            view,
            FnxExecutionLimits {
                max_iterations: max_iter,
                ..limits()
            },
            || Ok::<(), Infallible>(()),
        )
        .unwrap()
}

/// Exact official outputs, not regenerated golden values. Source:
/// https://github.com/ldbc/ldbc_graphalytics/tree/7b8bde76cf7aab5e90b25ecd4b38829e2f98b292/graphalytics-validation/src/main/resources/validation-graphs
/// `cdlp/{dir,undir}-{input,output}` use five rounds; the two
/// `example/example-{directed,undirected}-{input,CDLP}` pairs use two.
#[test]
fn cdlp_matches_all_four_pinned_graphalytics_validation_outputs() {
    for (name, input, direction, max_iter, expected) in [
        (
            "cdlp directed",
            "1 2 3 7\n2 1 3\n3 1 2\n4 5 6\n5 4 6 7\n6 5 7\n7 5 6 8\n8 6",
            Directedness::Directed,
            5,
            [1u128, 1, 1, 5, 4, 4, 4, 4].as_slice(),
        ),
        (
            "cdlp undirected",
            "1 2 3 7\n2 1 3\n3 1 2\n4 5 6 8\n5 4 6 7 8\n6 4 5 7 8\n7 1 5 6 8\n8 4 5 6 7",
            Directedness::Undirected,
            5,
            [1, 1, 1, 4, 4, 4, 4, 4].as_slice(),
        ),
        (
            "example directed",
            "1 3 5\n2 4 5 10\n3 1 5 8 10\n4\n5 3 4 8\n6 3 4\n7 4\n8 1\n9 4\n10",
            Directedness::Directed,
            2,
            [1, 2, 3, 4, 1, 1, 2, 3, 2, 1].as_slice(),
        ),
        (
            "example undirected",
            "2 3 4\n3 2 4 5 8\n4 2 3\n5 3 6 8\n6 5 7 8 9 10\n7 6 9\n8 3 5 6\n9 6 7\n10 6",
            Directedness::Undirected,
            2,
            [2, 3, 2, 2, 6, 5, 2, 5, 5].as_slice(),
        ),
    ] {
        let mut vertices = Vec::new();
        let mut edges = Vec::new();
        for line in input.lines() {
            let mut fields = line
                .split_whitespace()
                .map(|field| field.parse::<u128>().unwrap());
            let source = fields.next().unwrap();
            vertices.push(VId(source));
            for target in fields {
                // The official undirected input lists both directions.
                if direction == Directedness::Undirected && source > target {
                    continue;
                }
                edges.push(ProjectionEdge {
                    eid: EId(edges.len() as u128),
                    source: VId(source),
                    target: VId(target),
                    weight: -3.0,
                });
            }
        }
        assert_eq!(vertices.len(), expected.len());
        let reference: BTreeMap<_, _> = vertices
            .iter()
            .copied()
            .zip(expected.iter().copied().map(VId))
            .collect();
        let view = graph(&vertices, &edges, direction);
        let output = cdlp_result(&view, max_iter);
        assert_eq!(
            output.certificate.witness.nodes_touched,
            vertices.len() * max_iter
        );
        assert_eq!(
            output.certificate.witness.edges_scanned,
            2 * view.edge_count() * max_iter
        );
        assert_eq!(labels(output), reference, "{name}");
    }
}

/// Deliberately slow full-VId oracle: inspect every vertex pair against the
/// original edge list, then build ordered label histograms from scratch.
/// It shares neither CSR rows, dense labels nor histogram epochs with CDLP.
fn cdlp_oracle(
    vertices: &[VId],
    edges: &[ProjectionEdge],
    direction: Directedness,
    max_iter: usize,
) -> BTreeMap<VId, VId> {
    let mut previous: BTreeMap<_, _> = vertices.iter().map(|&vertex| (vertex, vertex)).collect();
    for _ in 0..max_iter {
        let mut next = BTreeMap::new();
        for &vertex in vertices {
            let mut frequencies: BTreeMap<VId, usize> = BTreeMap::new();
            for &neighbor in vertices {
                let forward = edges
                    .iter()
                    .any(|edge| edge.source == vertex && edge.target == neighbor);
                let backward = edges
                    .iter()
                    .any(|edge| edge.target == vertex && edge.source == neighbor);
                let votes = if direction == Directedness::Undirected {
                    usize::from(forward || backward)
                } else {
                    usize::from(forward) + usize::from(backward)
                };
                if votes != 0 {
                    *frequencies.entry(previous[&neighbor]).or_default() += votes;
                }
            }
            let maximum = frequencies.values().copied().max().unwrap_or(0);
            let label = frequencies
                .iter()
                .find_map(|(&label, &count)| (count == maximum).then_some(label))
                .unwrap_or(previous[&vertex]);
            next.insert(vertex, label);
        }
        previous = next;
    }
    previous
}

#[test]
fn cdlp_matches_scalar_oracle_on_every_three_vertex_graph_and_iteration() {
    // Unsorted IDs, including two with the same low bits: a label may never
    // be truncated to a machine word or compared as its decimal spelling.
    let vertices = [VId(u128::MAX), VId(7), VId((1u128 << 96) + 7)];
    let pairs: Vec<_> = (0..3)
        .flat_map(|a| (0..3).filter(move |&b| a != b).map(move |b| (a, b)))
        .collect();
    for mask in 0usize..1 << pairs.len() {
        let edges: Vec<_> = pairs
            .iter()
            .enumerate()
            .filter(|(bit, _)| mask & (1 << bit) != 0)
            .map(|(bit, &(a, b))| ProjectionEdge {
                eid: EId(bit as u128),
                source: vertices[a],
                target: vertices[b],
                weight: [-9.0, 0.0, 41.0][bit % 3],
            })
            .collect();
        for direction in [
            Directedness::Directed,
            Directedness::Reversed,
            Directedness::Undirected,
        ] {
            let view = graph(&vertices, &edges, direction);
            for max_iter in 0..=5 {
                let output = cdlp_result(&view, max_iter);
                assert_eq!(output.certificate.witness.nodes_touched, 3 * max_iter);
                assert_eq!(
                    labels(output),
                    cdlp_oracle(&vertices, &edges, direction, max_iter),
                    "mask={mask}, direction={direction:?}, max_iter={max_iter}",
                );
            }
        }
    }
}

#[test]
fn cdlp_preserves_bipartite_oscillation_and_isolate_labels() {
    let vertices = [VId(3), VId(17), VId(u128::MAX)];
    let edges = [ProjectionEdge {
        eid: EId(0),
        source: vertices[0],
        target: vertices[1],
        weight: 1.0,
    }];
    let view = graph(&vertices, &edges, Directedness::Directed);
    for max_iter in 0..=6 {
        let expected = if max_iter % 2 == 0 {
            [vertices[0], vertices[1], vertices[2]]
        } else {
            [vertices[1], vertices[0], vertices[2]]
        };
        let output = cdlp_result(&view, max_iter);
        assert_eq!(output.certificate.witness.nodes_touched, 3 * max_iter);
        assert_eq!(labels(output), vertices.into_iter().zip(expected).collect());
    }
    let empty = graph(&[], &[], Directedness::Directed);
    let output = cdlp_result(&empty, usize::MAX);
    assert!(output.rows.is_empty());
    assert_eq!(output.certificate.witness.nodes_touched, 0);
    assert_eq!(output.certificate.witness.edges_scanned, 0);
}

#[test]
fn cdlp_charged_work_covers_admission_initialization_and_every_refusal() {
    let vertices = [VId(5), VId(9), VId(17), VId(u128::MAX)];
    let edges: Vec<_> = [(0, 1), (1, 0), (1, 2), (2, 0)]
        .into_iter()
        .enumerate()
        .map(|(index, (a, b))| ProjectionEdge {
            eid: EId(index as u128),
            source: vertices[a],
            target: vertices[b],
            weight: 1.0,
        })
        .collect();
    for direction in [
        Directedness::Directed,
        Directedness::Reversed,
        Directedness::Undirected,
    ] {
        let view = graph(&vertices, &edges, direction);
        for max_iter in [0, 3] {
            let call = FnxCallSpec::cdlp(max_iter);
            let allowed = FnxExecutionLimits {
                max_iterations: max_iter,
                ..limits()
            };
            let mut units = 0;
            let scalar = call
                .execute(&view, allowed, || {
                    units += 1;
                    Ok::<(), usize>(())
                })
                .unwrap();
            assert_eq!(scalar.certificate.estimated_work, units);
            let mut meter = ComponentMeter::default();
            assert_eq!(call.execute(&view, allowed, &mut meter).unwrap(), scalar);
            assert_eq!(meter.observed, units);
            for stop in 1..=units {
                let mut meter = ComponentMeter {
                    stop: Some(stop),
                    ..ComponentMeter::default()
                };
                assert!(matches!(
                    call.execute(&view, allowed, &mut meter),
                    Err(FnxExecutionError::Cancelled(at)) if at == stop
                ));
                assert_eq!(meter.observed, stop);
                assert!(meter.stopped);
            }
            for (row_limit, work_limit, expected_resource) in [
                (vertices.len() - 1, units, "result rows"),
                (vertices.len(), units - 1, "estimated work"),
            ] {
                assert!(matches!(
                    call.execute(&view, FnxExecutionLimits {
                        max_result_rows: row_limit,
                        max_estimated_work: work_limit,
                        ..allowed
                    }, || Ok::<(), Infallible>(())),
                    Err(FnxExecutionError::LimitExceeded { resource, .. }) if resource == expected_resource
                ));
            }
            assert_eq!(
                call.execute(
                    &view,
                    FnxExecutionLimits {
                        max_result_rows: vertices.len(),
                        max_estimated_work: units,
                        ..allowed
                    },
                    || Ok::<(), Infallible>(())
                )
                .unwrap(),
                scalar,
            );
        }
        assert!(matches!(
            FnxCallSpec::cdlp(3).execute(&view, limits(), || Ok::<(), Infallible>(())),
            Err(FnxExecutionError::LimitExceeded {
                resource: "iterations",
                ..
            })
        ));
        assert!(matches!(
            FnxCallSpec::cdlp(usize::MAX).execute(
                &view,
                FnxExecutionLimits {
                    max_iterations: usize::MAX,
                    max_estimated_work: usize::MAX,
                    ..limits()
                },
                || Ok::<(), Infallible>(())
            ),
            Err(FnxExecutionError::SizeOverflow)
        ));
    }
}

#[test]
fn cdlp_batches_high_degree_rows_and_refuses_retained_self_loops() {
    let vertices: Vec<_> = (0..FNX_METER_RUN + 5)
        .map(|v| VId((v as u128) << 64))
        .collect();
    let edges: Vec<_> = (1..vertices.len())
        .map(|target| ProjectionEdge {
            eid: EId(target as u128),
            source: vertices[0],
            target: vertices[target],
            weight: -1.0,
        })
        .collect();
    let view = graph(&vertices, &edges, Directedness::Directed);
    let call = FnxCallSpec::cdlp(2);
    let allowed = FnxExecutionLimits {
        max_iterations: 2,
        ..limits()
    };
    let mut meter = ComponentMeter::default();
    let output = call.execute(&view, allowed, &mut meter).unwrap();
    assert_eq!(meter.largest_run, FNX_METER_RUN);
    assert!(meter.calls < meter.observed);
    assert_eq!(meter.observed, output.certificate.estimated_work);
    assert_eq!(
        labels(output),
        vertices
            .iter()
            .map(|&v| (v, if v == vertices[0] { v } else { vertices[1] }))
            .collect()
    );
    for stop in [
        FNX_METER_RUN,
        FNX_METER_RUN + 1,
        meter.observed - 1,
        meter.observed,
    ] {
        let mut cancelled = ComponentMeter {
            stop: Some(stop),
            ..ComponentMeter::default()
        };
        assert!(matches!(
            call.execute(&view, allowed, &mut cancelled),
            Err(FnxExecutionError::Cancelled(at)) if at == stop
        ));
        assert_eq!(cancelled.observed, stop);
    }
    let loop_edge = [ProjectionEdge {
        eid: EId(0),
        source: vertices[0],
        target: vertices[0],
        weight: -1.0,
    }];
    let with_loop = graph(&vertices[..1], &loop_edge, Directedness::Directed);
    for max_iter in [0, 2] {
        assert!(matches!(
            FnxCallSpec::cdlp(max_iter).execute(&with_loop, allowed, || Ok::<(), Infallible>(())),
            Err(FnxExecutionError::SelfLoopRefused)
        ));
    }
    let dropped = SnapshotGraphView::build(
        with_loop.binding(),
        &vertices[..1],
        &loop_edge,
        ProjectionSpec {
            self_loops: SelfLoopPolicy::Drop,
            ..with_loop.spec()
        },
        ProjectionLimits {
            max_vertices: 1,
            max_input_edges: 1,
            max_adjacency_entries: 2,
            max_workspace_bytes: 1 << 20,
        },
    )
    .unwrap();
    assert_eq!(
        labels(cdlp_result(&dropped, 2)),
        BTreeMap::from([(vertices[0], vertices[0])])
    );
}

#[test]
fn only_a_tail_after_the_yield_list_makes_a_call_a_read_pipeline() {
    for text in [
        "CALL fnx.pagerank() YIELD vertex RETURN vertex",
        "CALL fnx.pagerank() YIELD vertex AS n MATCH (n) RETURN n",
        "CALL fnx.single_source_shortest_path_length($s) YIELD vertex, distance \
         WHERE distance > 1 RETURN vertex",
        "CALL fnx.pagerank() RETURN 1 AS x",
    ] {
        assert!(FnxCallSpec::continues_past_call(text), "{text}");
    }
    for text in [
        "CALL fnx.pagerank()",
        "CALL fnx.pagerank() YIELD vertex, score AS rank;",
        "CALL fnx.single_source_shortest_path_length($s) YIELD *",
        // Malformed or unknown: the standalone binder owns the refusal.
        "CALL fnx.no_such_procedure() YIELD vertex RETURN vertex",
        "CALL fnx.pagerank(",
    ] {
        assert!(!FnxCallSpec::continues_past_call(text), "{text}");
    }
    // Structure only: an unbound parameter still binds as missing.
    assert_eq!(
        FnxCallSpec::bind(
            "CALL fnx.single_source_shortest_path_length($s) YIELD vertex",
            &FnxParameters::new()
        )
        .unwrap_err()
        .kind,
        FnxBindErrorKind::MissingParameter
    );
}
