//! Exact local triangles over immutable compressed rows, without adjacency copies.
//!
//! Orient each projected edge by (retained-row scan cost, vertex ordinal). The
//! higher endpoint marks its lower neighbors; the middle endpoint scans only
//! for lower marked neighbors. Thus a triangle is discovered once, then adds
//! one to all three exact u64 counts. Loops never enter counts or degrees.
//!
//! Retained history, including invisible and excluded incidences on BOTH faces,
//! determines row cost. A bounded planning pass admits every later repeated
//! scan; a tiny visible degree cannot conceal a large historical rescan. The
//! source must remain immutable between planning and execution, as SealedRows
//! does. These are admission-model units, not an instruction/deadline guarantee.
//! The same complete planning pass proves rows with fewer than two non-loop
//! neighbors ineligible. Retain that proof in the existing cost directory and
//! omit their counting scans. A source also needs two eligible lower neighbors
//! before its marked row can contribute a triangle. Without a degree-one row,
//! retain the original counting loop without pruning checks in its hot path:
//! isolates cannot occur in any neighbor row and do not justify that work.

use super::{
    ComplexityWitness, Cursor, Delivery, ExecutionError, Finished, FnxAlgorithm, FnxCallSpec,
    FnxExecutionLimits, FnxMemoryLimits, KernelOutput, KernelValues, Result, ResultAdmission, Rows,
    SealedGraphView, SealedRows, add, admit, checkpoint, finish, mul, reserve,
};
use crate::sealed_control::Control;
use std::mem::size_of;

pub(super) fn execute(
    call: &FnxCallSpec,
    cx: &Control<'_>,
    graph: &SealedGraphView,
    limits: FnxExecutionLimits,
    memory: FnxMemoryLimits,
    admission: &ResultAdmission,
) -> Result<Finished> {
    let coefficients = matches!(call.algorithm(), FnxAlgorithm::ClusteringCoefficient);
    let workspace = workspace(graph.node_count(), coefficients)?;
    admission.rows(graph.node_count())?;
    admit(
        "kernel workspace bytes",
        workspace,
        memory.max_kernel_workspace_bytes,
    )?;
    let rows = SealedRows { cx, graph };
    let mut control = || checkpoint(cx);
    let plan = prepare(
        &rows,
        graph.scan_incidence_bound(),
        |source| {
            graph
                .retained_row_incidence_bound(source)
                .map_err(Into::into)
        },
        coefficients,
        limits.max_estimated_work,
        &mut control,
    )?;
    let estimated_work = plan.work;
    let output = run(&rows, plan, coefficients, &mut control)?;
    let kernel = if coefficients {
        "fgdb-prism/sealed-clustering-retained-cost-pruned-v2"
    } else {
        "fgdb-prism/sealed-triangles-retained-cost-pruned-v2"
    };
    finish(
        call,
        graph,
        output,
        kernel,
        estimated_work,
        workspace,
        admission,
        &mut control,
    )
}

fn workspace(n: usize, coefficients: bool) -> Result<usize> {
    // Counts alone retain costs + marks + u64 counts. Coefficients also retain
    // loop-free degrees. Drop costs/marks before allocating the score vector.
    let counting = mul(
        n,
        add(
            mul(if coefficients { 3 } else { 2 }, size_of::<usize>())?,
            size_of::<u64>(),
        )?,
    )?;
    let conversion = if coefficients {
        mul(n, size_of::<usize>() + size_of::<u64>() + size_of::<f64>())?
    } else {
        0
    };
    Ok(counting.max(conversion))
}

struct Plan {
    // Every real row cost is positive. Zero records a fully validated row
    // with fewer than two distinct non-loop neighbors, never a cheap scan.
    costs: Vec<usize>,
    degrees: Vec<usize>, // empty for counts-only execution
    work: usize,
    // Isolates alone cannot save any repeated inner-neighbor scans.
    prune: bool,
}

fn bits(n: usize) -> usize {
    (usize::BITS - n.leading_zeros()) as usize
}

