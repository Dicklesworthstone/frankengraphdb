//! Exact signed distances over fallible compressed rows. The shared kernel
//! retains only O(V) wide distances and never materializes adjacency.

use super::{
    Cursor, Delivery, ExecutionError, Finished, FnxAlgorithm, FnxCallSpec, FnxExecutionLimits,
    FnxMemoryLimits, Result, ResultAdmission, Rows, SealedGraphView, SealedRows, admit, checkpoint,
    directional_pass_work, finish, finish_encoded,
};
use crate::bellman_ford::{Distances, Step, estimated_work, workspace};
use crate::sealed_control::{Control, Guard};
use std::convert::Infallible;
use std::future::Future;

fn prepare(
    call: &FnxCallSpec,
    graph: &SealedGraphView,
    limits: FnxExecutionLimits,
    memory: FnxMemoryLimits,
) -> Result<(usize, usize)> {
    let FnxAlgorithm::SingleSourceBellmanFordPathLength { source } = call.algorithm() else {
        return Err(ExecutionError::InvalidUpstreamResult.into());
    };
    let source = graph
        .vertex_ordinal(source)
        .ok_or(ExecutionError::UnknownSource(source))?;
    let n = graph.node_count();
    let estimated = estimated_work::<Infallible>(
        n,
        graph.adjacency_entry_count(),
        directional_pass_work(n, graph.scan_incidence_bound(), graph.spec().directedness)?,
        call.outputs().len(),
    )?;
    admit("estimated work", estimated, limits.max_estimated_work)?;
    admit(
        "kernel workspace bytes",
        workspace::<Infallible>(n)?,
        memory.max_kernel_workspace_bytes,
    )?;
    Ok((estimated, source))
}

fn run_rows(
    graph: &impl Rows,
    source: usize,
    arcs: usize,
    workspace_limit: usize,
    admission: &ResultAdmission,
    checkpoint: &mut impl FnMut() -> Result<()>,
) -> Result<(super::KernelOutput, usize)> {
    checkpoint()?;
    admission.rows(1)?;
    let mut state = Distances::new::<Infallible>(
        graph.node_count(),
        source,
        arcs,
        workspace_limit,
        admission.max_rows,
    )?;
    let mut row = None;
    loop {
        checkpoint()?;
        match state.step::<Infallible>()? {
            Step::Scalar => {}
            Step::Open(vertex) => {
                if row.is_some() {
                    return Err(ExecutionError::InvalidUpstreamResult.into());
                }
                row = Some(graph.open(vertex)?);
            }
            Step::Row(_) => match row
                .as_mut()
                .ok_or(ExecutionError::InvalidUpstreamResult)?
                .next()?
            {
                Some((neighbor, weight)) => state.visit::<Infallible>(neighbor, weight)?,
                None => {
                    state.end_row::<Infallible>()?;
                    row = None;
                }
            },
            Step::Complete => break,
        }
        admission.rows(state.row_count())?;
    }
    if row.is_some() {
        return Err(ExecutionError::InvalidUpstreamResult.into());
    }
    let peak = state.workspace_bytes();
    Ok((state.finish::<Infallible>()?, peak))
}

