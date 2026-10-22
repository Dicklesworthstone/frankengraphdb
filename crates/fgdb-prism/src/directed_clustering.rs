//! Directed local clustering under the Graphalytics neighbor-union law.
//!
//! For a center v, count each directed arc among its distinct incoming or
//! outgoing neighbors, then divide by d*(d-1). Reciprocal arcs count twice;
//! reciprocal incidence at v does not duplicate a neighbor. Self-loops are
//! ignored. This is distinct from the registered undirected coefficient.
//! Definition: LDBC Graphalytics, appendix algorithms, LCC pseudocode:
//! <https://github.com/ldbc/ldbc_graphalytics_docs/blob/c0f8927d144aa0216949c444eca96f42905a576d/tex/appendix_algorithms.tex>.
//!
//! Merge the admitted CSR faces into degree-oriented weak-neighbor rows.
//! Each undirected triangle is discovered once. Its contribution to a center
//! is the number (one or two) of directed arcs on the opposite side. This
//! keeps the directed semantics without rescanning every neighbor's complete
//! outgoing row for every center. The temporary forward CSR is query scratch,
//! never a new graph authority or a retained projection cache.

use crate::execute::{KernelOutput, KernelValues, reserve};
use crate::{
    ComplexityWitness, FNX_METER_RUN, FnxExecutionError, FnxMeter, GraphView, SnapshotGraphView,
};

pub(crate) const COMPLEXITY: &str = "O(|V| + |E| + sum_weak_edges min(deg_weak(u), deg_weak(v)))";

/// Bound admission, initialization, both weak-neighbor merges, forward-row
/// marking/scanning, and output conversion before allocating kernel scratch.
/// A forward edge points to the lower-degree endpoint, whose forward row is
/// no larger than either endpoint's weak degree. The sum below uses original
/// arcs and upper bounds on weak degree, so reciprocal arcs only overcount.
/// Unlike a degree-product bound, a high-degree star admits linear work.
pub(crate) fn estimated_work<C>(
    graph: &SnapshotGraphView,
    checkpoint: &mut impl FnxMeter<C>,
) -> Result<usize, FnxExecutionError<C>> {
    let mut work = graph
        .node_count()
        .checked_mul(10)
        .and_then(|work| work.checked_add(8))
        .ok_or(FnxExecutionError::SizeOverflow)?;
    // Every incoming/outgoing row has at most m entries. This proof plus a
    // per-run upper sum excludes an arithmetic refusal after a bulk charge.
    // Otherwise keep one unit per item, so cancellation never jumps ahead
    // of the first overflowing addition.
    let incidence_sums_fit = graph.edge_count().checked_mul(2).is_some();
    for vertex in 0..graph.node_count() {
        checkpoint.unit().map_err(FnxExecutionError::Cancelled)?;
        let outgoing = graph
            .neighbors_indices(vertex)
            .ok_or(FnxExecutionError::InvalidUpstreamResult)?;
        let incoming = graph
            .in_neighbors_indices(vertex)
            .ok_or(FnxExecutionError::InvalidUpstreamResult)?
            .len();
        let degree_bound = outgoing
            .len()
            .checked_add(incoming)
            .ok_or(FnxExecutionError::SizeOverflow)?;
        work = outgoing
            .len()
            .checked_mul(8)
            .and_then(|steps| work.checked_add(steps))
            .ok_or(FnxExecutionError::SizeOverflow)?;
        let mut first = 0;
        while first < outgoing.len() {
            let proposed = (outgoing.len() - first).min(FNX_METER_RUN);
            let count = if incidence_sums_fit
                && degree_bound
                    .checked_mul(proposed)
                    .and_then(|bound| work.checked_add(bound))
                    .is_some()
            {
                proposed
            } else {
                1
            };
            checkpoint
                .units(count)
                .map_err(FnxExecutionError::Cancelled)?;
            for &target in &outgoing[first..first + count] {
                if vertex == target {
                    continue;
                }
                let other = graph
                    .neighbors_indices(target)
                    .ok_or(FnxExecutionError::InvalidUpstreamResult)?
                    .len()
                    .checked_add(
                        graph
                            .in_neighbors_indices(target)
                            .ok_or(FnxExecutionError::InvalidUpstreamResult)?
                            .len(),
                    )
                    .ok_or(FnxExecutionError::SizeOverflow)?;
                work = work
                    .checked_add(degree_bound.min(other))
                    .ok_or(FnxExecutionError::SizeOverflow)?;
            }
            first += count;
        }
    }
    Ok(work)
}

