//! Cut vertices and bridges from fallible compressed rows. Active DFS cursors
//! use the shared depth-bounded frame slabs; no decoded adjacency is retained.

use super::components::{Frame, FrameStack, frame_minimum};
use super::{
    Cursor, Delivery, ExecutionError, Finished, FnxAlgorithm, FnxCallSpec, FnxExecutionLimits,
    FnxMemoryLimits, KernelOutput, Result, ResultAdmission, Rows, SealedGraphView, SealedRow,
    SealedRows, add, admit, checkpoint, directional_pass_work, finish, finish_encoded,
};
use crate::cuts::{Cuts, Step};
use crate::sealed_control::Control;
use std::convert::Infallible;
use std::future::Future;

fn output(state: Cuts) -> Result<KernelOutput> {
    let mut output = state.finish::<Infallible>()?;
    output.witness.complexity_claim = "O(V+E) lowlink and canonical ordering plus O(V*(1+log(1+H)) + H*(1+log(1+H)+log(1+V))) compressed incidence work; H is retained history; O(V) integer workspace and depth-bounded cursor frames".to_owned();
    Ok(output)
}

fn prepare(
    call: &FnxCallSpec,
    graph: &SealedGraphView,
    limits: FnxExecutionLimits,
    memory: FnxMemoryLimits,
) -> Result<(bool, usize)> {
    let bridges = matches!(call.algorithm(), FnxAlgorithm::Bridges);
    let n = graph.node_count();
    let scalar = Cuts::estimated_work(n, graph.adjacency_entry_count(), call.outputs().len())
        .ok_or(ExecutionError::SizeOverflow)?;
    let estimated = add(
        scalar,
        directional_pass_work(n, graph.scan_incidence_bound(), graph.spec().directedness)?,
    )?;
    admit("estimated work", estimated, limits.max_estimated_work)?;
    let arrays = Cuts::workspace(n, bridges).ok_or(ExecutionError::SizeOverflow)?;
    admit(
        "kernel workspace bytes",
        add(arrays, frame_minimum::<SealedRow<'_>>(n)?)?,
        memory.max_kernel_workspace_bytes,
    )?;
    Ok((bridges, estimated))
}

fn run_rows(
    graph: &impl Rows,
    bridges: bool,
    workspace_limit: usize,
    admission: &ResultAdmission,
    checkpoint: &mut impl FnMut() -> Result<()>,
) -> Result<(KernelOutput, usize)> {
    checkpoint()?;
    let n = graph.node_count();
    let mut state = Cuts::new::<Infallible>(n, bridges, workspace_limit, admission.max_rows)?;
    let mut frames = FrameStack::new(n, state.workspace_bytes(), workspace_limit)?;
    loop {
        checkpoint()?;
        match state.step::<Infallible>()? {
            Step::Scalar => {}
            Step::Complete => break,
            Step::Open(vertex) => {
                let row = graph.open(vertex)?;
                frames.push(Frame { vertex, row })?;
            }
            Step::Row(vertex) => {
                let frame = frames
                    .last_mut()
                    .ok_or(ExecutionError::InvalidUpstreamResult)?;
                if frame.vertex != vertex {
                    return Err(ExecutionError::InvalidUpstreamResult.into());
                }
                match frame.row.next()? {
                    Some((neighbor, _)) => state.visit::<Infallible>(neighbor)?,
                    None => {
                        state.end_row::<Infallible>()?;
                        frames.pop().ok_or(ExecutionError::InvalidUpstreamResult)?;
                    }
                }
                admission.rows(state.row_count())?;
            }
        }
    }
    if frames.len() != 0 {
        return Err(ExecutionError::InvalidUpstreamResult.into());
    }
    let peak = frames.workspace();
    Ok((output(state)?, peak))
}

pub(super) fn execute(
    call: &FnxCallSpec,
    cx: &Control<'_>,
    graph: &SealedGraphView,
    limits: FnxExecutionLimits,
    memory: FnxMemoryLimits,
    admission: &ResultAdmission,
) -> Result<Finished> {
    let (bridges, estimated) = prepare(call, graph, limits, memory)?;
    let (output, peak) = run_rows(
        &SealedRows { cx, graph },
        bridges,
        memory.max_kernel_workspace_bytes,
        admission,
        &mut || checkpoint(cx),
    )?;
    finish(
        call,
        graph,
        output,
        if bridges {
            "fgdb-prism/sealed-bridges-lowlink-v1"
        } else {
            "fgdb-prism/sealed-articulation-points-lowlink-v1"
        },
        estimated,
        peak,
        admission,
        &mut || checkpoint(cx),
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
    let (bridges, estimated) = prepare(call, graph, limits, memory)?;
    control.tick().await?;
    let n = graph.node_count();
    let mut state = Cuts::new::<Infallible>(
        n,
        bridges,
        memory.max_kernel_workspace_bytes,
        admission.max_rows,
    )?;
    // Wrap cooperative cursors in the SAME concrete frame layout used by the
    // immediate driver so both modes admit and report identical backing bytes.
    let mut frames = FrameStack::new(
        n,
        state.workspace_bytes(),
        memory.max_kernel_workspace_bytes,
    )?;
    loop {
        control.tick().await?;
        match state.step::<Infallible>()? {
            Step::Scalar => {}
            Step::Complete => break,
            Step::Open(vertex) => {
                let row = SealedRow {
                    cx: control.cx,
                    row: graph.neighbor_cursor_controlled(control.cx, vertex, None)?,
                };
                frames.push(Frame { vertex, row })?;
            }
            Step::Row(vertex) => {
                let frame = frames
                    .last_mut()
                    .ok_or(ExecutionError::InvalidUpstreamResult)?;
                if frame.vertex != vertex {
                    return Err(ExecutionError::InvalidUpstreamResult.into());
                }
                match control.next(&mut frame.row.row).await? {
                    Some((neighbor, _)) => state.visit::<Infallible>(neighbor)?,
                    None => {
                        state.end_row::<Infallible>()?;
                        frames.pop().ok_or(ExecutionError::InvalidUpstreamResult)?;
                    }
                }
                admission.rows(state.row_count())?;
            }
        }
    }
    if frames.len() != 0 {
        return Err(ExecutionError::InvalidUpstreamResult.into());
    }
    control.tick().await?;
    let peak = frames.workspace();
    let output = output(state)?;
    let mut encoded = super::EncodedRows::new(call, output.row_count, &admission)?;
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
        if bridges {
            "fgdb-prism/sealed-bridges-lowlink-cooperative-v1"
        } else {
            "fgdb-prism/sealed-articulation-points-lowlink-cooperative-v1"
        },
        estimated,
        peak,
        &mut || checkpoint(control.cx),
    )
}

#[cfg(test)]
mod tests {
    use super::super::{Error, KernelValues, SealedProjectionError};
    use super::*;
    use std::cell::Cell;

    struct Fixture {
        rows: Vec<Vec<usize>>,
        source_steps: Cell<usize>,
        fail_at: usize,
        opens: Vec<Cell<usize>>,
    }
    impl Fixture {
        fn new(rows: Vec<Vec<usize>>) -> Self {
            let opens = (0..rows.len()).map(|_| Cell::new(0)).collect();
            Self {
                rows,
                source_steps: Cell::new(0),
                fail_at: usize::MAX,
                opens,
            }
        }
        fn source_step(&self) -> Result<()> {
            let step = self.source_steps.get() + 1;
            self.source_steps.set(step);
            if step == self.fail_at {
                Err(Error::Source(SealedProjectionError::UnknownOrdinal(
                    usize::MAX,
                )))
            } else {
                Ok(())
            }
        }
        fn reset(&mut self) {
            self.source_steps.set(0);
            self.fail_at = usize::MAX;
            for opens in &self.opens {
                opens.set(0);
            }
        }
    }
    struct Row<'a> {
        fixture: &'a Fixture,
        neighbors: std::slice::Iter<'a, usize>,
    }
    impl Cursor for Row<'_> {
        fn next(&mut self) -> Result<Option<(usize, f64)>> {
            self.fixture.source_step()?;
            Ok(self.neighbors.next().map(|&target| (target, -7.0)))
        }
    }
    impl Rows for Fixture {
        type Cursor<'a>
            = Row<'a>
        where
            Self: 'a;
        fn node_count(&self) -> usize {
            self.rows.len()
        }
        fn degree(&self, source: usize) -> Option<usize> {
            self.rows.get(source).map(Vec::len)
        }
        fn open(&self, source: usize) -> Result<Self::Cursor<'_>> {
            self.source_step()?;
            self.opens[source].set(self.opens[source].get() + 1);
            Ok(Row {
                fixture: self,
                neighbors: self.rows[source].iter(),
            })
        }
    }
    fn admission(rows: usize, bytes: usize) -> ResultAdmission {
        ResultAdmission {
            row_bytes: 64,
            column_bytes: 0,
            max_rows: rows,
            max_bytes: bytes,
            delivery: Delivery::Collected,
        }
    }
    fn shape() -> Fixture {
        Fixture::new(vec![
            vec![1, 4],
            vec![0, 2, 3],
            vec![1, 3],
            vec![1, 2],
            vec![0, 5],
            vec![4],
            vec![],
        ])
    }

    #[test]
    fn cursor_driver_matches_cutsets_opens_each_row_once_and_keeps_endpoint_order() {
        for bridges in [false, true] {
            let graph = shape();
            let (output, _) = run_rows(
                &graph,
                bridges,
                usize::MAX,
                &admission(usize::MAX, usize::MAX),
                &mut || Ok(()),
            )
            .unwrap();
            assert!(graph.opens.iter().all(|count| count.get() == 1));
            assert_eq!(output.witness.nodes_touched, graph.rows.len());
            assert_eq!(
                output.witness.edges_scanned,
                graph.rows.iter().map(Vec::len).sum::<usize>()
            );
            match output.values {
                KernelValues::SelectedVertices(values) => {
                    assert_eq!(values, vec![true, true, false, false, true, false, false])
                }
                KernelValues::EndpointPairs(values) => {
                    assert_eq!(values, vec![(0, 1), (0, 4), (4, 5)])
                }
                _ => panic!("cut result"),
            }
        }
    }

    #[test]
    fn every_cursor_fault_and_live_checkpoint_refuses_without_a_successful_prefix() {
        for bridges in [false, true] {
            let mut graph = shape();
            let admission = admission(usize::MAX, usize::MAX);
            let mut checks = 0;
            run_rows(&graph, bridges, usize::MAX, &admission, &mut || {
                checks += 1;
                Ok(())
            })
            .unwrap();
            let source_steps = graph.source_steps.get();
            for fail_at in 1..=source_steps {
                graph.reset();
                graph.fail_at = fail_at;
                assert!(matches!(
                    run_rows(&graph, bridges, usize::MAX, &admission, &mut || Ok(())),
                    Err(Error::Source(SealedProjectionError::UnknownOrdinal(
                        usize::MAX
                    )))
                ));
                assert_eq!(graph.source_steps.get(), fail_at);
            }
            for stop in 1..=checks {
                graph.reset();
                let mut seen = 0;
                let result = run_rows(&graph, bridges, usize::MAX, &admission, &mut || {
                    seen += 1;
                    if seen == stop {
                        Err(Error::Source(SealedProjectionError::UnknownOrdinal(stop)))
                    } else {
                        Ok(())
                    }
                });
                assert!(
                    matches!(result, Err(Error::Source(SealedProjectionError::UnknownOrdinal(at))) if at == stop)
                );
                assert_eq!(seen, stop);
            }
        }
    }

    #[test]
    fn source_errors_remain_observable_in_late_components_and_after_zero_cutsets() {
        let graph = Fixture::new(vec![vec![1, 2], vec![0, 2], vec![0, 1], vec![4]]);
        for bridges in [false, true] {
            assert!(matches!(
                run_rows(
                    &graph,
                    bridges,
                    usize::MAX,
                    &admission(0, 0),
                    &mut || Ok(())
                ),
                Err(Error::Execution(ExecutionError::InvalidUpstreamResult))
            ));
        }
    }

    #[test]
    fn cursor_workspace_follows_depth_and_result_bytes_count_actual_rows() {
        let n = 2048;
        let mut rows = vec![Vec::new(); n];
        for leaf in 1..n {
            rows[0].push(leaf);
            rows[leaf].push(0);
        }
        let graph = Fixture::new(rows);
        let (output, peak) =
            run_rows(&graph, false, usize::MAX, &admission(1, 64), &mut || Ok(())).unwrap();
        assert_eq!(output.row_count, 1);
        assert_eq!(output.witness.queue_peak, 2);
        let arrays = Cuts::workspace(n, false).unwrap();
        assert!(peak < arrays + n * size_of::<Frame<Row<'_>>>() / 2);
        run_rows(&graph, false, peak, &admission(1, 64), &mut || Ok(())).unwrap();
        assert!(matches!(
            run_rows(&graph, false, usize::MAX, &admission(1, 63), &mut || Ok(())),
            Err(Error::Execution(ExecutionError::LimitExceeded {
                resource: "result bytes",
                ..
            }))
        ));
        assert!(matches!(
            run_rows(&graph, false, arrays - 1, &admission(1, 64), &mut || Ok(())),
            Err(Error::Execution(ExecutionError::LimitExceeded {
                resource: "kernel workspace bytes",
                ..
            }))
        ));
    }
}