fn row_work(n: usize, history: usize, incidences: usize) -> Result<usize> {
    // Two descriptor lookups, incoming chunk/prefix-directory searches, merged
    // raw pulls, endpoint searches and counting. This deliberately uses raw
    // source population, not the reduced degree. Property decoding happened at
    // source admission; numeric property lookup uses the existing row adapter.
    let search = mul(4, bits(history))?;
    add(
        add(16, search)?,
        mul(incidences, add(32, add(search, mul(2, bits(n))?)?)?)?,
    )
}

fn charge(work: &mut usize, amount: usize, limit: usize) -> Result<()> {
    let next = add(*work, amount)?;
    admit("estimated work", next, limit)?;
    *work = next;
    Ok(())
}

fn lower(costs: &[usize], left: usize, right: usize) -> bool {
    (costs[left], left) < (costs[right], right)
}

fn next_neighbor(
    row: &mut impl Cursor,
    n: usize,
    previous: &mut Option<usize>,
) -> Result<Option<usize>> {
    let Some((target, _)) = row.next()? else {
        return Ok(None);
    };
    // The production adapter already guarantees this contract. Check it at the
    // shared kernel seam as well; malformed test/future sources must not index
    // outside the workspace or duplicate a triangle silently.
    if target >= n || previous.is_some_and(|last| last >= target) {
        return Err(ExecutionError::InvalidUpstreamResult.into());
    }
    *previous = Some(target);
    Ok(Some(target))
}

fn prepare(
    graph: &impl Rows,
    history: usize,
    mut retained: impl FnMut(usize) -> Result<usize>,
    coefficients: bool,
    limit: usize,
    control: &mut impl FnMut() -> Result<()>,
) -> Result<Plan> {
    control()?;
    let n = graph.node_count();
    let mut work = 0;
    // Include directory-cost lookup, workspace initialization and result walks.
    charge(&mut work, mul(n, add(32, mul(4, bits(history))?)?)?, limit)?;
    let mut costs = reserve(n)?;
    let mut degrees = reserve(if coefficients { n } else { 0 })?;
    let mut prune = false;
    for source in 0..n {
        control()?;
        let cost = row_work(n, history, retained(source)?)?;
        // Planning, marking and outer-edge walks are all complete row scans.
        // Admit all three before opening even the planning cursor.
        charge(&mut work, mul(cost, 3)?, limit)?;
        costs.push(cost);
    }
    for source in 0..n {
        control()?;
        let mut row = graph.open(source)?;
        let mut previous = None;
        let mut seen = 0;
        let mut degree = 0;
        while let Some(target) = next_neighbor(&mut row, n, &mut previous)? {
            control()?;
            seen = add(seen, 1)?;
            if source != target {
                degree = add(degree, 1)?;
                if lower(&costs, target, source) {
                    // At most one later inner scan per oriented simple edge.
                    // Subsequent eligibility proofs may omit it entirely.
                    charge(&mut work, costs[target], limit)?;
                }
            }
        }
        if graph.degree(source) != Some(seen) {
            return Err(ExecutionError::InvalidUpstreamResult.into());
        }
        if degree < 2 {
            costs[source] = 0;
            prune |= degree == 1;
        }
        if coefficients {
            degrees.push(degree);
        }
    }
    control()?;
    Ok(Plan {
        costs,
        degrees,
        work,
        prune,
    })
}