pub(super) fn execute(
    call: &FnxCallSpec,
    cx: &Control<'_, impl Guard>,
    graph: &SealedGraphView,
    limits: FnxExecutionLimits,
    memory: FnxMemoryLimits,
    admission: &ResultAdmission,
) -> Result<Finished> {
    let (estimated, source) = prepare(call, graph, limits, memory)?;
    let (output, peak) = run_rows(
        &SealedRows { cx, graph },
        source,
        graph.adjacency_entry_count(),
        memory.max_kernel_workspace_bytes,
        admission,
        &mut || checkpoint(cx),
    )?;
    finish(
        call,
        graph,
        output,
        "fgdb-prism/sealed-bellman-ford-exact-dyadic-sweeps-v1",
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
    let (estimated, source) = prepare(call, graph, limits, memory)?;
    control.tick().await?;
    admission.rows(1)?;
    let mut state = Distances::new::<Infallible>(
        graph.node_count(),
        source,
        graph.adjacency_entry_count(),
        memory.max_kernel_workspace_bytes,
        admission.max_rows,
    )?;
    let mut row = None;
    loop {
        control.tick().await?;
        match state.step::<Infallible>()? {
            Step::Scalar => {}
            Step::Open(vertex) => {
                if row.is_some() {
                    return Err(ExecutionError::InvalidUpstreamResult.into());
                }
                row = Some(graph.neighbor_cursor_controlled(control.cx, vertex, None)?);
            }
            Step::Row(_) => match control
                .next(row.as_mut().ok_or(ExecutionError::InvalidUpstreamResult)?)
                .await?
            {
                Some((neighbor, weight)) => state.visit::<Infallible>(neighbor, weight)?,
                None => {
                    state.end_row::<Infallible>()?;
                    row = None;
                }
            },
            Step::Complete => break,
        }
        admission.rows(state.row_count())?;
    }
    if row.is_some() {
        return Err(ExecutionError::InvalidUpstreamResult.into());
    }
    control.tick().await?;
    let peak = state.workspace_bytes();
    let output = state.finish::<Infallible>()?;
    let mut encoded = super::EncodedRows::new(call, output.row_count, &admission)?;
    for index in 0..output.values.scan_len(graph.node_count()) {
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
        "fgdb-prism/sealed-bellman-ford-exact-dyadic-sweeps-cooperative-v1",
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
        rows: Vec<Vec<(usize, f64)>>,
        pulls: Cell<usize>,
        fail_at: Cell<usize>,
        opens: Vec<Cell<usize>>,
    }

    impl Fixture {
        fn new(rows: Vec<Vec<(usize, f64)>>) -> Self {
            Self {
                opens: (0..rows.len()).map(|_| Cell::new(0)).collect(),
                rows,
                pulls: Cell::new(0),
                fail_at: Cell::new(usize::MAX),
            }
        }
        fn shape() -> Self {
            Self::new(vec![
                vec![(1, 4.0), (2, 7.0)],
                vec![(2, -3.0)],
                vec![(3, 2.0)],
                vec![],
                vec![(4, -99.0)],
            ])
        }
        fn arcs(&self) -> usize {
            self.rows.iter().map(Vec::len).sum()
        }
        fn source_step(&self) -> Result<()> {
            let at = self.pulls.get() + 1;
            self.pulls.set(at);
            if at == self.fail_at.get() {
                Err(SealedProjectionError::UnknownOrdinal(at).into())
            } else {
                Ok(())
            }
        }
        fn reset(&self) {
            self.pulls.set(0);
            self.fail_at.set(usize::MAX);
            for opens in &self.opens {
                opens.set(0);
            }
        }
    }

    struct Row<'a> {
        graph: &'a Fixture,
        values: std::slice::Iter<'a, (usize, f64)>,
    }
    impl Cursor for Row<'_> {
        fn next(&mut self) -> Result<Option<(usize, f64)>> {
            self.graph.source_step()?;
            Ok(self.values.next().copied())
        }
    }
    impl Rows for Fixture {
        type Cursor<'a> = Row<'a>;
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
                graph: self,
                values: self.rows[source].iter(),
            })
        }
    }

    fn admission(rows: usize, bytes: usize) -> ResultAdmission {
        ResultAdmission {
            row_bytes: 96,
            column_bytes: 0,
            max_rows: rows,
            max_bytes: bytes,
            delivery: Delivery::Collected,
        }
    }

    #[test]
    fn signed_rows_preserve_every_source_failure_checkpoint_and_admission_boundary() {
        let graph = Fixture::shape();
        let budget = admission(4, 384);
        let mut checks = 0;
        let (output, peak) = run_rows(&graph, 0, graph.arcs(), usize::MAX, &budget, &mut || {
            checks += 1;
            Ok(())
        })
        .unwrap();
        let pulls = graph.pulls.get();
        assert_eq!(output.row_count, 4);
        assert_eq!(
            graph.opens[4].get(),
            0,
            "an unreachable cycle is not visited"
        );
        assert!(graph.opens[..4].iter().all(|opens| opens.get() == 2));
        assert_eq!(output.witness.edges_scanned, 8);
        assert!(checks <= estimated_work::<Infallible>(5, graph.arcs(), 5, 2).unwrap());
        let KernelValues::WeightedDistances(values) = output.values else {
            panic!("weighted distances");
        };
        assert_eq!(
            values,
            vec![Some(0.0), Some(4.0), Some(1.0), Some(3.0), None]
        );

        graph.reset();
        run_rows(&graph, 0, graph.arcs(), peak, &budget, &mut || Ok(())).unwrap();
        for (memory, budget) in [
            (peak - 1, admission(4, 384)),
            (peak, admission(3, 384)),
            (peak, admission(4, 383)),
        ] {
            graph.reset();
            assert!(matches!(
                run_rows(&graph, 0, graph.arcs(), memory, &budget, &mut || Ok(())),
                Err(Error::Execution(ExecutionError::LimitExceeded { .. }))
            ));
        }
        for stop in 1..=pulls {
            graph.reset();
            graph.fail_at.set(stop);
            assert!(matches!(
                run_rows(&graph, 0, graph.arcs(), peak, &budget, &mut || Ok(())),
                Err(Error::Source(SealedProjectionError::UnknownOrdinal(at))) if at == stop
            ));
        }
        for stop in 1..=checks {
            graph.reset();
            let mut at = 0;
            assert!(matches!(
                run_rows(&graph, 0, graph.arcs(), peak, &budget, &mut || {
                    at += 1;
                    if at == stop {
                        Err(Error::Cancelled(Box::new(std::io::Error::other("stop"))))
                    } else {
                        Ok(())
                    }
                }),
                Err(Error::Cancelled(_))
            ));
            assert_eq!(at, stop);
        }
    }

    #[test]
    fn fallible_rows_keep_numeric_cycle_and_invalid_source_refusals_distinct() {
        let cycle = Fixture::new(vec![vec![(1, 1e20)], vec![(2, -1.0)], vec![(1, 0.0)]]);
        assert!(matches!(
            run_rows(
                &cycle,
                0,
                cycle.arcs(),
                usize::MAX,
                &admission(3, 288),
                &mut || Ok(())
            ),
            Err(Error::Execution(ExecutionError::ReachableNegativeCycle))
        ));
        let overflow = Fixture::new(vec![vec![(1, f64::MAX)], vec![(2, f64::MAX)], vec![]]);
        assert!(matches!(
            run_rows(
                &overflow,
                0,
                overflow.arcs(),
                usize::MAX,
                &admission(3, 288),
                &mut || Ok(())
            ),
            Err(Error::Execution(ExecutionError::NonFiniteDistanceSum))
        ));
        for weight in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let graph = Fixture::new(vec![vec![(1, 1.0)], vec![(2, weight)], vec![]]);
            assert!(matches!(
                run_rows(
                    &graph,
                    0,
                    graph.arcs(),
                    usize::MAX,
                    &admission(3, 288),
                    &mut || Ok(())
                ),
                Err(Error::Execution(ExecutionError::InvalidNumericResult))
            ));
        }
        let invalid = Fixture::new(vec![vec![(1, 1.0)], vec![(99, 0.0)]]);
        assert!(matches!(
            run_rows(
                &invalid,
                0,
                invalid.arcs(),
                usize::MAX,
                &admission(2, 192),
                &mut || Ok(())
            ),
            Err(Error::Execution(ExecutionError::InvalidUpstreamResult))
        ));
    }
}
