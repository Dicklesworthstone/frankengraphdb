//! Target-only integer hop distance over admitted, fallible compressed rows.
//! Both drivers drain each opened row through EOF before settling a target.
//! A one-hop cutoff uses the shared allocation-free source-row scan, retaining
//! the same bounded raw-history pulls and live checks as the FIFO driver.

use super::{
    Cursor, Delivery, Finished, FnxCallSpec, FnxExecutionLimits, FnxMemoryLimits, KernelOutput,
    Result, ResultAdmission, Rows, SealedGraphView, SealedRows, admit, checkpoint,
    directional_pass_work, finish, finish_encoded,
};
use crate::sealed_control::{Control, Guard};
use crate::target_bfs::{OneHop, Search, Step, estimated_work, one_hop, workspace};
use crate::{FnxAlgorithm, FnxExecutionError};
use std::convert::Infallible;
use std::future::Future;

#[derive(Clone, Copy)]
struct Target {
    source: usize,
    target: usize,
    cutoff: Option<usize>,
}

fn prepare(
    call: &FnxCallSpec,
    graph: &SealedGraphView,
    limits: FnxExecutionLimits,
    memory: FnxMemoryLimits,
) -> Result<(Target, usize)> {
    let FnxAlgorithm::ShortestPathLength {
        source,
        target,
        cutoff,
    } = call.algorithm()
    else {
        return Err(FnxExecutionError::InvalidUpstreamResult.into());
    };
    let source = graph
        .vertex_ordinal(source)
        .ok_or(FnxExecutionError::UnknownSource(source))?;
    let target = graph
        .vertex_ordinal(target)
        .ok_or(FnxExecutionError::UnknownTarget(target))?;
    let n = graph.node_count();
    let pass = directional_pass_work(n, graph.scan_incidence_bound(), graph.spec().directedness)?;
    let estimated =
        estimated_work::<Infallible>(n, graph.adjacency_entry_count(), pass, call.outputs().len())?;
    admit("estimated work", estimated, limits.max_estimated_work)?;
    admit(
        "kernel workspace bytes",
        workspace::<Infallible>(n, source, target, cutoff)?,
        memory.max_kernel_workspace_bytes,
    )?;
    Ok((
        Target {
            source,
            target,
            cutoff,
        },
        estimated,
    ))
}

fn run(
    graph: &impl Rows,
    target: Target,
    limit: usize,
    control: &mut impl FnMut() -> Result<()>,
) -> Result<(KernelOutput, usize)> {
    control()?;
    if one_hop(target.source, target.target, target.cutoff) {
        let mut state =
            OneHop::new::<Infallible>(graph.node_count(), target.source, target.target)?;
        control()?;
        let mut row = graph.open(target.source)?;
        while let Some((neighbor, _)) = row.next()? {
            control()?;
            state.visit::<Infallible>(neighbor)?;
        }
        control()?;
        control()?;
        return Ok((state.finish(), 0));
    }
    let mut state = Search::new::<Infallible>(
        graph.node_count(),
        target.source,
        target.target,
        target.cutoff,
        limit,
    )?;
    loop {
        control()?;
        match state.step::<Infallible>()? {
            Step::Scalar => {}
            Step::Complete => break,
            Step::Open(vertex) => {
                let mut row = graph.open(vertex)?;
                while let Some((neighbor, _)) = row.next()? {
                    control()?;
                    state.visit::<Infallible>(neighbor)?;
                }
                state.end_row::<Infallible>()?;
            }
        }
    }
    control()?;
    Ok(state.finish())
}

fn compressed_witness(output: &mut KernelOutput) {
    output.witness.complexity_claim.push_str(
        " plus separately bounded compressed history and ordinal lookup work for opened rows",
    );
}

pub(super) fn execute(
    call: &FnxCallSpec,
    cx: &Control<'_, impl Guard>,
    graph: &SealedGraphView,
    limits: FnxExecutionLimits,
    memory: FnxMemoryLimits,
    admission: &ResultAdmission,
) -> Result<Finished> {
    let (target, estimated) = prepare(call, graph, limits, memory)?;
    let rows = SealedRows { cx, graph };
    let mut control = || checkpoint(cx);
    let (mut output, peak) = run(
        &rows,
        target,
        memory.max_kernel_workspace_bytes,
        &mut control,
    )?;
    admission.rows(output.row_count)?;
    compressed_witness(&mut output);
    finish(
        call,
        graph,
        output,
        "fgdb-prism/sealed-target-bfs-bitmap-fifo-v1",
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
    let (target, estimated) = prepare(call, graph, limits, memory)?;
    control.tick().await?;
    let (mut output, peak) = if one_hop(target.source, target.target, target.cutoff) {
        let mut state =
            OneHop::new::<Infallible>(graph.node_count(), target.source, target.target)?;
        control.tick().await?;
        let mut row = graph.neighbor_cursor_controlled(control.cx, target.source, None)?;
        while let Some((neighbor, _)) = control.next(&mut row).await? {
            control.tick().await?;
            state.visit::<Infallible>(neighbor)?;
        }
        control.tick().await?;
        (state.finish(), 0)
    } else {
        let mut state = Search::new::<Infallible>(
            graph.node_count(),
            target.source,
            target.target,
            target.cutoff,
            memory.max_kernel_workspace_bytes,
        )?;
        loop {
            control.tick().await?;
            match state.step::<Infallible>()? {
                Step::Scalar => {}
                Step::Complete => break,
                Step::Open(vertex) => {
                    let mut row = graph.neighbor_cursor_controlled(control.cx, vertex, None)?;
                    while let Some((neighbor, _)) = control.next(&mut row).await? {
                        control.tick().await?;
                        state.visit::<Infallible>(neighbor)?;
                    }
                    state.end_row::<Infallible>()?;
                }
            }
        }
        state.finish()
    };
    control.tick().await?;
    admission.rows(output.row_count)?;
    compressed_witness(&mut output);
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
        "fgdb-prism/sealed-target-bfs-bitmap-fifo-cooperative-v1",
        estimated,
        peak,
        &mut || checkpoint(control.cx),
    )
}

#[cfg(test)]
#[path = "sealed_target_bfs_tests.rs"]
mod tests;