fn run(
    graph: &impl Rows,
    plan: Plan,
    coefficients: bool,
    control: &mut impl FnMut() -> Result<()>,
) -> Result<KernelOutput> {
    let n = graph.node_count();
    let Plan {
        costs,
        degrees,
        prune,
        ..
    } = plan;
    if costs.len() != n || (coefficients && degrees.len() != n) {
        return Err(ExecutionError::InvalidUpstreamResult.into());
    }
    let mut counts = reserve(n)?;
    let mut marked = reserve(n)?;
    for _ in 0..n {
        control()?;
        counts.push(0u64);
        marked.push(usize::MAX);
    }
    let mut witness = ComplexityWitness {
        algorithm: if prune {
            "triangles_retained_cost_pruned_rows"
        } else {
            "triangles_retained_cost_oriented_rows"
        }
        .to_owned(),
        complexity_claim:
            "O(V + sum_v c(v) + sum_edges min(c(u),c(v))); c = retained row scan model".to_owned(),
        nodes_touched: 0,
        edges_scanned: 0,
        queue_peak: 0,
    };
    if prune {
        count_rows::<true>(
            graph,
            &costs,
            &mut counts,
            &mut marked,
            &mut witness,
            control,
        )?;
    } else {
        count_rows::<false>(
            graph,
            &costs,
            &mut counts,
            &mut marked,
            &mut witness,
            control,
        )?;
    }
    drop(marked);
    drop(costs);
    let values = if coefficients {
        let mut scores = reserve(n)?;
        for (count, degree) in counts.into_iter().zip(degrees) {
            control()?;
            let degree = degree as u128;
            let score = if degree < 2 {
                if count != 0 {
                    return Err(ExecutionError::InvalidUpstreamResult.into());
                }
                0.0
            } else {
                let denominator = degree
                    .checked_mul(degree - 1)
                    .ok_or(ExecutionError::SizeOverflow)?;
                let numerator = u128::from(count) * 2;
                if numerator > denominator {
                    return Err(ExecutionError::InvalidUpstreamResult.into());
                }
                // Same exact-integer-to-f64 ratio as the decoded/fnx oracle.
                numerator as f64 / denominator as f64
            };
            scores.push(score);
        }
        KernelValues::Scores(scores)
    } else {
        KernelValues::Counts(counts)
    };
    control()?;
    Ok(KernelOutput {
        values,
        row_count: n,
        witness,
    })
}

// Keep the caller's admitted vector-length facts visible to both specialized
// loops; outlining this seam adds bounds work to the unchanged dense path.
#[inline(always)]
fn count_rows<const PRUNE: bool>(
    graph: &impl Rows,
    costs: &[usize],
    counts: &mut [u64],
    marked: &mut [usize],
    witness: &mut ComplexityWitness,
    control: &mut impl FnMut() -> Result<()>,
) -> Result<()> {
    let n = graph.node_count();
    for source in 0..n {
        control()?;
        witness.nodes_touched = add(witness.nodes_touched, 1)?;
        if PRUNE && costs[source] == 0 {
            continue;
        }
        let mut lower_neighbors = 0usize;
        {
            let mut row = graph.open(source)?;
            let mut previous = None;
            while let Some(target) = next_neighbor(&mut row, n, &mut previous)? {
                control()?;
                witness.edges_scanned = add(witness.edges_scanned, 1)?;
                if lower(costs, target, source) && (!PRUNE || costs[target] != 0) {
                    marked[target] = source;
                    if PRUNE {
                        lower_neighbors = add(lower_neighbors, 1)?;
                    }
                }
            }
        }
        // A triangle's highest endpoint must mark its other two endpoints.
        // All rows were validated during planning; skipping a repeat scan
        // cannot hide a malformed row or bypass its original source errors.
        if PRUNE && lower_neighbors < 2 {
            continue;
        }
        let mut row = graph.open(source)?;
        let mut previous = None;
        while let Some(target) = next_neighbor(&mut row, n, &mut previous)? {
            control()?;
            witness.edges_scanned = add(witness.edges_scanned, 1)?;
            // The marking pass already proved eligibility and orientation.
            if if PRUNE {
                marked[target] != source
            } else {
                !lower(costs, target, source)
            } {
                continue;
            }
            let mut other = graph.open(target)?;
            let mut previous = None;
            while let Some(third) = next_neighbor(&mut other, n, &mut previous)? {
                control()?;
                witness.edges_scanned = add(witness.edges_scanned, 1)?;
                if lower(costs, third, target) && marked[third] == source {
                    // Total orientation makes source, target and third distinct.
                    for vertex in [source, target, third] {
                        counts[vertex] = counts[vertex]
                            .checked_add(1)
                            .ok_or(ExecutionError::SizeOverflow)?;
                    }
                }
            }
        }
    }
    Ok(())
}

