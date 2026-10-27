//! Newman load centrality without retaining decoded adjacency or predecessors.
//! The shared state machine fixes source and reverse-BFS arithmetic order;
//! compressed row pulls and every scalar transition remain host-controlled.

use super::{
    Cursor, Delivery, ExecutionError, Finished, FnxCallSpec, FnxExecutionLimits, FnxMemoryLimits,
    Result, ResultAdmission, Rows, SealedGraphView, SealedRows, add, admit, checkpoint,
    directional_pass_work, finish, finish_encoded, mul, require_loop_free_degree,
};
use crate::foundation::{Load, LoadStep};
use crate::sealed_control::Control;
use std::convert::Infallible;
use std::future::Future;

fn output(state: Load) -> Result<(super::KernelOutput, usize)> {
    let (mut output, peak) = state.finish::<Infallible>()?;
    output.witness.complexity_claim = "O(V^2 + V*(V*(1+log(1+H)) + H*(1+log(1+H)+log(1+V)))); H is retained incidence count; O(V) kernel workspace".to_owned();
    Ok((output, peak))
}

fn prepare(
    call: &FnxCallSpec,
    graph: &SealedGraphView,
    limits: FnxExecutionLimits,
    memory: FnxMemoryLimits,
    admission: &ResultAdmission,
) -> Result<usize> {
    let n = graph.node_count();
    admission.rows(n)?;
    let pass = directional_pass_work(n, graph.scan_incidence_bound(), graph.spec().directedness)?;
    let scalar = crate::foundation::estimated_work(
        crate::FoundationAlgorithm::LoadCentrality,
        n,
        graph.adjacency_entry_count(),
    )
    .ok_or(ExecutionError::SizeOverflow)?;
    // Reopening parents can read every retained incidence twice per source.
    // Invisible/excluded histories therefore remain priced on every pass.
    let estimated = add(
        scalar,
        add(
            mul(add(mul(n, 2)?, 1)?, pass)?,
            mul(n, call.outputs().len())?,
        )?,
    )?;
    admit("estimated work", estimated, limits.max_estimated_work)?;
    admit(
        "kernel workspace bytes",
        Load::workspace(n).ok_or(ExecutionError::SizeOverflow)?,
        memory.max_kernel_workspace_bytes,
    )?;
    require_loop_free_degree(graph.edge_count(), graph.adjacency_entry_count())?;
    Ok(estimated)
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
    checkpoint(cx)?;
    let mut state = Load::new::<Infallible>(graph.node_count(), memory.max_kernel_workspace_bytes)?;
    let rows = SealedRows { cx, graph };
    loop {
        checkpoint(cx)?;
        match state.step::<Infallible>()? {
            LoadStep::Scalar => {}
            LoadStep::Complete => break,
            LoadStep::Row(vertex) => {
                let mut row = rows.open(vertex)?;
                while let Some((neighbor, _)) = row.next()? {
                    checkpoint(cx)?;
                    state.visit::<Infallible>(neighbor)?;
                }
                state.end_row::<Infallible>()?;
            }
        }
    }
    checkpoint(cx)?;
    let (output, peak) = output(state)?;
    finish(
        call,
        graph,
        output,
        "fgdb-prism/sealed-newman-load-v1",
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
    let estimated = prepare(call, graph, limits, memory, &admission)?;
    control.tick().await?;
    let n = graph.node_count();
    let mut state = Load::new::<Infallible>(n, memory.max_kernel_workspace_bytes)?;
    loop {
        control.tick().await?;
        match state.step::<Infallible>()? {
            LoadStep::Scalar => {}
            LoadStep::Complete => break,
            LoadStep::Row(vertex) => {
                let mut row = graph.neighbor_cursor_controlled(control.cx, vertex, None)?;
                while let Some((neighbor, _)) = control.next(&mut row).await? {
                    control.tick().await?;
                    state.visit::<Infallible>(neighbor)?;
                }
                state.end_row::<Infallible>()?;
            }
        }
    }
    control.tick().await?;
    let (output, peak) = output(state)?;
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
        "fgdb-prism/sealed-newman-load-cooperative-v1",
        estimated,
        peak,
        &mut || checkpoint(control.cx),
    )
}
