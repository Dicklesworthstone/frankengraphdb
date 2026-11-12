//! Newman load centrality without retaining decoded adjacency or predecessors.
//! The shared state machine fixes source and reverse-BFS arithmetic order;
//! compressed row pulls and every scalar transition remain host-controlled.
//! Directed predecessor recovery borrows the admitted graph's opposite face,
//! with its retained index and degree backing charged to kernel workspace.

use super::{
    Delivery, Directedness, ExecutionError, Finished, FnxCallSpec, FnxExecutionLimits,
    FnxMemoryLimits, Result, ResultAdmission, SealedGraphView, add, admit, checkpoint,
    directional_pass_work, finish, finish_encoded, mul, require_loop_free_degree,
};
use crate::foundation::{Load, LoadStep};
use crate::sealed::SealedRowSource;
use crate::sealed_control::{Control, Guard};
use std::convert::Infallible;
use std::future::Future;

fn output(state: Load, directed: bool) -> Result<(super::KernelOutput, usize)> {
    let (mut output, peak) = state.finish::<Infallible>()?;
    output.witness.complexity_claim = if directed {
        "O(V + C*(V*(1+log(1+H)) + H*(1+log(1+H)+log(1+V)))); C bounds weak component size, H retained incidences; O(V) scalar workspace plus a bounded compressed opposite index"
    } else {
        "O(V + C*(V*(1+log(1+H)) + H*(1+log(1+H)+log(1+V)))); C is the largest visible component, H retained incidences; O(V) kernel workspace"
    }
    .to_owned();
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
    let direction = graph.spec().directedness;
    let directed = direction != Directedness::Undirected;
    let retained = graph.scan_incidence_bound();
    let forward = directional_pass_work(n, retained, direction)?;
    let scalar = crate::foundation::estimated_work(
        crate::FoundationAlgorithm::LoadCentrality,
        n,
        graph.adjacency_entry_count(),
        directed,
    )
    .ok_or(ExecutionError::SizeOverflow)?;
    // Each row is opened only by sources in its weak component. Its
    // retained histories are still fully priced on every such visit, even
    // when invisible/excluded edges connect it to another component.
    let sources = Load::source_bound(n, graph.adjacency_entry_count());
    let pulls = if directed && n != 0 {
        let opposite = if direction == Directedness::Directed {
            Directedness::Reversed
        } else {
            Directedness::Directed
        };
        let incoming = directional_pass_work(n, retained, opposite)?;
        // Admission covers authentication and retained-locator construction,
        // sorting and encoding before the opposite face can allocate.
        let height = (usize::BITS - retained.leading_zeros()) as usize;
        let index = mul(retained, add(32, mul(height, 4)?)?)?;
        let setup = add(mul(incoming, 2)?, add(index, mul(n, 8)?)?)?;
        add(mul(sources, add(forward, incoming)?)?, add(forward, setup)?)?
    } else {
        // Preserve the existing undirected work admission exactly.
        mul(add(mul(sources, 2)?, 1)?, forward)?
    };
    let estimated = add(scalar, add(pulls, mul(n, call.outputs().len())?)?)?;
    admit("estimated work", estimated, limits.max_estimated_work)?;
    admit(
        "kernel workspace bytes",
        Load::workspace(n).ok_or(ExecutionError::SizeOverflow)?,
        memory.max_kernel_workspace_bytes,
    )?;
    if !directed {
        require_loop_free_degree(graph.edge_count(), graph.adjacency_entry_count())?;
    }
    Ok(estimated)
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
    checkpoint(cx)?;
    let n = graph.node_count();
    let directed = graph.spec().directedness != Directedness::Undirected;
    let mut state = Load::new::<Infallible>(n, memory.max_kernel_workspace_bytes)?;
    let reverse = if n != 0 && directed {
        let remaining = memory
            .max_kernel_workspace_bytes
            .checked_sub(state.workspace_bytes())
            .ok_or(ExecutionError::SizeOverflow)?;
        Some(graph.reverse_projection_controlled(cx, remaining)?)
    } else {
        None
    };
    let auxiliary = reverse
        .as_ref()
        .map_or(0, |face| face.charged_workspace_bytes());
    let outgoing = graph.row_view();
    let incoming = reverse.as_ref().map_or(outgoing, SealedRowSource::row_view);
    loop {
        checkpoint(cx)?;
        match state.step::<Infallible>()? {
            LoadStep::Scalar => {}
            LoadStep::Complete => break,
            LoadStep::Row {
                vertex,
                predecessors,
            } => {
                let face = if predecessors { incoming } else { outgoing };
                let mut row = face.neighbor_cursor_controlled(cx, vertex, None)?;
                while let Some((neighbor, _)) = row.next_controlled(cx)? {
                    checkpoint(cx)?;
                    state.visit::<Infallible>(neighbor)?;
                }
                state.end_row::<Infallible>()?;
            }
        }
    }
    checkpoint(cx)?;
    let (output, peak) = output(state, directed)?;
    let peak = add(peak, auxiliary)?;
    drop(reverse);
    finish(
        call,
        graph,
        output,
        "fgdb-prism/sealed-newman-load-v3",
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
    let estimated = prepare(call, graph, limits, memory, &admission)?;
    control.tick().await?;
    let n = graph.node_count();
    let directed = graph.spec().directedness != Directedness::Undirected;
    let mut state = Load::new::<Infallible>(n, memory.max_kernel_workspace_bytes)?;
    let reverse = if n != 0 && directed {
        let remaining = memory
            .max_kernel_workspace_bytes
            .checked_sub(state.workspace_bytes())
            .ok_or(ExecutionError::SizeOverflow)?;
        Some(control.reverse_projection(graph, remaining).await?)
    } else {
        None
    };
    let auxiliary = reverse
        .as_ref()
        .map_or(0, |face| face.charged_workspace_bytes());
    let outgoing = graph.row_view();
    let incoming = reverse.as_ref().map_or(outgoing, SealedRowSource::row_view);
    loop {
        control.tick().await?;
        match state.step::<Infallible>()? {
            LoadStep::Scalar => {}
            LoadStep::Complete => break,
            LoadStep::Row {
                vertex,
                predecessors,
            } => {
                let face = if predecessors { incoming } else { outgoing };
                let mut row = face.neighbor_cursor_controlled(control.cx, vertex, None)?;
                while let Some((neighbor, _)) = control.next(&mut row).await? {
                    control.tick().await?;
                    state.visit::<Infallible>(neighbor)?;
                }
                state.end_row::<Infallible>()?;
            }
        }
    }
    control.tick().await?;
    let (output, peak) = output(state, directed)?;
    let peak = add(peak, auxiliary)?;
    drop(reverse);
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
        "fgdb-prism/sealed-newman-load-cooperative-v3",
        estimated,
        peak,
        &mut || checkpoint(control.cx),
    )
}
