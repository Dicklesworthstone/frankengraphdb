//! Minimum spanning forests over fallible compressed rows. Only one row cursor
//! is live; the shared indexed Prim state retains O(V) numeric/ordinal arrays.

use super::{
    Cursor, Delivery, ExecutionError, Finished, FnxCallSpec, FnxExecutionLimits, FnxMemoryLimits,
    Result, ResultAdmission, Rows, SealedGraphView, SealedRows, admit, checkpoint,
    directional_pass_work, finish, finish_encoded,
};
use crate::sealed_control::{Control, Guard};
use crate::spanning_tree::{Forest, Step, estimated_work, workspace};
use std::convert::Infallible;
use std::future::Future;

fn prepare(
    call: &FnxCallSpec,
    graph: &SealedGraphView,
    limits: FnxExecutionLimits,
    memory: FnxMemoryLimits,
) -> Result<usize> {
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
    Ok(estimated)
}

fn run_rows(
    graph: &impl Rows,
    workspace_limit: usize,
    admission: &ResultAdmission,
    checkpoint: &mut impl FnMut() -> Result<()>,
) -> Result<(super::KernelOutput, usize)> {
    checkpoint()?;
    let mut state =
        Forest::new::<Infallible>(graph.node_count(), workspace_limit, admission.max_rows)?;
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
            Step::Row(_) => {
                match row
                    .as_mut()
                    .ok_or(ExecutionError::InvalidUpstreamResult)?
                    .next()?
                {
                    Some((neighbor, weight)) => state.visit::<Infallible>(neighbor, weight)?,
                    None => {
                        state.end_row::<Infallible>()?;
                        row = None;
                    }
                }
            }
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
    let estimated = prepare(call, graph, limits, memory)?;
    let (output, peak) = run_rows(
        &SealedRows { cx, graph },
        memory.max_kernel_workspace_bytes,
        admission,
        &mut || checkpoint(cx),
    )?;
    finish(
        call,
        graph,
        output,
        "fgdb-prism/sealed-minimum-spanning-forest-indexed-prim-v1",
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
    let estimated = prepare(call, graph, limits, memory)?;
    control.tick().await?;
    let mut state = Forest::new::<Infallible>(
        graph.node_count(),
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
            Step::Row(_) => {
                match control
                    .next(row.as_mut().ok_or(ExecutionError::InvalidUpstreamResult)?)
                    .await?
                {
                    Some((neighbor, weight)) => state.visit::<Infallible>(neighbor, weight)?,
                    None => {
                        state.end_row::<Infallible>()?;
                        row = None;
                    }
                }
            }
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
    for index in 0..output.row_count {
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
        "fgdb-prism/sealed-minimum-spanning-forest-indexed-prim-cooperative-v1",
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
        fail_at: usize,
        opens: Vec<Cell<usize>>,
    }

    impl Fixture {
        fn shape() -> Self {
            Self {
                rows: vec![
                    vec![(0, -90.0), (1, 3.0), (2, 1.0)],
                    vec![(0, 3.0), (2, -2.0), (3, 0.0)],
                    vec![(0, 1.0), (1, -2.0), (3, 0.0)],
                    vec![(1, 0.0), (2, 0.0)],
                    vec![(5, -7.0)],
                    vec![(4, -7.0)],
                    vec![],
                ],
                pulls: Cell::new(0),
                fail_at: usize::MAX,
                opens: (0..7).map(|_| Cell::new(0)).collect(),
            }
        }
        fn source_step(&self) -> Result<()> {
            let step = self.pulls.get() + 1;
            self.pulls.set(step);
            if step == self.fail_at {
                Err(SealedProjectionError::UnknownOrdinal(step).into())
            } else {
                Ok(())
            }
        }
        fn reset(&self) {
            self.pulls.set(0);
            for count in &self.opens {
                count.set(0);
            }
        }
    }

    struct Row<'a> {
        fixture: &'a Fixture,
        values: std::slice::Iter<'a, (usize, f64)>,
    }
    impl Cursor for Row<'_> {
        fn next(&mut self) -> Result<Option<(usize, f64)>> {
            self.fixture.source_step()?;
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
                fixture: self,
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
    fn forest_drains_every_row_once_preserves_disconnected_components_and_refuses_every_fault() {
        let mut graph = Fixture::shape();
        let budget = admission(4, 384);
        let mut checks = 0;
        let (output, peak) = run_rows(&graph, usize::MAX, &budget, &mut || {
            checks += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(output.row_count, 4);
        assert!(graph.opens.iter().all(|count| count.get() == 1));
        let arcs: usize = graph.rows.iter().map(Vec::len).sum();
        assert_eq!(output.witness.edges_scanned, arcs);
        assert_eq!(output.witness.nodes_touched, 7);
        assert!(checks <= estimated_work::<Infallible>(7, arcs, 7, 3).unwrap());
        let KernelValues::WeightedEndpointPairs(edges) = output.values else {
            panic!("weighted forest");
        };
        assert_eq!(
            edges
                .iter()
                .map(|e| (e.vertex, e.neighbor, e.weight.to_bits()))
                .collect::<Vec<_>>(),
            vec![
                (4, 5, (-7.0f64).to_bits()),
                (1, 2, (-2.0f64).to_bits()),
                (1, 3, 0.0f64.to_bits()),
                (0, 2, 1.0f64.to_bits())
            ]
        );
        let pulls = graph.pulls.get();
        graph.reset();
        run_rows(&graph, peak, &budget, &mut || Ok(())).unwrap();
        for (memory, budget) in [
            (peak - 1, admission(4, 384)),
            (peak, admission(3, 384)),
            (peak, admission(4, 383)),
        ] {
            graph.reset();
            assert!(matches!(
                run_rows(&graph, memory, &budget, &mut || Ok(())),
                Err(Error::Execution(ExecutionError::LimitExceeded { .. }))
            ));
        }
        for stop in 1..=pulls {
            graph.reset();
            graph.fail_at = stop;
            assert!(
                matches!(run_rows(&graph, peak, &budget, &mut || Ok(())), Err(Error::Source(SealedProjectionError::UnknownOrdinal(value))) if value == stop)
            );
        }
        graph.fail_at = usize::MAX;
        for stop in 1..=checks {
            graph.reset();
            let mut count = 0;
            assert!(matches!(
                run_rows(&graph, peak, &budget, &mut || {
                    count += 1;
                    if count == stop {
                        Err(Error::Cancelled(Box::new(std::io::Error::other("stop"))))
                    } else {
                        Ok(())
                    }
                }),
                Err(Error::Cancelled(_))
            ));
        }
    }

    #[test]
    fn raw_signed_zeros_tie_by_endpoints_without_total_ordering_the_sign_bit() {
        let mut graph = Fixture::shape();
        graph.rows = vec![
            vec![(1, 0.0), (2, -0.0)],
            vec![(0, 0.0), (2, -0.0)],
            vec![(0, -0.0), (1, -0.0)],
        ];
        let (output, _) = run_rows(&graph, usize::MAX, &admission(2, 192), &mut || Ok(())).unwrap();
        let KernelValues::WeightedEndpointPairs(edges) = output.values else {
            panic!("weighted forest");
        };
        assert_eq!(
            edges
                .iter()
                .map(|e| (e.vertex, e.neighbor, e.weight.to_bits()))
                .collect::<Vec<_>>(),
            vec![(0, 1, 0.0f64.to_bits()), (0, 2, (-0.0f64).to_bits())]
        );
    }

    #[test]
    fn malformed_or_nonfinite_late_rows_refuse_even_when_they_cannot_change_the_forest() {
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let mut graph = Fixture::shape();
            graph.rows[6].push((6, bad));
            assert!(matches!(
                run_rows(
                    &graph,
                    usize::MAX,
                    &admission(7, usize::MAX),
                    &mut || Ok(())
                ),
                Err(Error::Execution(ExecutionError::InvalidNumericResult))
            ));
        }
        let mut graph = Fixture::shape();
        graph.rows[6].push((99, 1.0));
        assert!(matches!(
            run_rows(
                &graph,
                usize::MAX,
                &admission(7, usize::MAX),
                &mut || Ok(())
            ),
            Err(Error::Execution(ExecutionError::InvalidUpstreamResult))
        ));
    }
}
