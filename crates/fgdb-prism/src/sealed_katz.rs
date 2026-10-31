//! Checked Katz iteration directly over authenticated compressed rows. Shared
//! scalar state retains canonical source order through all cooperative yields.

use super::{
    Cursor, Delivery, Finished, FnxCallSpec, FnxExecutionLimits, FnxMemoryLimits, KernelOutput,
    Result, ResultAdmission, Rows, SealedGraphView, SealedRows, admit, checkpoint,
    directional_pass_work, finish, finish_encoded,
};
use crate::katz::{Iteration, estimated_work, workspace};
use crate::sealed_control::{Control, Guard};
use crate::{FnxAlgorithm, FnxExecutionError, KatzOptions};
use std::convert::Infallible;
use std::future::Future;

fn options(call: &FnxCallSpec) -> Result<KatzOptions> {
    match call.algorithm() {
        FnxAlgorithm::KatzCentrality(options) => Ok(options),
        _ => Err(FnxExecutionError::InvalidUpstreamResult.into()),
    }
}

fn prepare(
    call: &FnxCallSpec,
    graph: &SealedGraphView,
    limits: FnxExecutionLimits,
    memory: FnxMemoryLimits,
    admission: &ResultAdmission,
) -> Result<(KatzOptions, usize)> {
    let options = options(call)?;
    admit("iterations", options.max_iter(), limits.max_iterations)?;
    let n = graph.node_count();
    admission.rows(n)?;
    let pass = directional_pass_work(n, graph.scan_incidence_bound(), graph.spec().directedness)?;
    let estimated = estimated_work::<Infallible>(
        n,
        graph.adjacency_entry_count(),
        pass,
        call.outputs().len(),
        options,
    )?;
    admit("estimated work", estimated, limits.max_estimated_work)?;
    admit(
        "kernel workspace bytes",
        workspace::<Infallible>(n, options)?,
        memory.max_kernel_workspace_bytes,
    )?;
    Ok((options, estimated))
}

fn run(
    graph: &impl Rows,
    options: KatzOptions,
    limit: usize,
    control: &mut impl FnMut() -> Result<()>,
) -> Result<(KernelOutput, usize)> {
    control()?;
    let mut state = Iteration::new::<Infallible>(graph.node_count(), options, limit)?;
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
    cx: &Control<'_, impl Guard>,
    graph: &SealedGraphView,
    limits: FnxExecutionLimits,
    memory: FnxMemoryLimits,
    admission: &ResultAdmission,
) -> Result<Finished> {
    let (options, estimated) = prepare(call, graph, limits, memory, admission)?;
    let rows = SealedRows { cx, graph };
    let mut control = || checkpoint(cx);
    let (mut output, peak) = run(
        &rows,
        options,
        memory.max_kernel_workspace_bytes,
        &mut control,
    )?;
    compressed_witness(&mut output);
    finish(
        call,
        graph,
        output,
        "fgdb-prism/sealed-katz-source-order-v1",
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
    let (options, estimated) = prepare(call, graph, limits, memory, &admission)?;
    control.tick().await?;
    let n = graph.node_count();
    let mut state = Iteration::new::<Infallible>(n, options, memory.max_kernel_workspace_bytes)?;
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
        "fgdb-prism/sealed-katz-source-order-cooperative-v1",
        estimated,
        peak,
        &mut || checkpoint(control.cx),
    )
}

#[cfg(test)]
#[path = "sealed_katz_tests.rs"]
mod tests;
