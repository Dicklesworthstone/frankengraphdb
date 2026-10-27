//! HITS directly over authenticated compressed rows. The shared scalar state
//! preserves fnx's order across yields, including both final normalizations.
//! No reverse projection, decoded fallback or owned neighbor row is needed.

use super::{
    Cursor, Delivery, Finished, FnxCallSpec, FnxExecutionLimits, FnxMemoryLimits, KernelOutput,
    Result, ResultAdmission, Rows, SealedGraphView, SealedRows, admit, checkpoint,
    directional_pass_work, finish, finish_encoded,
};
use crate::hits::{Iteration, MAX_ITERATIONS, estimated_work, workspace};
use crate::sealed_control::Control;
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
        .push_str(" plus O(k * (|V| log(1+H) + H log(1+|V|))) compressed row visits");
}

pub(super) fn execute(
    call: &FnxCallSpec,
    cx: &Control<'_>,
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
        "fgdb-prism/sealed-hits-source-order-v1",
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
    control: &mut super::cooperative::Cooperate<'_, Yield>,
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
        "fgdb-prism/sealed-hits-source-order-cooperative-v1",
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
        fn new(
            n: usize,
            pairs: &[(usize, usize)],
            direction: Directedness,
            loops: SelfLoopPolicy,
            parallel: ParallelEdgePolicy,
        ) -> Self {
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
                        root: ObjectId([0x71; 32]),
                        as_of: CommitSeq(3),
                    },
                    &vertices,
                    &edges,
                    ProjectionSpec {
                        directedness: direction,
                        parallel_edges: parallel,
                        self_loops: loops,
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

    fn reference(graph: &SnapshotGraphView) -> fnx_algorithms::HitsCentralityResult {
        let names = graph.nodes_ordered();
        if graph.is_directed() {
            let mut oracle = fnx_classes::digraph::DiGraph::strict();
            // Reverse insertion cannot change the canonical floating-point order.
            for &name in names.iter().rev() {
                oracle.add_node(name);
            }
            for (source, &name) in names.iter().enumerate().rev() {
                for &target in graph.neighbors_indices(source).unwrap().iter().rev() {
                    oracle.add_edge(name, names[target]).unwrap();
                }
            }
            fnx_algorithms::hits_centrality_directed(&oracle)
        } else {
            let mut oracle = fnx_classes::Graph::strict();
            for &name in names.iter().rev() {
                oracle.add_node(name);
            }
            for (source, &name) in names.iter().enumerate().rev() {
                for &target in graph.neighbors_indices(source).unwrap().iter().rev() {
                    if source <= target {
                        oracle.add_edge(name, names[target]).unwrap();
                    }
                }
            }
            fnx_algorithms::hits_centrality(&oracle)
        }
    }

    fn compare(fixture: &Fixture) -> crate::FnxResult {
        let graph = &fixture.graph;
        let expected = reference(graph);
        let hubs: BTreeMap<_, _> = expected
            .hubs
            .into_iter()
            .map(|value| (value.node, value.score.to_bits()))
            .collect();
        let authorities: BTreeMap<_, _> = expected
            .authorities
            .into_iter()
            .map(|value| (value.node, value.score.to_bits()))
            .collect();
        let actual = FnxCallSpec::hits_centrality()
            .execute(graph, limits(), || Ok::<_, Infallible>(()))
            .unwrap();
        let (cursor, peak) = run(
            fixture,
            workspace::<Infallible>(graph.node_count()).unwrap(),
            &mut || Ok(()),
        )
        .unwrap();
        assert_eq!(actual.certificate.witness, expected.witness);
        assert_eq!(cursor.witness, expected.witness);
        assert_eq!(actual.certificate.kernel_workspace_bytes, peak);
        assert_eq!(peak, workspace::<Infallible>(graph.node_count()).unwrap());
        let KernelValues::HubsAndAuthorities {
            hubs: cursor_hubs,
            authorities: cursor_authorities,
        } = cursor.values
        else {
            panic!("HITS values");
        };
        assert_eq!(actual.columns, ["vertex", "hub", "authority"]);
        assert_eq!(actual.rows.len(), graph.node_count());
        for (index, name) in graph.nodes_ordered().iter().enumerate() {
            let [
                FnxValue::Vertex(vertex),
                FnxValue::Score(hub),
                FnxValue::Score(authority),
            ] = actual.rows[index][..]
            else {
                panic!("HITS row shape");
            };
            assert_eq!(vertex, graph.vertex_id(index).unwrap());
            assert_eq!(hub.to_bits(), hubs[*name]);
            assert_eq!(authority.to_bits(), authorities[*name]);
            assert_eq!(hub.to_bits(), cursor_hubs[index].to_bits());
            assert_eq!(authority.to_bits(), cursor_authorities[index].to_bits());
        }
        actual
    }

    #[test]
    fn hits_matches_pinned_fnx_on_every_three_node_topology_and_projection() {
        for mask in 0..512usize {
            let pairs: Vec<_> = (0..9)
                .filter(|bit| mask & (1 << bit) != 0)
                .map(|bit| (bit / 3, bit % 3))
                .collect();
            for direction in [
                Directedness::Directed,
                Directedness::Reversed,
                Directedness::Undirected,
            ] {
                compare(&Fixture::new(
                    3,
                    &pairs,
                    direction,
                    SelfLoopPolicy::Keep,
                    ParallelEdgePolicy::CollapseUnit,
                ));
            }
        }
        // Larger canonical sums catch regrouping that tiny graphs cannot.
        let mut seed = 0xead7_3915_d823_04a1u64;
        for n in [7, 19, 65] {
            for _ in 0..12 {
                let mut pairs = Vec::new();
                for source in 0..n {
                    for target in 0..n {
                        seed ^= seed << 13;
                        seed ^= seed >> 7;
                        seed ^= seed << 17;
                        if seed.is_multiple_of(7) {
                            pairs.push((source, target));
                        }
                    }
                }
                compare(&Fixture::new(
                    n,
                    &pairs,
                    Directedness::Directed,
                    SelfLoopPolicy::Keep,
                    ParallelEdgePolicy::Reject,
                ));
            }
        }
    }

    #[test]
    fn hits_preserves_empty_zero_sum_singleton_and_iteration_cap_semantics() {
        for n in [0, 1, 7, 129] {
            let fixture = Fixture::new(
                n,
                &[],
                Directedness::Directed,
                SelfLoopPolicy::Keep,
                ParallelEdgePolicy::Reject,
            );
            let actual = compare(&fixture);
            for row in &actual.rows {
                assert_eq!(row[1], FnxValue::Score(1.0 / n as f64));
                assert_eq!(row[2], FnxValue::Score(1.0 / n as f64));
            }
            assert_eq!(
                actual.certificate.witness.nodes_touched,
                if n <= 1 { n } else { 4 * n }
            );
            assert_eq!(actual.certificate.witness.edges_scanned, 0);
        }
        compare(&Fixture::new(
            1,
            &[(0, 0)],
            Directedness::Directed,
            SelfLoopPolicy::Keep,
            ParallelEdgePolicy::Reject,
        ));
        let n = 65;
        let pairs: Vec<_> = (1..n).map(|index| (index - 1, index)).collect();
        let actual = compare(&Fixture::new(
            n,
            &pairs,
            Directedness::Undirected,
            SelfLoopPolicy::Keep,
            ParallelEdgePolicy::Reject,
        ));
        assert_eq!(
            actual.certificate.witness.nodes_touched,
            2 * n * MAX_ITERATIONS
        );
        assert_eq!(
            actual.certificate.witness.edges_scanned,
            4 * (n - 1) * MAX_ITERATIONS
        );
    }

    #[test]
    fn hits_obeys_explicit_parallel_loop_and_weight_projection_laws() {
        let pairs = [(0, 0), (0, 1), (0, 1), (1, 0), (1, 2), (2, 2)];
        for direction in [
            Directedness::Directed,
            Directedness::Reversed,
            Directedness::Undirected,
        ] {
            for loops in [SelfLoopPolicy::Keep, SelfLoopPolicy::Drop] {
                let mut previous = None;
                for parallel in [
                    ParallelEdgePolicy::Sum,
                    ParallelEdgePolicy::Minimum,
                    ParallelEdgePolicy::Maximum,
                    ParallelEdgePolicy::CollapseUnit,
                ] {
                    let actual = compare(&Fixture::new(4, &pairs, direction, loops, parallel));
                    if let Some(prior) = &previous {
                        assert_eq!(&actual.rows, prior);
                    }
                    previous = Some(actual.rows);
                }
            }
        }
    }

    #[test]
    fn hits_native_aliases_cursor_and_admission_keep_both_score_columns() {
        let fixture = Fixture::new(
            4,
            &[(0, 1), (0, 2), (3, 2)],
            Directedness::Directed,
            SelfLoopPolicy::Keep,
            ParallelEdgePolicy::Reject,
        );
        let baseline = compare(&fixture);
        let call = FnxCallSpec::bind(
            "CALL fnx.hits_centrality() YIELD authority AS rank,node AS id,hub",
            &FnxParameters::new(),
        )
        .unwrap();
        let result = call
            .execute(&fixture.graph, limits(), || Ok::<_, Infallible>(()))
            .unwrap();
        for (row, original) in result.rows.iter().zip(&baseline.rows) {
            assert_eq!(row, &[original[2], original[0], original[1]]);
        }
        let mut cursor = call
            .execute_rows(&fixture.graph, limits(), || Ok::<_, Infallible>(()))
            .unwrap();
        assert_eq!(cursor.certificate(), &result.certificate);
        assert_eq!(cursor.max_integer(), None);
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
                "result rows" => limits.max_result_rows = 3,
                _ => unreachable!(),
            }
            assert!(
                matches!(call.execute(&fixture.graph, limits, || Ok::<_, Infallible>(())), Err(FnxExecutionError::LimitExceeded { resource: actual, .. }) if actual == resource)
            );
        }
        let bytes = workspace::<Infallible>(4).unwrap();
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
        assert!(estimated_work::<Infallible>(usize::MAX, 0, 0, 3).is_err());
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
            panic!("HITS charges each bounded phase directly");
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
    fn hits_stops_at_every_native_unit_and_fallible_cursor_step() {
        let fixture = Fixture::new(
            3,
            &[(0, 1), (1, 2)],
            Directedness::Directed,
            SelfLoopPolicy::Keep,
            ParallelEdgePolicy::Reject,
        );
        let call = FnxCallSpec::hits_centrality();
        let mut meter = Meter {
            units: 0,
            stop: usize::MAX,
            largest: 0,
        };
        let baseline = call.execute(&fixture.graph, limits(), &mut meter).unwrap();
        assert!(meter.largest <= FNX_METER_RUN);
        for stop in 1..=meter.units {
            let mut stopped = Meter {
                units: 0,
                stop,
                largest: 0,
            };
            assert!(
                matches!(call.execute(&fixture.graph, limits(), &mut stopped), Err(FnxExecutionError::Cancelled(at)) if at == stop)
            );
            assert_eq!(stopped.units, stop, "no metering after refusal");
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
            assert!(
                matches!(run(&fixture, usize::MAX, &mut || Ok(())), Err(super::super::Error::Source(SealedProjectionError::UnknownOrdinal(at))) if at == stop)
            );
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
        // A single high-degree row still requests only bounded meter batches.
        let hub = Fixture::new(
            2 * FNX_METER_RUN + 3,
            &(1..2 * FNX_METER_RUN + 3)
                .map(|n| (0, n))
                .collect::<Vec<_>>(),
            Directedness::Directed,
            SelfLoopPolicy::Keep,
            ParallelEdgePolicy::Reject,
        );
        let mut meter = Meter {
            units: 0,
            stop: usize::MAX,
            largest: 0,
        };
        call.execute(&hub.graph, limits(), &mut meter).unwrap();
        assert_eq!(meter.largest, FNX_METER_RUN);
    }
}
