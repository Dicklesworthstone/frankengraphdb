//! Directed local clustering under the Graphalytics neighbor-union law.
//!
//! For a center v, count each directed arc among its distinct incoming or
//! outgoing neighbors, then divide by d*(d-1). Reciprocal arcs count twice;
//! reciprocal incidence at v does not duplicate a neighbor. Self-loops are
//! ignored. This is distinct from the registered undirected coefficient.
//! Definition: LDBC Graphalytics, appendix algorithms, LCC pseudocode:
//! <https://github.com/ldbc/ldbc_graphalytics_docs/blob/c0f8927d144aa0216949c444eca96f42905a576d/tex/appendix_algorithms.tex>.
//!
//! Borrow both admitted CSR faces. An ordinal stamp directory and a reused
//! neighborhood buffer avoid per-center maps, pair searches and graph copies.

use crate::execute::{KernelOutput, KernelValues, reserve};
use crate::{
    ComplexityWitness, FNX_METER_RUN, FnxExecutionError, FnxMeter, GraphView, SnapshotGraphView,
};

const COMPLEXITY: &str = "O(|V| + |E| + sum_v deg_weak(v) * deg_out(v))";

/// Bound preparation and all possible neighbor-row scans before allocating.
/// The union degree is at most in-degree plus out-degree. Summing the cost
/// of a vertex's outgoing row over all centers containing it gives this
/// bound, without materializing every center's neighborhood first.
pub(crate) fn estimated_work<C>(
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

pub(crate) fn run<C>(
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
        complexity_claim: COMPLEXITY.to_owned(),
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
                assert_eq!(output.witness.nodes_touched, n);
                assert_eq!(output.witness.edges_scanned, scanned);
                assert_eq!(output.witness.queue_peak, peak);
                let estimate = estimated_work(&view, &mut || Ok::<_, ()>(())).unwrap();
                assert!(estimate >= scanned);
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
            "fgdb-prism/directed-clustering-neighbor-union-v1"
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
        let (scalar, _) = run(&view, &mut || {
            scalar_units += 1;
            Ok::<_, usize>(())
        })
        .unwrap();
        let mut complete = Meter {
            units: 0,
            batches: Vec::new(),
            stop_at: None,
            stopped: false,
        };
        let (bulk, _) = run(&view, &mut complete).unwrap();
        assert_eq!(complete.units, scalar_units);
        let KernelValues::Scores(scalar) = scalar.values else {
            panic!("scores")
        };
        let KernelValues::Scores(bulk) = bulk.values else {
            panic!("scores")
        };
        assert_eq!(bulk, scalar);
        // Only the first center has a long incidence row. Later full runs
        // therefore prove that inner neighbor-row scans are bounded as well.
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
            assert!(
                matches!(run(&view, &mut refused), Err(FnxExecutionError::Cancelled(observed)) if observed == at)
            );
            assert_eq!(refused.batches, complete.batches[..at]);
            assert!(refused.stopped);
        }
    }
}