// The asynchronous entrypoint shares the synchronous admission model and
// retained-cost orientation. Only raw pulls and scalar walks are scheduled;
// no eager call, decoded adjacency or row reopening is hidden inside a pause.
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
    YieldFuture: std::future::Future<Output = ()>,
{
    control.tick().await?;
    let coefficients = matches!(call.algorithm(), FnxAlgorithm::ClusteringCoefficient);
    let n = graph.node_count();
    let admission = ResultAdmission::for_delivery(call, limits, memory, delivery)?;
    admission.rows(n)?;
    let workspace = workspace(n, coefficients)?;
    admit(
        "kernel workspace bytes",
        workspace,
        memory.max_kernel_workspace_bytes,
    )?;

    let history = graph.scan_incidence_bound();
    let limit = limits.max_estimated_work;
    let mut work = 0;
    charge(&mut work, mul(n, add(32, mul(4, bits(history))?)?)?, limit)?;
    let mut costs = reserve(n)?;
    let mut degrees = reserve(if coefficients { n } else { 0 })?;
    let mut prune = false;
    for source in 0..n {
        control.tick().await?;
        let cost = row_work(n, history, graph.retained_row_incidence_bound(source)?)?;
        // Charge planning, marking and outer scans before opening any cursor.
        charge(&mut work, mul(cost, 3)?, limit)?;
        costs.push(cost);
    }
    for source in 0..n {
        control.tick().await?;
        let mut row = graph.neighbor_cursor_controlled(control.cx, source, None)?;
        let mut previous = None;
        let mut seen = 0;
        let mut degree = 0;
        while let Some(target) =
            next_cooperative_neighbor(&mut row, n, &mut previous, control).await?
        {
            control.tick().await?;
            seen = add(seen, 1)?;
            if source != target {
                degree = add(degree, 1)?;
                if lower(&costs, target, source) {
                    charge(&mut work, costs[target], limit)?;
                }
            }
        }
        if graph.degree(source) != Some(seen) {
            return Err(ExecutionError::InvalidUpstreamResult.into());
        }
        if degree < 2 {
            costs[source] = 0;
            prune |= degree == 1;
        }
        if coefficients {
            degrees.push(degree);
        }
    }

    control.tick().await?;
    let mut counts = reserve(n)?;
    let mut marked = reserve(n)?;
    for _ in 0..n {
        control.tick().await?;
        counts.push(0u64);
        marked.push(usize::MAX);
    }
    let mut witness = ComplexityWitness {
        algorithm: if prune {
            "triangles_retained_cost_pruned_rows"
        } else {
            "triangles_retained_cost_oriented_rows"
        }
        .to_owned(),
        complexity_claim:
            "O(V + sum_v c(v) + sum_edges min(c(u),c(v))); c = retained row scan model".to_owned(),
        nodes_touched: 0,
        edges_scanned: 0,
        queue_peak: 0,
    };
    if prune {
        count_rows_cooperative::<true, _, _>(
            graph,
            &costs,
            &mut counts,
            &mut marked,
            &mut witness,
            control,
        )
        .await?;
    } else {
        count_rows_cooperative::<false, _, _>(
            graph,
            &costs,
            &mut counts,
            &mut marked,
            &mut witness,
            control,
        )
        .await?;
    }
    // The coefficient conversion's peak allocation is admitted separately
    // from counting. Never retain costs/marks alongside the score vector.
    drop(marked);
    drop(costs);
    control.tick().await?;
    let values = if coefficients {
        let mut scores = reserve(n)?;
        for (count, degree) in counts.into_iter().zip(degrees) {
            control.tick().await?;
            let degree = degree as u128;
            let score = if degree < 2 {
                if count != 0 {
                    return Err(ExecutionError::InvalidUpstreamResult.into());
                }
                0.0
            } else {
                let denominator = degree
                    .checked_mul(degree - 1)
                    .ok_or(ExecutionError::SizeOverflow)?;
                let numerator = u128::from(count) * 2;
                if numerator > denominator {
                    return Err(ExecutionError::InvalidUpstreamResult.into());
                }
                numerator as f64 / denominator as f64
            };
            scores.push(score);
        }
        KernelValues::Scores(scores)
    } else {
        KernelValues::Counts(counts)
    };
    control.tick().await?;
    let mut encoded = super::EncodedRows::new(call, n, &admission)?;
    for index in 0..n {
        control.tick().await?;
        encoded.push(call, graph, &values, index, &mut || checkpoint(control.cx))?;
    }
    control.tick().await?;
    let kernel = if coefficients {
        "fgdb-prism/sealed-clustering-retained-cost-pruned-cooperative-v2"
    } else {
        "fgdb-prism/sealed-triangles-retained-cost-pruned-cooperative-v2"
    };
    super::finish_encoded(
        call,
        graph,
        encoded,
        values,
        witness,
        kernel,
        work,
        workspace,
        &mut || checkpoint(control.cx),
    )
}

