//! Semi-synchronous label propagation over authenticated compressed rows.
//! The shared state retains five integer vectors and no copied adjacency;
//! every coloring, voting, and representative step can yield or be cancelled.

use super::{
    Cursor, Delivery, Finished, FnxCallSpec, FnxExecutionLimits, FnxMemoryLimits, KernelOutput,
    Result, ResultAdmission, Rows, SealedGraphView, SealedRows, admit, checkpoint,
    directional_pass_work, finish, finish_encoded,
};
use crate::label_propagation::{Iteration, MAX_ITERATIONS, estimated_work, workspace};
use crate::sealed_control::{Control, Guard};
use std::convert::Infallible;
use std::future::Future;

fn prepare(
    call: &FnxCallSpec,
    graph: &SealedGraphView,
    limits: FnxExecutionLimits,
    memory: FnxMemoryLimits,
    admission: &ResultAdmission,
) -> Result<usize> {
    admit("iterations", MAX_ITERATIONS, limits.max_iterations)?;
    let n = graph.node_count();
    admission.rows(n)?;
    let pass = directional_pass_work(n, graph.scan_incidence_bound(), graph.spec().directedness)?;
    let estimated =
        estimated_work::<Infallible>(n, graph.adjacency_entry_count(), pass, call.outputs().len())?;
    admit("estimated work", estimated, limits.max_estimated_work)?;
    admit(
        "kernel workspace bytes",
        workspace::<Infallible>(n)?,
        memory.max_kernel_workspace_bytes,
    )?;
    Ok(estimated)
}

fn run(
    graph: &impl Rows,
    limit: usize,
    control: &mut impl FnMut() -> Result<()>,
) -> Result<(KernelOutput, usize)> {
    control()?;
    let mut state = Iteration::new::<Infallible>(graph.node_count(), limit)?;
    while !state.complete() {
        control()?;
        if let Some(vertex) = state.row() {
            let mut row = graph.open(vertex)?;
            while let Some((neighbor, _)) = row.next()? {
                control()?;
                state.visit::<Infallible>(neighbor)?;
            }
            state.end_row::<Infallible>()?;
        } else {
            state.scalar_step::<Infallible>()?;
        }
    }
    control()?;
    Ok(state.finish())
}

fn compressed_witness(output: &mut KernelOutput) {
    output
        .witness
        .complexity_claim
        .push_str(" plus O((k + 1) * (|V| log(1+H) + H log(1+|V|))) compressed row visits");
}

pub(super) fn execute(
    call: &FnxCallSpec,
    cx: &Control<'_, impl Guard>,
    graph: &SealedGraphView,
    limits: FnxExecutionLimits,
    memory: FnxMemoryLimits,
    admission: &ResultAdmission,
) -> Result<Finished> {
    let estimated = prepare(call, graph, limits, memory, admission)?;
    let rows = SealedRows { cx, graph };
    let mut control = || checkpoint(cx);
    let (mut output, peak) = run(&rows, memory.max_kernel_workspace_bytes, &mut control)?;
    compressed_witness(&mut output);
    finish(
        call,
        graph,
        output,
        "fgdb-prism/sealed-label-propagation-semisynchronous-v1",
        estimated,
        peak,
        admission,
        &mut control,
    )
}