/// Visit distinct weak neighbors in VId order, preserving the multiplicity of
/// the directed pair. Every consumed raw incidence is metered, including both
/// faces of an ignored loop. At most 64 raw incidences are admitted per run;
/// a reciprocal pair crossing a run boundary admits its remaining face before
/// visiting the neighbor. The visitor enables runs only after any fallible
/// allocation succeeds; until then no future neighbor is prepaid.
fn weak_neighbors<C>(
    graph: &SnapshotGraphView,
    source: usize,
    checkpoint: &mut impl FnxMeter<C>,
    mut can_batch: bool,
    mut visit: impl FnMut(usize, u8) -> Result<bool, FnxExecutionError<C>>,
) -> Result<usize, FnxExecutionError<C>> {
    let mut outgoing = graph
        .neighbors_indices(source)
        .ok_or(FnxExecutionError::InvalidUpstreamResult)?;
    let mut incoming = graph
        .in_neighbors_indices(source)
        .ok_or(FnxExecutionError::InvalidUpstreamResult)?;
    let scanned = outgoing
        .len()
        .checked_add(incoming.len())
        .ok_or(FnxExecutionError::SizeOverflow)?;
    let mut pending = scanned;
    let mut admitted = 0usize;
    while !outgoing.is_empty() || !incoming.is_empty() {
        let neighbor = match (outgoing.first(), incoming.first()) {
            (Some(&out), Some(&input)) => out.min(input),
            (Some(&out), None) => out,
            (None, Some(&input)) => input,
            (None, None) => unreachable!("nonempty incidence face"),
        };
        let out = outgoing.first() == Some(&neighbor);
        let input = incoming.first() == Some(&neighbor);
        let multiplicity = u8::from(out) + u8::from(input);
        while admitted < usize::from(multiplicity) {
            let count = if !can_batch || admitted != 0 {
                // A reciprocal pair can straddle a run. Pay only its missing
                // face, rather than leaving 65 raw steps behind one poll.
                1
            } else {
                pending.min(FNX_METER_RUN)
            };
            checkpoint
                .units(count)
                .map_err(FnxExecutionError::Cancelled)?;
            pending -= count;
            admitted += count;
        }
        admitted -= usize::from(multiplicity);
        if out {
            outgoing = &outgoing[1..];
        }
        if input {
            incoming = &incoming[1..];
        }
        if neighbor != source {
            can_batch = visit(neighbor, multiplicity)?;
        }
    }
    Ok(scanned)
}

fn add<C>(value: &mut usize, amount: usize) -> Result<(), FnxExecutionError<C>> {
    *value = value
        .checked_add(amount)
        .ok_or(FnxExecutionError::SizeOverflow)?;
    Ok(())
}

fn vector_bytes<T, C>(values: &Vec<T>) -> Result<usize, FnxExecutionError<C>> {
    values
        .capacity()
        .checked_mul(size_of::<T>())
        .ok_or(FnxExecutionError::SizeOverflow)
}

pub(crate) fn run<C>(
    graph: &SnapshotGraphView,
    checkpoint: &mut impl FnxMeter<C>,
) -> Result<(KernelOutput, usize), FnxExecutionError<C>> {
    let n = graph.node_count();
    let mut degrees = reserve(n)?;
    let mut witness = ComplexityWitness {
        algorithm: "directed_clustering_degree_oriented_multiplicity".to_owned(),
        complexity_claim: COMPLEXITY.to_owned(),
        nodes_touched: 0,
        edges_scanned: 0,
        // Maximum forward neighborhood marked at once, not the weak degree.
        queue_peak: 0,
    };
    for source in 0..n {
        checkpoint.unit().map_err(FnxExecutionError::Cancelled)?;
        let mut degree = 0;
        let scans = weak_neighbors(graph, source, checkpoint, true, |_, _| {
            // A simple weak neighborhood has at most n-1 entries, so this
            // increment cannot overflow after a successful projection build.
            add(&mut degree, 1)?;
            Ok(true)
        })?;
        degrees.push(degree);
        add(&mut witness.nodes_touched, 1)?;
        add(&mut witness.edges_scanned, scans)?;
    }
    let mut offsets = reserve(n.checked_add(1).ok_or(FnxExecutionError::SizeOverflow)?)?;
    let mut targets = Vec::new();
    let mut multiplicities = Vec::new();
    let mut has_wedges = false;
    offsets.push(0);
    for source in 0..n {
        checkpoint.unit().map_err(FnxExecutionError::Cancelled)?;
        add(&mut witness.nodes_touched, 1)?;
        if degrees[source] >= 2 {
            let scans = weak_neighbors(
                graph,
                source,
                checkpoint,
                !targets.is_empty(),
                |target, multiplicity| {
                    if degrees[target] >= 2 && (degrees[target], target) < (degrees[source], source)
                    {
                        if targets.is_empty() {
                            // At most one retained weak edge for each original
                            // directed arc. Allocate lazily: a star needs none.
                            targets = reserve(graph.edge_count())?;
                            multiplicities = reserve(graph.edge_count())?;
                        }
                        if targets.len() == targets.capacity()
                            || multiplicities.len() == multiplicities.capacity()
                        {
                            return Err(FnxExecutionError::InvalidUpstreamResult);
                        }
                        targets.push(target);
                        multiplicities.push(multiplicity);
                    }
                    Ok(!targets.is_empty())
                },
            )?;
            add(&mut witness.edges_scanned, scans)?;
        }
        has_wedges |= targets.len() - offsets[source] >= 2;
        offsets.push(targets.len());
    }

    let mut closed = reserve(n)?;
    let mut marks = if has_wedges { reserve(n)? } else { Vec::new() };
    for start in (0..n).step_by(FNX_METER_RUN) {
        let count = (n - start).min(FNX_METER_RUN);
        checkpoint
            .units(count)
            .map_err(FnxExecutionError::Cancelled)?;
        closed.resize(start + count, 0usize);
        if has_wedges {
            marks.resize(start + count, usize::MAX);
        }
    }
    let mut workspace = 0usize;
    for bytes in [
        vector_bytes(&degrees)?,
        vector_bytes(&offsets)?,
        vector_bytes(&targets)?,
        vector_bytes(&multiplicities)?,
        vector_bytes(&closed)?,
        vector_bytes(&marks)?,
    ] {
        add(&mut workspace, bytes)?;
    }
    if has_wedges {
        // Each numerator is <= (n-1)*(n-2). Batch only when no count can
        // overflow; larger domains preserve the exact scalar refusal unit.
        let count_run = if n
            .saturating_sub(1)
            .checked_mul(n.saturating_sub(2))
            .is_some()
        {
            FNX_METER_RUN
        } else {
            1
        };
        for source in 0..n {
            checkpoint.unit().map_err(FnxExecutionError::Cancelled)?;
            add(&mut witness.nodes_touched, 1)?;
            let (start, end) = (offsets[source], offsets[source + 1]);
            if end - start < 2 {
                continue;
            }
            witness.queue_peak = witness.queue_peak.max(end - start);
            for first in (start..end).step_by(FNX_METER_RUN) {
                let last = end.min(first.saturating_add(FNX_METER_RUN));
                checkpoint
                    .units(last - first)
                    .map_err(FnxExecutionError::Cancelled)?;
                add(&mut witness.edges_scanned, last - first)?;
                for slot in first..last {
                    marks[targets[slot]] = slot;
                }
            }
            for slot in start..end {
                checkpoint.unit().map_err(FnxExecutionError::Cancelled)?;
                add(&mut witness.edges_scanned, 1)?;
                let target = targets[slot];
                let mut first = offsets[target];
                let last = offsets[target + 1];
                while first < last {
                    let count = (last - first).min(count_run);
                    checkpoint
                        .units(count)
                        .map_err(FnxExecutionError::Cancelled)?;
                    add(&mut witness.edges_scanned, count)?;
                    for next in first..first + count {
                        let third = targets[next];
                        let marked = marks[third];
                        // Marked slots from an earlier row cannot lie in this
                        // row's disjoint interval. No O(n) stamp clear needed.
                        if (start..end).contains(&marked) {
                            add(&mut closed[source], usize::from(multiplicities[next]))?;
                            add(&mut closed[target], usize::from(multiplicities[marked]))?;
                            add(&mut closed[third], usize::from(multiplicities[slot]))?;
                        }
                    }
                    first += count;
                }
            }
        }
    }
    drop(offsets);
    drop(targets);
    drop(multiplicities);
    drop(marks);
    let mut scores = reserve(n)?;
    let mut conversion_bytes = vector_bytes(&degrees)?;
    add(&mut conversion_bytes, vector_bytes(&closed)?)?;
    add(&mut conversion_bytes, vector_bytes(&scores)?)?;
    workspace = workspace.max(conversion_bytes);
    for vertex in 0..n {
        checkpoint.unit().map_err(FnxExecutionError::Cancelled)?;
        add(&mut witness.nodes_touched, 1)?;
        let degree = degrees[vertex];
        scores.push(if degree < 2 {
            0.0
        } else {
            let pairs = degree
                .checked_mul(degree - 1)
                .ok_or(FnxExecutionError::SizeOverflow)?;
            closed[vertex] as f64 / pairs as f64
        });
    }
    Ok((
        KernelOutput {
            values: KernelValues::Scores(scores),
            row_count: n,
            witness,
        },
        workspace,
    ))
}