async fn count_rows_cooperative<const PRUNE: bool, Yield, YieldFuture>(
    graph: &SealedGraphView,
    costs: &[usize],
    counts: &mut [u64],
    marked: &mut [usize],
    witness: &mut ComplexityWitness,
    control: &mut super::cooperative::Cooperate<'_, Yield>,
) -> Result<()>
where
    Yield: FnMut() -> YieldFuture,
    YieldFuture: std::future::Future<Output = ()>,
{
    let n = graph.node_count();
    for source in 0..n {
        control.tick().await?;
        witness.nodes_touched = add(witness.nodes_touched, 1)?;
        if PRUNE && costs[source] == 0 {
            continue;
        }
        let mut lower_neighbors = 0usize;
        {
            let mut row = graph.neighbor_cursor_controlled(control.cx, source, None)?;
            let mut previous = None;
            while let Some(target) =
                next_cooperative_neighbor(&mut row, n, &mut previous, control).await?
            {
                control.tick().await?;
                witness.edges_scanned = add(witness.edges_scanned, 1)?;
                if lower(costs, target, source) && (!PRUNE || costs[target] != 0) {
                    marked[target] = source;
                    if PRUNE {
                        lower_neighbors = add(lower_neighbors, 1)?;
                    }
                }
            }
        }
        if PRUNE && lower_neighbors < 2 {
            continue;
        }
        let mut row = graph.neighbor_cursor_controlled(control.cx, source, None)?;
        let mut previous = None;
        while let Some(target) =
            next_cooperative_neighbor(&mut row, n, &mut previous, control).await?
        {
            control.tick().await?;
            witness.edges_scanned = add(witness.edges_scanned, 1)?;
            if if PRUNE {
                marked[target] != source
            } else {
                !lower(costs, target, source)
            } {
                continue;
            }
            let mut other = graph.neighbor_cursor_controlled(control.cx, target, None)?;
            let mut previous = None;
            while let Some(third) =
                next_cooperative_neighbor(&mut other, n, &mut previous, control).await?
            {
                control.tick().await?;
                witness.edges_scanned = add(witness.edges_scanned, 1)?;
                if lower(costs, third, target) && marked[third] == source {
                    for vertex in [source, target, third] {
                        counts[vertex] = counts[vertex]
                            .checked_add(1)
                            .ok_or(ExecutionError::SizeOverflow)?;
                    }
                }
            }
        }
    }
    Ok(())
}

async fn next_cooperative_neighbor<Yield, YieldFuture>(
    row: &mut super::SealedNeighborCursor<'_>,
    n: usize,
    previous: &mut Option<usize>,
    control: &mut super::cooperative::Cooperate<'_, Yield>,
) -> Result<Option<usize>>
where
    Yield: FnMut() -> YieldFuture,
    YieldFuture: std::future::Future<Output = ()>,
{
    let Some((target, _)) = control.next(row).await? else {
        return Ok(None);
    };
    if target >= n || previous.is_some_and(|last| last >= target) {
        return Err(ExecutionError::InvalidUpstreamResult.into());
    }
    *previous = Some(target);
    Ok(Some(target))
}

#[cfg(test)]
#[path = "sealed_clustering_tests.rs"]
mod tests;