pub(super) async fn execute_cooperative<Yield, YieldFuture>(
    call: &FnxCallSpec,
    graph: &SealedGraphView,
    limits: FnxExecutionLimits,
    memory: FnxMemoryLimits,
    delivery: Delivery,
    control: &mut super::cooperative::Cooperate<'_, Yield, impl Guard>,
) -> Result<Finished>
where
    Yield: FnMut() -> YieldFuture,
    YieldFuture: Future<Output = ()>,
{
    let admission = ResultAdmission::for_delivery(call, limits, memory, delivery)?;
    let estimated = prepare(call, graph, limits, memory, &admission)?;
    control.tick().await?;
    let n = graph.node_count();
    let mut state = Iteration::new::<Infallible>(n, memory.max_kernel_workspace_bytes)?;
    while !state.complete() {
        control.tick().await?;
        if let Some(vertex) = state.row() {
            let mut row = graph.neighbor_cursor_controlled(control.cx, vertex, None)?;
            while let Some((neighbor, _)) = control.next(&mut row).await? {
                control.tick().await?;
                state.visit::<Infallible>(neighbor)?;
            }
            state.end_row::<Infallible>()?;
        } else {
            state.scalar_step::<Infallible>()?;
        }
    }
    control.tick().await?;
    let (mut output, peak) = state.finish();
    compressed_witness(&mut output);
    let mut encoded = super::EncodedRows::new(call, n, &admission)?;
    for vertex in 0..n {
        control.tick().await?;
        encoded.push(call, graph, &output.values, vertex, &mut || {
            checkpoint(control.cx)
        })?;
    }
    control.tick().await?;
    finish_encoded(
        call,
        graph,
        encoded,
        output.values,
        output.witness,
        "fgdb-prism/sealed-label-propagation-semisynchronous-cooperative-v1",
        estimated,
        peak,
        &mut || checkpoint(control.cx),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execute::KernelValues;
    use crate::{
        Directedness, FNX_METER_RUN, FnxExecutionError, FnxMeter, FnxParameters, FnxValue,
        GraphView, ParallelEdgePolicy, ProjectionEdge, ProjectionLimits, ProjectionSpec,
        SealedProjectionError, SelfLoopPolicy, SnapshotBinding, SnapshotGraphView,
    };
    use fgdb_types::{CommitSeq, EId, VId, ids::ObjectId};
    use std::cell::Cell;
    use std::collections::BTreeMap;

    struct Fixture {
        graph: SnapshotGraphView,
        source_steps: Cell<usize>,
        fail_at: Cell<usize>,
    }

    impl Fixture {
        fn new(n: usize, pairs: &[(usize, usize)], direction: Directedness) -> Self {
            // Reverse insertion and high bits exercise canonical full-width IDs.
            let vertices: Vec<_> = (0..n)
                .map(|index| VId(((n - index) as u128) << 96 | index as u128))
                .collect();
            let edges: Vec<_> = pairs
                .iter()
                .enumerate()
                .map(|(id, &(source, target))| ProjectionEdge {
                    eid: EId((1u128 << 112) | id as u128),
                    source: vertices[source],
                    target: vertices[target],
                    weight: [-7.0, 0.0, 2.0, 11.0][id % 4],
                })
                .collect();
            Self {
                graph: SnapshotGraphView::build(
                    SnapshotBinding {
                        root: ObjectId([0xa1; 32]),
                        as_of: CommitSeq(3),
                    },
                    &vertices,
                    &edges,
                    ProjectionSpec {
                        directedness: direction,
                        parallel_edges: ParallelEdgePolicy::CollapseUnit,
                        self_loops: SelfLoopPolicy::Keep,
                    },
                    ProjectionLimits {
                        max_vertices: n,
                        max_input_edges: edges.len(),
                        max_adjacency_entries: 2 * edges.len(),
                        max_workspace_bytes: 1 << 26,
                    },
                )
                .unwrap(),
                source_steps: Cell::new(0),
                fail_at: Cell::new(usize::MAX),
            }
        }

        fn step(&self) -> Result<()> {
            let next = self.source_steps.get() + 1;
            self.source_steps.set(next);
            if next == self.fail_at.get() {
                return Err(SealedProjectionError::UnknownOrdinal(next).into());
            }
            Ok(())
        }
    }

    struct Row<'a> {
        fixture: &'a Fixture,
        targets: std::slice::Iter<'a, usize>,
    }

    impl Cursor for Row<'_> {
        fn next(&mut self) -> Result<Option<(usize, f64)>> {
            self.fixture.step()?;
            Ok(self.targets.next().map(|&target| (target, -7.0)))
        }
    }

    impl Rows for Fixture {
        type Cursor<'a> = Row<'a>;

        fn node_count(&self) -> usize {
            self.graph.node_count()
        }

        fn degree(&self, source: usize) -> Option<usize> {
            self.graph.neighbors_indices(source).map(<[_]>::len)
        }

        fn open(&self, source: usize) -> Result<Self::Cursor<'_>> {
            self.step()?;
            Ok(Row {
                fixture: self,
                targets: self.graph.neighbors_indices(source).unwrap().iter(),
            })
        }
    }

    fn limits() -> FnxExecutionLimits {
        FnxExecutionLimits {
            max_iterations: 100,
            max_result_rows: 1000,
            max_estimated_work: 1 << 30,
        }
    }

    fn reference(graph: &SnapshotGraphView) -> BTreeMap<String, String> {
        let names = graph.nodes_ordered();
        let mut oracle = fnx_classes::Graph::strict();
        for &name in names.iter().rev() {
            oracle.add_node(name);
        }
        for (source, &name) in names.iter().enumerate().rev() {
            for &target in graph.neighbors_indices(source).unwrap().iter().rev() {
                if source < target {
                    oracle.add_edge(name, names[target]).unwrap();
                }
            }
        }
        fnx_algorithms::label_propagation_communities(&oracle)
            .into_iter()
            .flat_map(|community| {
                let representative = community.iter().min().unwrap().clone();
                community
                    .into_iter()
                    .map(move |vertex| (vertex, representative.clone()))
            })
            .collect()
    }

    fn compare(fixture: &Fixture) -> crate::FnxResult {
        let graph = &fixture.graph;
        let expected = reference(graph);
        let actual = FnxCallSpec::label_propagation_communities()
            .execute(graph, limits(), || Ok::<_, Infallible>(()))
            .unwrap();
        let (cursor, peak) = run(
            fixture,
            workspace::<Infallible>(graph.node_count()).unwrap(),
            &mut || Ok(()),
        )
        .unwrap();
        assert_eq!(actual.certificate.witness, cursor.witness);
        assert_eq!(actual.certificate.kernel_workspace_bytes, peak);
        assert_eq!(peak, workspace::<Infallible>(graph.node_count()).unwrap());
        let KernelValues::Components(labels) = cursor.values else {
            panic!("community labels");
        };
        assert_eq!(actual.columns, ["vertex", "component"]);
        assert_eq!(actual.rows.len(), graph.node_count());
        let names = graph.nodes_ordered();
        for (index, name) in names.iter().enumerate() {
            assert_eq!(
                actual.rows[index],
                [
                    FnxValue::Vertex(graph.vertex_id(index).unwrap()),
                    FnxValue::Vertex(graph.vertex_id(labels[index]).unwrap())
                ]
            );
            assert_eq!(names[labels[index]], expected[*name]);
            assert!(labels[index] <= index);
        }
        actual
    }

    #[test]
    fn label_propagation_matches_pinned_fnx_on_all_simple_graphs_through_five_vertices() {
        for n in 0..=5 {
            let possibilities: Vec<_> = (0..n)
                .flat_map(|source| (source + 1..n).map(move |target| (source, target)))
                .collect();
            for mask in 0..1usize << possibilities.len() {
                let pairs: Vec<_> = possibilities
                    .iter()
                    .enumerate()
                    .filter(|(bit, _)| mask & (1 << bit) != 0)
                    .map(|(_, &pair)| pair)
                    .collect();
                compare(&Fixture::new(n, &pairs, Directedness::Undirected));
            }
        }
        // Larger disconnected, tied, and dense neighborhoods exercise multiple
        // color classes and reuse of frequency generations across many rounds.
        let mut seed = 0x4197_eca5_d173_001fu64;
        for n in [7, 19, 65] {
            for _ in 0..12 {
                let mut pairs = Vec::new();
                for source in 0..n {
                    for target in source + 1..n {
                        seed ^= seed << 13;
                        seed ^= seed >> 7;
                        seed ^= seed << 17;
                        if seed.is_multiple_of(7) {
                            pairs.push((source, target));
                        }
                    }
                }
                compare(&Fixture::new(n, &pairs, Directedness::Undirected));
            }
        }
    }

    #[test]
    fn label_propagation_returns_distinct_communities_and_retains_isolates() {
        let fixture = Fixture::new(
            8,
            &[(0, 1), (1, 2), (2, 0), (3, 4), (4, 5), (5, 3)],
            Directedness::Undirected,
        );
        let actual = compare(&fixture);
        let ids = fixture.graph.vertex_ids();
        let representatives = [
            ids[0], ids[1], ids[2], ids[2], ids[2], ids[5], ids[5], ids[5],
        ];
        for (row, representative) in actual.rows.iter().zip(representatives) {
            assert_eq!(row[1], FnxValue::Vertex(representative));
        }
        // A bridge connects the two dense groups into one connected component;
        // community detection must still retain their distinct majority labels.
        let joined = Fixture::new(
            6,
            &[(0, 1), (1, 2), (2, 0), (3, 4), (4, 5), (5, 3), (2, 3)],
            Directedness::Undirected,
        );
        let actual = compare(&joined);
        for (index, row) in actual.rows.iter().enumerate() {
            let representative = joined
                .graph
                .vertex_id(if index < 3 { 0 } else { 3 })
                .unwrap();
            assert_eq!(row[1], FnxValue::Vertex(representative));
        }
        for n in [0, 1, 129] {
            let actual = compare(&Fixture::new(n, &[], Directedness::Undirected));
            for row in &actual.rows {
                assert_eq!(row[0], row[1]);
            }
            assert_eq!(actual.certificate.witness.edges_scanned, 0);
        }
    }

    #[test]
    fn label_propagation_aliases_retained_rows_and_resource_refusals_are_exact() {
        let fixture = Fixture::new(5, &[(0, 1), (1, 2), (2, 3)], Directedness::Undirected);
        let baseline = compare(&fixture);
        let call = FnxCallSpec::bind(
            "CALL fnx.label_propagation_communities() YIELD component AS community,node AS id",
            &FnxParameters::new(),
        )
        .unwrap();
        let result = call
            .execute(&fixture.graph, limits(), || Ok::<_, Infallible>(()))
            .unwrap();
        for (row, original) in result.rows.iter().zip(&baseline.rows) {
            assert_eq!(row, &[original[1], original[0]]);
        }
        let mut cursor = call
            .execute_rows(&fixture.graph, limits(), || Ok::<_, Infallible>(()))
            .unwrap();
        assert_eq!(cursor.certificate(), &result.certificate);
        let mut row = Vec::new();
        for expected in &result.rows {
            assert!(
                cursor
                    .next_row(&mut row, || Ok::<_, Infallible>(()))
                    .unwrap()
            );
            assert_eq!(&row, expected);
        }
        assert!(
            !cursor
                .next_row(&mut row, || Ok::<_, Infallible>(()))
                .unwrap()
        );
        for resource in ["iterations", "estimated work", "result rows"] {
            let mut limits = limits();
            match resource {
                "iterations" => limits.max_iterations = 99,
                "estimated work" => {
                    limits.max_estimated_work = result.certificate.estimated_work - 1
                }
                "result rows" => limits.max_result_rows = 4,
                _ => unreachable!(),
            }
            assert!(matches!(
                call.execute(&fixture.graph, limits, || Ok::<_, Infallible>(())),
                Err(FnxExecutionError::LimitExceeded { resource: actual, .. }) if actual == resource
            ));
        }
        let bytes = workspace::<Infallible>(5).unwrap();
        assert!(matches!(
            run(&fixture, bytes - 1, &mut || Ok(())),
            Err(super::super::Error::Execution(
                FnxExecutionError::LimitExceeded {
                    resource: "kernel workspace bytes",
                    ..
                }
            ))
        ));
        assert!(workspace::<Infallible>(usize::MAX).is_err());
        assert!(estimated_work::<Infallible>(usize::MAX, 0, 0, 2).is_err());
    }

    #[test]
    fn label_propagation_refuses_directed_graphs_and_retained_self_loops() {
        let call = FnxCallSpec::label_propagation_communities();
        for direction in [Directedness::Directed, Directedness::Reversed] {
            let fixture = Fixture::new(2, &[(0, 1)], direction);
            assert!(matches!(
                call.execute(&fixture.graph, limits(), || Ok::<_, Infallible>(())),
                Err(FnxExecutionError::GraphKind { .. })
            ));
        }
        let fixture = Fixture::new(2, &[(0, 0), (0, 1)], Directedness::Undirected);
        assert!(matches!(
            call.execute(&fixture.graph, limits(), || Ok::<_, Infallible>(())),
            Err(FnxExecutionError::SelfLoopRefused)
        ));
        assert!(matches!(
            run(&fixture, usize::MAX, &mut || Ok(())),
            Err(super::super::Error::Execution(
                FnxExecutionError::SelfLoopRefused
            ))
        ));
    }

    struct Meter {
        units: usize,
        stop: usize,
        largest: usize,
    }

    impl FnxMeter<usize> for &mut Meter {
        fn unit(&mut self) -> std::result::Result<(), usize> {
            self.units += 1;
            if self.units == self.stop {
                Err(self.units)
            } else {
                Ok(())
            }
        }

        fn poll(&mut self) -> std::result::Result<(), usize> {
            panic!("label propagation charges each bounded phase directly");
        }

        fn units(&mut self, n: usize) -> std::result::Result<(), usize> {
            self.largest = self.largest.max(n);
            for _ in 0..n {
                self.unit()?;
            }
            Ok(())
        }
    }

    #[test]
    fn label_propagation_stops_at_every_native_unit_and_fallible_cursor_step() {
        let fixture = Fixture::new(4, &[(0, 1), (1, 2), (2, 3)], Directedness::Undirected);
        let call = FnxCallSpec::label_propagation_communities();
        let mut meter = Meter {
            units: 0,
            stop: usize::MAX,
            largest: 0,
        };
        let baseline = call.execute(&fixture.graph, limits(), &mut meter).unwrap();
        assert!(meter.units <= baseline.certificate.estimated_work);
        assert!(meter.largest <= FNX_METER_RUN);
        for stop in 1..=meter.units {
            let mut stopped = Meter {
                units: 0,
                stop,
                largest: 0,
            };
            assert!(matches!(
                call.execute(&fixture.graph, limits(), &mut stopped),
                Err(FnxExecutionError::Cancelled(at)) if at == stop
            ));
            assert_eq!(stopped.units, stop);
        }
        let mut controls = 0;
        run(&fixture, usize::MAX, &mut || {
            controls += 1;
            Ok(())
        })
        .unwrap();
        let source_steps = fixture.source_steps.get();
        for stop in 1..=source_steps {
            fixture.source_steps.set(0);
            fixture.fail_at.set(stop);
            assert!(matches!(
                run(&fixture, usize::MAX, &mut || Ok(())),
                Err(super::super::Error::Source(SealedProjectionError::UnknownOrdinal(at))) if at == stop
            ));
            assert_eq!(fixture.source_steps.get(), stop);
        }
        fixture.fail_at.set(usize::MAX);
        for stop in 1..=controls {
            let mut observed = 0;
            assert!(matches!(run(&fixture, usize::MAX, &mut || {
                observed += 1;
                if observed == stop { Err(SealedProjectionError::UnknownOrdinal(stop).into()) } else { Ok(()) }
            }), Err(super::super::Error::Source(SealedProjectionError::UnknownOrdinal(at))) if at == stop));
            assert_eq!(observed, stop);
        }
        assert_eq!(
            call.execute(&fixture.graph, limits(), || Ok::<_, Infallible>(()))
                .unwrap(),
            baseline
        );
        let n = 2 * FNX_METER_RUN + 3;
        let hub = Fixture::new(
            n,
            &(1..n).map(|i| (0, i)).collect::<Vec<_>>(),
            Directedness::Undirected,
        );
        let mut meter = Meter {
            units: 0,
            stop: usize::MAX,
            largest: 0,
        };
        let result = call.execute(&hub.graph, limits(), &mut meter).unwrap();
        assert_eq!(meter.largest, FNX_METER_RUN);
        assert!(meter.units <= result.certificate.estimated_work);
    }
}