/// The previous production kernel, retained only as a paired correctness and
/// performance comparator. It deliberately keeps its original work and memory.
#[cfg(test)]
fn incumbent_estimated_work<C>(
    graph: &SnapshotGraphView,
    checkpoint: &mut impl FnxMeter<C>,
) -> Result<usize, FnxExecutionError<C>> {
    let mut work = graph
        .node_count()
        .checked_mul(6)
        .ok_or(FnxExecutionError::SizeOverflow)?;
    for vertex in 0..graph.node_count() {
        checkpoint.unit().map_err(FnxExecutionError::Cancelled)?;
        let outgoing = graph
            .neighbors_indices(vertex)
            .ok_or(FnxExecutionError::InvalidUpstreamResult)?
            .len();
        let incoming = graph
            .in_neighbors_indices(vertex)
            .ok_or(FnxExecutionError::InvalidUpstreamResult)?
            .len();
        let incidence = outgoing
            .checked_add(incoming)
            .ok_or(FnxExecutionError::SizeOverflow)?;
        work = incidence
            .checked_mul(outgoing)
            .and_then(|scans| incidence.checked_mul(2)?.checked_add(scans))
            .and_then(|steps| work.checked_add(steps))
            .ok_or(FnxExecutionError::SizeOverflow)?;
    }
    Ok(work)
}

