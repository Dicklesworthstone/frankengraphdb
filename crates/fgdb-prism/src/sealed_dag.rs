//! Directed DAG procedures over authenticated compressed outgoing rows.
//! Active DFS cursors use the shared depth-bounded frame slabs. The decoded
//! and cooperative paths share the same integer state, without copying edges.

use super::components::{Frame, FrameStack, frame_minimum};
use super::{
    Cursor, Delivery, ExecutionError, Finished, FnxAlgorithm, FnxCallSpec, FnxExecutionLimits,
    FnxMemoryLimits, KernelOutput, Result, ResultAdmission, Rows, SealedGraphView, SealedRow,
    SealedRows, add, admit, checkpoint, directional_pass_work, finish, finish_encoded,
};
use crate::dag::{Dag, Step};
use crate::sealed_control::{Control, Guard};
use std::convert::Infallible;
use std::future::Future;

fn output(state: Dag) -> Result<KernelOutput> {
    let mut output = state.finish::<Infallible>()?;
    output.witness.complexity_claim = "O(V+E) directed DFS plus O(V*(1+log(1+H)) + H*(1+log(1+H)+log(1+V))) compressed incidence work; H is retained history; O(V) integer workspace and depth-bounded cursor frames".to_owned();
    Ok(output)
}

fn prepare(
    call: &FnxCallSpec,
    graph: &SealedGraphView,
    limits: FnxExecutionLimits,
    memory: FnxMemoryLimits,
) -> Result<(bool, usize)> {
    let predicate = matches!(call.algorithm(), FnxAlgorithm::IsDirectedAcyclicGraph);
    let n = graph.node_count();
    let scalar = Dag::estimated_work(n, graph.adjacency_entry_count(), call.outputs().len())
        .ok_or(ExecutionError::SizeOverflow)?;
    let estimated = add(
        scalar,
        directional_pass_work(n, graph.scan_incidence_bound(), graph.spec().directedness)?,
    )?;
    admit("estimated work", estimated, limits.max_estimated_work)?;
    let arrays = Dag::workspace(n, predicate).ok_or(ExecutionError::SizeOverflow)?;
    admit(
        "kernel workspace bytes",
        add(arrays, frame_minimum::<SealedRow<'_>>(n)?)?,
        memory.max_kernel_workspace_bytes,
    )?;
    Ok((predicate, estimated))
}

fn run_rows(
    graph: &impl Rows,
    predicate: bool,
    workspace_limit: usize,
    admission: &ResultAdmission,
    checkpoint: &mut impl FnMut() -> Result<()>,
) -> Result<(KernelOutput, usize)> {
    checkpoint()?;
    let n = graph.node_count();
    admission.rows(if predicate { 1 } else { n })?;
    let mut state = Dag::new::<Infallible>(n, predicate, workspace_limit, admission.max_rows)?;
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
    cx: &Control<'_, impl Guard>,
    graph: &SealedGraphView,
    limits: FnxExecutionLimits,
    memory: FnxMemoryLimits,
    admission: &ResultAdmission,
) -> Result<Finished> {
    let (predicate, estimated) = prepare(call, graph, limits, memory)?;
    let (output, peak) = run_rows(
        &SealedRows { cx, graph },
        predicate,
        memory.max_kernel_workspace_bytes,
        admission,
        &mut || checkpoint(cx),
    )?;
    finish(
        call,
        graph,
        output,
        if predicate {
            "fgdb-prism/sealed-directed-acyclic-dfs-v1"
        } else {
            "fgdb-prism/sealed-topological-sort-dfs-v1"
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
    control: &mut super::cooperative::Cooperate<'_, Yield, impl Guard>,
) -> Result<Finished>
where
    Yield: FnMut() -> YieldFuture,
    YieldFuture: Future<Output = ()>,
{
    let admission = ResultAdmission::for_delivery(call, limits, memory, delivery)?;
    let (predicate, estimated) = prepare(call, graph, limits, memory)?;
    control.tick().await?;
    let n = graph.node_count();
    admission.rows(if predicate { 1 } else { n })?;
    let mut state = Dag::new::<Infallible>(
        n,
        predicate,
        memory.max_kernel_workspace_bytes,
        admission.max_rows,
    )?;
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
    for index in 0..output.values.scan_len(n) {
        control.tick().await?;
        encoded.push(call, graph, &output.values, index, &mut || {
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
        if predicate {
            "fgdb-prism/sealed-directed-acyclic-dfs-cooperative-v1"
        } else {
            "fgdb-prism/sealed-topological-sort-dfs-cooperative-v1"
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
        steps: Cell<usize>,
        fail_at: usize,
        opens: Vec<Cell<usize>>,
    }
    impl Fixture {
        fn new(rows: Vec<Vec<usize>>) -> Self {
            let opens = (0..rows.len()).map(|_| Cell::new(0)).collect();
            Self {
                rows,
                steps: Cell::new(0),
                fail_at: usize::MAX,
                opens,
            }
        }
        fn source_step(&self) -> Result<()> {
            let next = self.steps.get() + 1;
            self.steps.set(next);
            if next == self.fail_at {
                Err(Error::Source(SealedProjectionError::UnknownOrdinal(next)))
            } else {
                Ok(())
            }
        }
    }
    struct Row<'a> {
        source: &'a Fixture,
        neighbors: std::slice::Iter<'a, usize>,
    }
    impl Cursor for Row<'_> {
        fn next(&mut self) -> Result<Option<(usize, f64)>> {
            self.source.source_step()?;
            Ok(self.neighbors.next().map(|&target| (target, -3.0)))
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
                source: self,
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

    #[test]
    fn compressed_cursor_state_opens_every_row_once_and_keeps_pinned_order() {
        for predicate in [false, true] {
            let graph = Fixture::new(vec![vec![1, 2], vec![3], vec![3], vec![], vec![]]);
            let (result, _) = run_rows(
                &graph,
                predicate,
                usize::MAX,
                &admission(5, 320),
                &mut || Ok(()),
            )
            .unwrap();
            assert!(graph.opens.iter().all(|n| n.get() == 1));
            assert_eq!(result.witness.nodes_touched, 5);
            assert_eq!(result.witness.edges_scanned, 4);
            match result.values {
                KernelValues::TopologicalOrder(order) => assert_eq!(order, [4, 0, 2, 1, 3]),
                KernelValues::Acyclic(value) => assert!(value),
                _ => panic!("DAG result"),
            }
        }
    }

    #[test]
    fn every_cursor_fault_and_checkpoint_refuses_without_a_successful_prefix() {
        for predicate in [false, true] {
            let mut graph = Fixture::new(vec![vec![1, 2], vec![], vec![3], vec![], vec![]]);
            let admission = admission(5, 320);
            let mut checks = 0;
            run_rows(&graph, predicate, usize::MAX, &admission, &mut || {
                checks += 1;
                Ok(())
            })
            .unwrap();
            let source_steps = graph.steps.get();
            for fail_at in 1..=source_steps {
                graph.steps.set(0);
                graph.fail_at = fail_at;
                assert!(matches!(
                    run_rows(&graph, predicate, usize::MAX, &admission, &mut || Ok(())),
                    Err(Error::Source(SealedProjectionError::UnknownOrdinal(at))) if at == fail_at
                ));
                assert_eq!(graph.steps.get(), fail_at);
            }
            graph.fail_at = usize::MAX;
            for stop in 1..=checks {
                graph.steps.set(0);
                let mut at = 0;
                let result = run_rows(&graph, predicate, usize::MAX, &admission, &mut || {
                    at += 1;
                    if at == stop {
                        Err(Error::Source(SealedProjectionError::UnknownOrdinal(stop)))
                    } else {
                        Ok(())
                    }
                });
                assert!(
                    matches!(result, Err(Error::Source(SealedProjectionError::UnknownOrdinal(found))) if found == stop)
                );
                assert_eq!(at, stop);
            }
        }
    }

    #[test]
    fn cycles_do_not_hide_late_source_errors_and_predicate_admits_one_row() {
        let graph = Fixture::new(vec![vec![0], vec![], vec![3]]);
        for predicate in [false, true] {
            assert!(matches!(
                run_rows(
                    &graph,
                    predicate,
                    usize::MAX,
                    &admission(3, 192),
                    &mut || Ok(())
                ),
                Err(Error::Execution(ExecutionError::InvalidUpstreamResult))
            ));
        }
        let graph = Fixture::new(vec![vec![0], vec![], vec![]]);
        let (result, peak) =
            run_rows(&graph, true, usize::MAX, &admission(1, 64), &mut || Ok(())).unwrap();
        assert!(matches!(result.values, KernelValues::Acyclic(false)));
        run_rows(&graph, true, peak, &admission(1, 64), &mut || Ok(())).unwrap();
        for budget in [admission(0, 64), admission(1, 63)] {
            assert!(matches!(
                run_rows(&graph, true, usize::MAX, &budget, &mut || Ok(())),
                Err(Error::Execution(ExecutionError::LimitExceeded { .. }))
            ));
        }
    }
}