#[cfg(test)]
fn incumbent<C>(
    graph: &SnapshotGraphView,
    checkpoint: &mut impl FnxMeter<C>,
) -> Result<(KernelOutput, usize), FnxExecutionError<C>> {
    let n = graph.node_count();
    let mut max_neighbors = 0;
    for vertex in 0..n {
        checkpoint.unit().map_err(FnxExecutionError::Cancelled)?;
        let outgoing = graph
            .neighbors_indices(vertex)
            .ok_or(FnxExecutionError::InvalidUpstreamResult)?;
        let incoming = graph
            .in_neighbors_indices(vertex)
            .ok_or(FnxExecutionError::InvalidUpstreamResult)?;
        max_neighbors = max_neighbors.max(
            outgoing
                .len()
                .checked_add(incoming.len())
                .ok_or(FnxExecutionError::SizeOverflow)?
                .min(n.saturating_sub(1)),
        );
    }
    let mut marks = reserve(n)?;
    let mut scores = reserve(n)?;
    let mut neighbors = reserve(max_neighbors)?;
    let workspace = marks
        .capacity()
        .checked_mul(size_of::<usize>())
        .and_then(|bytes| bytes.checked_add(scores.capacity().checked_mul(size_of::<f64>())?))
        .and_then(|bytes| bytes.checked_add(neighbors.capacity().checked_mul(size_of::<usize>())?))
        .ok_or(FnxExecutionError::SizeOverflow)?;
    for _ in 0..n {
        checkpoint.unit().map_err(FnxExecutionError::Cancelled)?;
        marks.push(usize::MAX);
        scores.push(0.0);
    }
    let mut witness = ComplexityWitness {
        algorithm: "directed_clustering_neighbor_union".to_owned(),
        complexity_claim: "O(|V| + |E| + sum_v deg_weak(v) * deg_out(v))".to_owned(),
        nodes_touched: 0,
        edges_scanned: 0,
        // Maximum active neighborhood in the reusable ordinal buffer.
        queue_peak: 0,
    };
    for center in 0..n {
        checkpoint.unit().map_err(FnxExecutionError::Cancelled)?;
        neighbors.clear();
        let outgoing = graph
            .neighbors_indices(center)
            .ok_or(FnxExecutionError::InvalidUpstreamResult)?;
        let incoming = graph
            .in_neighbors_indices(center)
            .ok_or(FnxExecutionError::InvalidUpstreamResult)?;
        for row in [outgoing, incoming] {
            for chunk in row.chunks(FNX_METER_RUN) {
                checkpoint
                    .units(chunk.len())
                    .map_err(FnxExecutionError::Cancelled)?;
                for &neighbor in chunk {
                    if neighbor != center && marks[neighbor] != center {
                        marks[neighbor] = center;
                        neighbors.push(neighbor);
                    }
                }
                witness.edges_scanned = witness
                    .edges_scanned
                    .checked_add(chunk.len())
                    .ok_or(FnxExecutionError::SizeOverflow)?;
            }
        }
        let degree = neighbors.len();
        witness.queue_peak = witness.queue_peak.max(degree);
        if degree >= 2 {
            let mut closed = 0usize;
            for &neighbor in &neighbors {
                checkpoint.unit().map_err(FnxExecutionError::Cancelled)?;
                let row = graph
                    .neighbors_indices(neighbor)
                    .ok_or(FnxExecutionError::InvalidUpstreamResult)?;
                for chunk in row.chunks(FNX_METER_RUN) {
                    checkpoint
                        .units(chunk.len())
                        .map_err(FnxExecutionError::Cancelled)?;
                    for &target in chunk {
                        if target != neighbor && marks[target] == center {
                            closed = closed
                                .checked_add(1)
                                .ok_or(FnxExecutionError::SizeOverflow)?;
                        }
                    }
                    witness.edges_scanned = witness
                        .edges_scanned
                        .checked_add(chunk.len())
                        .ok_or(FnxExecutionError::SizeOverflow)?;
                }
            }
            let pairs = degree
                .checked_mul(degree - 1)
                .ok_or(FnxExecutionError::SizeOverflow)?;
            scores[center] = closed as f64 / pairs as f64;
        }
        witness.nodes_touched += 1;
    }
    Ok((
        KernelOutput {
            values: KernelValues::Scores(scores),
            row_count: n,
            witness,
        },
        workspace,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Directedness, FnxCallSpec, FnxExecutionLimits, FnxGraphKind, FnxParameters, FnxValue,
        ParallelEdgePolicy, ProjectionEdge, ProjectionLimits, ProjectionSpec, SelfLoopPolicy,
        SnapshotBinding,
    };
    use fgdb_types::{CommitSeq, EId, VId, ids::ObjectId};
    use std::collections::BTreeSet;

    fn vertex(index: usize) -> VId {
        VId((1u128 << 103) + 19 * index as u128)
    }

    fn graph(n: usize, arcs: &[(usize, usize)], directed: bool) -> SnapshotGraphView {
        let vertices: Vec<_> = (0..n).map(vertex).collect();
        let edges: Vec<_> = arcs
            .iter()
            .enumerate()
            .map(|(index, &(source, target))| ProjectionEdge {
                eid: EId(index as u128 + 1),
                source: vertex(source),
                target: vertex(target),
                weight: -7.0,
            })
            .collect();
        SnapshotGraphView::build(
            SnapshotBinding {
                root: ObjectId([31; 32]),
                as_of: CommitSeq(7),
            },
            &vertices,
            &edges,
            ProjectionSpec {
                directedness: if directed {
                    Directedness::Directed
                } else {
                    Directedness::Undirected
                },
                parallel_edges: ParallelEdgePolicy::Minimum,
                self_loops: SelfLoopPolicy::Keep,
            },
            ProjectionLimits {
                max_vertices: n,
                max_input_edges: edges.len(),
                max_adjacency_entries: edges.len() * 2,
                max_workspace_bytes: 1 << 24,
            },
        )
        .unwrap()
    }

    fn limits() -> FnxExecutionLimits {
        FnxExecutionLimits {
            max_iterations: 1,
            max_result_rows: usize::MAX,
            max_estimated_work: usize::MAX,
        }
    }

    // Deliberately use ordered pairs over a set of raw arcs, not projected
    // row scans, stamps, or the production counting loop.
    fn oracle(n: usize, raw: &[(usize, usize)]) -> (Vec<f64>, usize, usize) {
        let arcs: BTreeSet<_> = raw.iter().copied().collect();
        let mut scores = Vec::new();
        let mut scans = 2 * arcs.len();
        let mut peak = 0;
        for center in 0..n {
            let neighbors: BTreeSet<_> = (0..n)
                .filter(|&other| {
                    other != center
                        && (arcs.contains(&(center, other)) || arcs.contains(&(other, center)))
                })
                .collect();
            let degree = neighbors.len();
            peak = peak.max(degree);
            let mut closed = 0;
            if degree >= 2 {
                for &a in &neighbors {
                    scans += arcs.iter().filter(|&&(source, _)| source == a).count();
                    for &b in &neighbors {
                        closed += usize::from(a != b && arcs.contains(&(a, b)));
                    }
                }
            }
            scores.push(if degree < 2 {
                0.0
            } else {
                closed as f64 / (degree * (degree - 1)) as f64
            });
        }
        (scores, scans, peak)
    }

    #[test]
    fn all_small_directed_graphs_match_independent_ordered_pair_oracle() {
        for n in 0..=4 {
            let possible: Vec<_> = (0..n)
                .flat_map(|a| (0..n).filter(move |&b| a != b).map(move |b| (a, b)))
                .collect();
            for mask in 0usize..1usize << possible.len() {
                let arcs: Vec<_> = possible
                    .iter()
                    .enumerate()
                    .filter_map(|(i, &edge)| ((mask >> i) & 1 != 0).then_some(edge))
                    .collect();
                let view = graph(n, &arcs, true);
                let mut units = 0;
                let (output, _) = run(&view, &mut || {
                    units += 1;
                    Ok::<_, ()>(())
                })
                .unwrap();
                let KernelValues::Scores(actual) = output.values else {
                    panic!("score output")
                };
                let (expected, scanned, peak) = oracle(n, &arcs);
                assert_eq!(
                    actual.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
                    expected.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
                    "n={n} mask={mask}"
                );
                let (previous, _) = incumbent(&view, &mut || Ok::<_, ()>(())).unwrap();
                let KernelValues::Scores(previous_scores) = previous.values else {
                    panic!("score output")
                };
                assert_eq!(
                    actual.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
                    previous_scores
                        .iter()
                        .map(|v| v.to_bits())
                        .collect::<Vec<_>>(),
                    "incumbent n={n} mask={mask}"
                );
                // The independent raw-arc oracle still checks the incumbent's
                // physical accounting; the new kernel declares its own work.
                assert_eq!(previous.witness.nodes_touched, n);
                assert_eq!(previous.witness.edges_scanned, scanned);
                assert_eq!(previous.witness.queue_peak, peak);
                let estimate = estimated_work(&view, &mut || Ok::<_, ()>(())).unwrap();
                assert!(estimate >= output.witness.edges_scanned);
                assert!(estimate >= units, "n={n} mask={mask}: {units} > {estimate}");
            }
        }
    }

    #[test]
    fn public_call_binds_full_width_ids_reciprocal_arcs_loops_and_isolates() {
        let arcs = [
            (0, 1),
            (1, 0),
            (0, 2),
            (1, 2),
            (2, 1),
            (2, 2),
            (3, 3),
            (0, 1),
        ];
        let view = graph(5, &arcs, true);
        let bound = FnxCallSpec::bind(
            "CALL fnx.directed_clustering_coefficient() YIELD node, score",
            &FnxParameters::new(),
        )
        .unwrap();
        let result = bound.execute(&view, limits(), || Ok::<_, ()>(())).unwrap();
        let expected = oracle(5, &arcs).0;
        assert_eq!(result.columns, ["node", "score"]);
        for (index, row) in result.rows.iter().enumerate() {
            assert_eq!(
                row,
                &[
                    FnxValue::Vertex(vertex(index)),
                    FnxValue::Score(expected[index])
                ]
            );
        }
        assert_eq!(expected, [1.0, 0.5, 1.0, 0.0, 0.0]);
        assert_eq!(
            result.certificate.execution_kernel,
            "fgdb-prism/directed-clustering-oriented-multiplicity-v2"
        );
        assert_eq!(
            result.certificate.numeric_profile,
            "fgdb-directed-clustering-exact-count-f64-ratio-v1"
        );
        let undirected = graph(5, &arcs, false);
        assert!(matches!(
            bound.execute(&undirected, limits(), || Ok::<_, ()>(())),
            Err(FnxExecutionError::GraphKind {
                required: FnxGraphKind::Directed
            })
        ));
        assert!(matches!(
            FnxCallSpec::clustering_coefficient().execute(&view, limits(), || Ok::<_, ()>(())),
            Err(FnxExecutionError::GraphKind {
                required: FnxGraphKind::Undirected
            })
        ));
    }

    #[test]
    fn every_logical_refusal_stops_the_complete_call_without_an_extra_checkpoint() {
        let arcs: Vec<_> = (0..9)
            .flat_map(|a| {
                (0..9)
                    .filter(move |&b| a != b && (a * 3 + b) % 4 != 0)
                    .map(move |b| (a, b))
            })
            .collect();
        let view = graph(10, &arcs, true);
        let call = FnxCallSpec::directed_clustering_coefficient();
        let mut total = 0;
        let expected = call
            .execute(&view, limits(), || {
                total += 1;
                Ok::<_, usize>(())
            })
            .unwrap();
        for cut in 1..=total {
            let mut seen = 0;
            let result = call.execute(&view, limits(), || {
                seen += 1;
                if seen == cut { Err(cut) } else { Ok(()) }
            });
            assert!(matches!(result, Err(FnxExecutionError::Cancelled(at)) if at == cut));
            assert_eq!(seen, cut);
        }
        let mut rows = limits();
        rows.max_result_rows = view.node_count() - 1;
        assert!(matches!(
            call.execute(&view, rows, || Ok::<_, ()>(())),
            Err(FnxExecutionError::LimitExceeded {
                resource: "result rows",
                ..
            })
        ));
        let mut work = limits();
        work.max_estimated_work = expected.certificate.estimated_work - 1;
        assert!(matches!(
            call.execute(&view, work, || Ok::<_, ()>(())),
            Err(FnxExecutionError::LimitExceeded {
                resource: "estimated work",
                ..
            })
        ));
        assert_eq!(
            call.execute(&view, limits(), || Ok::<_, ()>(()))
                .unwrap()
                .rows,
            expected.rows
        );
    }

    #[test]
    fn hub_rows_keep_bulk_charges_bounded_and_stop_at_the_refusing_batch() {
        struct Meter {
            units: usize,
            batches: Vec<usize>,
            stop_at: Option<usize>,
            stopped: bool,
        }
        impl FnxMeter<usize> for Meter {
            fn unit(&mut self) -> Result<(), usize> {
                assert!(!self.stopped, "work after refusal");
                self.units += 1;
                Ok(())
            }
            fn poll(&mut self) -> Result<(), usize> {
                assert!(!self.stopped, "poll after refusal");
                Ok(())
            }
            fn units(&mut self, count: usize) -> Result<(), usize> {
                assert!(!self.stopped, "batch after refusal");
                assert!((1..=FNX_METER_RUN).contains(&count));
                self.batches.push(count);
                if self.stop_at == Some(self.batches.len()) {
                    self.stopped = true;
                    return Err(self.batches.len());
                }
                self.units += count;
                Ok(())
            }
        }
        let n = 2 * FNX_METER_RUN + 4;
        let arcs: Vec<_> = (1..n)
            .map(|target| (0, target))
            .chain((1..n).map(|source| (source, if source + 1 == n { 1 } else { source + 1 })))
            .collect();
        let view = graph(n, &arcs, true);
        let mut scalar_units = 0;
        let mut scalar_control = || {
            scalar_units += 1;
            Ok::<_, usize>(())
        };
        let scalar_estimate = estimated_work(&view, &mut scalar_control).unwrap();
        let (scalar, _) = run(&view, &mut scalar_control).unwrap();
        let mut complete = Meter {
            units: 0,
            batches: Vec::new(),
            stop_at: None,
            stopped: false,
        };
        assert_eq!(
            estimated_work(&view, &mut complete).unwrap(),
            scalar_estimate
        );
        let (bulk, _) = run(&view, &mut complete).unwrap();
        assert_eq!(complete.units, scalar_units);
        let KernelValues::Scores(scalar) = scalar.values else {
            panic!("scores")
        };
        let KernelValues::Scores(bulk) = bulk.values else {
            panic!("scores")
        };
        assert_eq!(bulk, scalar);
        // Admission, the hub's forward row and initialization use full runs.
        // Weak-row merges also use bounded raw-incidence runs.
        // Every multi-unit run must stop without a later mutation or poll.
        let first_center_full_runs = (n - 1) / FNX_METER_RUN;
        let full_runs: Vec<_> = complete
            .batches
            .iter()
            .enumerate()
            .filter_map(|(index, &count)| (count == FNX_METER_RUN).then_some(index + 1))
            .collect();
        assert!(full_runs.len() > first_center_full_runs);
        for at in full_runs {
            let mut refused = Meter {
                units: 0,
                batches: Vec::new(),
                stop_at: Some(at),
                stopped: false,
            };
            let result = estimated_work(&view, &mut refused).and_then(|_| run(&view, &mut refused));
            assert!(
                matches!(result, Err(FnxExecutionError::Cancelled(observed)) if observed == at)
            );
            assert_eq!(refused.batches, complete.batches[..at]);
            assert!(refused.stopped);
        }
    }

    #[test]
    fn weak_merge_keeps_physical_runs_bounded_and_does_not_precharge_lazy_allocation() {
        use std::cell::Cell;
        struct Meter<'a> {
            visited: &'a Cell<usize>,
            last_poll: usize,
            charged: usize,
        }
        impl FnxMeter<()> for Meter<'_> {
            fn unit(&mut self) -> Result<(), ()> {
                self.units(1)
            }
            fn poll(&mut self) -> Result<(), ()> {
                assert!(self.visited.get() - self.last_poll <= FNX_METER_RUN);
                self.last_poll = self.visited.get();
                Ok(())
            }
            fn units(&mut self, count: usize) -> Result<(), ()> {
                assert!((1..=FNX_METER_RUN).contains(&count));
                assert!(self.visited.get() - self.last_poll <= FNX_METER_RUN);
                self.last_poll = self.visited.get();
                self.charged += count;
                Ok(())
            }
        }
        // The first 63 pairs are one-way. Pair 64 is reciprocal and crosses
        // the first raw-incidence charge run, exercising its missing face.
        let arcs: Vec<_> = (1..=130)
            .map(|target| (0, target))
            .chain((64..=130).map(|source| (source, 0)))
            .collect();
        let view = graph(131, &arcs, true);
        let visited = Cell::new(0);
        let mut meter = Meter {
            visited: &visited,
            last_poll: 0,
            charged: 0,
        };
        weak_neighbors(&view, 0, &mut meter, true, |_, multiplicity| {
            visited.set(visited.get() + usize::from(multiplicity));
            Ok(true)
        })
        .unwrap();
        assert_eq!(visited.get(), arcs.len());
        assert_eq!(meter.charged, visited.get());
        assert!(visited.get() - meter.last_poll <= FNX_METER_RUN);

        visited.set(0);
        meter.last_poll = 0;
        meter.charged = 0;
        let result = weak_neighbors(&view, 0, &mut meter, false, |target, multiplicity| {
            visited.set(visited.get() + usize::from(multiplicity));
            if target == 80 {
                // The same fallible visitor boundary as the first lazy
                // forward-CSR allocation; no future neighbor may be charged.
                Err(FnxExecutionError::AllocationFailed)
            } else {
                Ok(false)
            }
        });
        assert!(matches!(result, Err(FnxExecutionError::AllocationFailed)));
        assert_eq!(meter.charged, visited.get());
        assert_eq!(visited.get(), 63 + 2 * 17);
    }

    #[test]
    fn all_three_node_graphs_with_loops_keep_directed_ratio_bits() {
        let possible: Vec<_> = (0..3).flat_map(|a| (0..3).map(move |b| (a, b))).collect();
        for mask in 0usize..1usize << possible.len() {
            let arcs: Vec<_> = possible
                .iter()
                .enumerate()
                .filter_map(|(index, &arc)| ((mask >> index) & 1 != 0).then_some(arc))
                .collect();
            let view = graph(4, &arcs, true);
            let (output, _) = run(&view, &mut || Ok::<_, ()>(())).unwrap();
            let KernelValues::Scores(actual) = output.values else {
                panic!("scores")
            };
            assert_eq!(
                actual.iter().map(|x| x.to_bits()).collect::<Vec<_>>(),
                oracle(4, &arcs)
                    .0
                    .iter()
                    .map(|x| x.to_bits())
                    .collect::<Vec<_>>(),
                "loop mask={mask}"
            );
        }
    }

    #[test]
    fn reciprocal_graphs_equal_undirected_clustering_without_multiplying_multiplicities() {
        let pairs = [(0, 1), (0, 2), (1, 2), (0, 3), (2, 3), (3, 4), (4, 5)];
        let arcs: Vec<_> = pairs
            .iter()
            .flat_map(|&(a, b)| [(a, b), (b, a)])
            .chain([(0, 0), (5, 5)])
            .collect();
        let directed = graph(7, &arcs, true);
        let undirected = graph(7, &arcs, false);
        let (actual, _) = run(&directed, &mut || Ok::<_, ()>(())).unwrap();
        let KernelValues::Scores(actual) = actual.values else {
            panic!("scores")
        };
        let expected =
            crate::clustering::triangle_statistics(&undirected, limits(), || Ok::<_, ()>(()))
                .unwrap();
        assert_eq!(expected.triangles.iter().sum::<u64>(), 6);
        assert_eq!(
            actual.iter().map(|x| x.to_bits()).collect::<Vec<_>>(),
            expected
                .clustering
                .iter()
                .map(|x| x.to_bits())
                .collect::<Vec<_>>()
        );
        assert!(actual.iter().all(|score| (0.0..=1.0).contains(score)));
    }

    #[test]
    fn high_degree_stars_have_linear_admission_and_no_forward_edge_workspace() {
        let n = 4096;
        let arcs: Vec<_> = (1..n)
            .map(|target| (0, target))
            .chain((1..n).filter(|v| v % 3 == 0).map(|source| (source, 0)))
            .chain([(0, 0), (n - 1, n - 1)])
            .collect();
        let view = graph(n, &arcs, true);
        let (output, workspace) = run(&view, &mut || Ok::<_, ()>(())).unwrap();
        let KernelValues::Scores(scores) = output.values else {
            panic!("scores")
        };
        assert!(scores.iter().all(|score| score.to_bits() == 0));
        assert_eq!(output.witness.nodes_touched, 3 * n);
        assert_eq!(output.witness.queue_peak, 0);
        assert!(output.witness.edges_scanned <= 4 * arcs.len());
        assert!(estimated_work(&view, &mut || Ok::<_, ()>(())).unwrap() < 40 * n);
        // No eligible pair exists. Only degree/offset/count vectors coexist,
        // or degree/count/score during output. try_reserve_exact may retain
        // more than requested, so compare the reported capacity-backed bytes
        // with the actual sizes of vectors reserved by this allocator.
        let mut requested: Vec<usize> = Vec::new();
        requested.try_reserve_exact(n).unwrap();
        let mut offsets: Vec<usize> = Vec::new();
        offsets.try_reserve_exact(n + 1).unwrap();
        let count_phase = (2 * requested.capacity() + offsets.capacity()) * size_of::<usize>();
        let mut scores: Vec<f64> = Vec::new();
        scores.try_reserve_exact(n).unwrap();
        let conversion =
            2 * requested.capacity() * size_of::<usize>() + scores.capacity() * size_of::<f64>();
        assert_eq!(workspace, count_phase.max(conversion));
    }

    /// Paired maintenance measurement: both actual kernels run in every
    /// invocation and consume every typed result cell. The first series
    /// includes projection creation; the second reuses one admitted immutable
    /// projection. Both include their actual work-admission pass, scratch
    /// construction, exact score conversion,
    /// typed row allocation and canonical result hashing. This is not durable
    /// database/server latency or a GAPBS competitor result.
    ///
    /// Optional real input: FGDB_LCC_EDGE_LIST=/path/graph.el and
    /// FGDB_LCC_VERTICES=N (explicitly retaining isolates). The complete input
    /// is used without truncating vertices or arcs. Run release/optimized with
    /// --ignored --nocapture --test-threads=1 and retain every printed sample.
    #[test]
    #[ignore = "paired performance measurement; run optimized and retain all samples"]
    fn oriented_directed_lcc_paired_incumbent_measurement() {
        use std::io::BufRead;
        use std::time::Instant;

        fn consumed(
            view: &SnapshotGraphView,
            previous: bool,
        ) -> (fgdb_crypto::Digest, usize, usize) {
            let estimate = if previous {
                incumbent_estimated_work(view, &mut || Ok::<_, ()>(()))
            } else {
                estimated_work(view, &mut || Ok::<_, ()>(()))
            }
            .unwrap();
            crate::execute::admit::<()>("estimated work", estimate, usize::MAX).unwrap();
            let (output, workspace) = if previous {
                incumbent(view, &mut || Ok::<_, ()>(()))
            } else {
                run(view, &mut || Ok::<_, ()>(()))
            }
            .unwrap();
            let KernelValues::Scores(scores) = output.values else {
                panic!("scores")
            };
            let call = FnxCallSpec::directed_clustering_coefficient();
            let rows: Vec<Vec<FnxValue>> = scores
                .into_iter()
                .enumerate()
                .map(|(index, score)| vec![FnxValue::Vertex(vertex(index)), FnxValue::Score(score)])
                .collect();
            let mut hash = fgdb_crypto::Hasher::new();
            hash.update(b"fgdb:prism:result-rows:v3");
            hash.update(&(rows.len() as u128).to_le_bytes());
            hash.update(&call.digest().0);
            for row in &rows {
                for &value in row {
                    crate::execute::hash_result_value(&mut hash, value);
                }
            }
            std::hint::black_box(&rows);
            (hash.finalize(), workspace, output.witness.edges_scanned)
        }

        let mut fixtures: Vec<(String, usize, Vec<(usize, usize)>)> = Vec::new();
        let n = 4096;
        fixtures.push((
            "hub_with_neighbor_cycle".to_owned(),
            n,
            (1..n)
                .map(|target| (0, target))
                .chain((1..n).map(|source| (source, 0)))
                .chain((1..n).map(|source| (source, if source + 1 == n { 1 } else { source + 1 })))
                .collect(),
        ));
        let width = 64;
        let mut roads = Vec::new();
        for source in 0..width * width {
            if source % width + 1 < width {
                roads.extend([(source, source + 1), (source + 1, source)]);
            }
            if source + width < width * width {
                roads.push((source, source + width));
            }
        }
        fixtures.push(("road_grid".to_owned(), width * width, roads));
        let n = 2048;
        let mut communities = Vec::new();
        for source in 0..n {
            let first = source / 32 * 32;
            for offset in 1..=8 {
                let target = first + (source - first + offset) % 32;
                communities.push((source, target));
                if (source + offset) % 3 == 0 {
                    communities.push((target, source));
                }
            }
            communities.extend([(source, source / 128 * 128), (source, (source + 31) % n)]);
        }
        fixtures.push(("communities_with_hubs".to_owned(), n, communities));
        if let Ok(path) = std::env::var("FGDB_LCC_EDGE_LIST") {
            let vertices: usize = std::env::var("FGDB_LCC_VERTICES")
                .expect("real input requires explicit FGDB_LCC_VERTICES")
                .parse()
                .unwrap();
            let mut arcs = Vec::new();
            for line in std::io::BufReader::new(std::fs::File::open(&path).unwrap()).lines() {
                let line = line.unwrap();
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') || line.starts_with('%') {
                    continue;
                }
                let cells: Vec<_> = line.split_whitespace().collect();
                assert_eq!(cells.len(), 2, "edge list must have exactly two columns");
                let source: usize = cells[0].parse().unwrap();
                let target: usize = cells[1].parse().unwrap();
                assert!(source < vertices && target < vertices);
                arcs.push((source, target));
            }
            fixtures.push((format!("edge_list:{path}"), vertices, arcs));
        }
        for (name, n, arcs) in fixtures {
            let reused = graph(n, &arcs, true);
            for include_projection in [true, false] {
                for trial in 0..7 {
                    let mut pair = [None, None];
                    for previous in [trial % 2 == 0, trial % 2 != 0] {
                        let start = Instant::now();
                        let (digest, workspace, scans) = if include_projection {
                            let freshly_built = graph(n, &arcs, true);
                            consumed(&freshly_built, previous)
                        } else {
                            consumed(&reused, previous)
                        };
                        let elapsed = start.elapsed().as_nanos();
                        pair[usize::from(previous)] = Some(digest);
                        eprintln!(
                            "lcc_pair fixture={name} vertices={n} input_arcs={} include_projection={include_projection} trial={trial} kernel={} elapsed_ns={elapsed} workspace_bytes={workspace} scanned={scans} digest={digest:?}",
                            arcs.len(),
                            if previous { "incumbent" } else { "oriented" }
                        );
                    }
                    assert_eq!(pair[0], pair[1], "{name} trial={trial}");
                }
            }
        }
    }
}
