//! Directed connectivity without incoming rows or a decoded adjacency cache.
//! WCC uses union-by-size; SCC retains one resumable cursor per active DFS
//! frame. Both publish the minimum snapshot-local ordinal of each component,
//! which the CALL boundary maps back to the full stable VId.

use super::{
    ComplexityWitness, Cursor, ExecutionError, KernelOutput, KernelValues, Result, ResultAdmission,
    Rows, SealedRow, add, mul, reserve,
};
use std::mem::size_of;

// Both scheduling modes retain this exact frame, so workspace admission uses
// the concrete compressed cursor layout rather than a second approximation.
pub(super) struct Frame<C> {
    pub(super) vertex: usize,
    pub(super) row: C,
}

pub(super) fn workspace(n: usize, strong: bool) -> Result<usize> {
    let per_vertex = if strong {
        // Discovery indices, lowlinks, component stack, labels, on-stack flags,
        // and the actual concrete Strata cursor frame (not just its ordinal).
        4 * size_of::<usize>() + size_of::<bool>() + size_of::<Frame<SealedRow<'_>>>()
    } else {
        2 * size_of::<usize>() // parents/output labels and sizes/minimum scratch
    };
    mul(n, per_vertex)
}

pub(super) fn work(n: usize, edges: usize, pass: usize, strong: bool) -> Result<usize> {
    let vertex_work = mul(n, 12)?;
    if strong {
        add(pass, vertex_work)
    } else {
        // Union-by-size bounds tree height by log2(n), even before path
        // halving. Include two finds per edge and a final find per vertex.
        let height = (usize::BITS - n.leading_zeros()) as usize;
        let finds = mul(add(n, mul(edges, 2)?)?, add(height, 1)?)?;
        add(pass, add(vertex_work, finds)?)
    }
}

fn root(
    parents: &mut [usize],
    mut vertex: usize,
    checkpoint: &mut impl FnMut() -> Result<()>,
) -> Result<usize> {
    while parents[vertex] != vertex {
        checkpoint()?;
        parents[vertex] = parents[parents[vertex]];
        vertex = parents[vertex];
    }
    Ok(vertex)
}

pub(super) fn weak(
    graph: &impl Rows,
    admission: &ResultAdmission,
    checkpoint: &mut impl FnMut() -> Result<()>,
) -> Result<KernelOutput> {
    checkpoint()?;
    let n = graph.node_count();
    admission.rows(n)?;
    let mut parents = reserve(n)?;
    let mut sizes = reserve(n)?;
    for vertex in 0..n {
        checkpoint()?;
        parents.push(vertex);
        sizes.push(1usize);
    }
    let mut witness = ComplexityWitness {
        algorithm: "weakly_connected_components_union_find".to_owned(),
        complexity_claim:
            "O(|V| log(1+H) + (H+|V|) log(1+|V|)) compressed row visits and bounded union-find"
                .to_owned(),
        nodes_touched: 0,
        edges_scanned: 0,
        queue_peak: 0,
    };
    for source in 0..n {
        checkpoint()?;
        witness.nodes_touched = add(witness.nodes_touched, 1)?;
        let mut row = graph.open(source)?;
        while let Some((target, _)) = row.next()? {
            checkpoint()?;
            if target >= n {
                return Err(ExecutionError::InvalidUpstreamResult.into());
            }
            witness.edges_scanned = add(witness.edges_scanned, 1)?;
            let mut a = root(&mut parents, source, checkpoint)?;
            let mut b = root(&mut parents, target, checkpoint)?;
            if a == b {
                continue;
            }
            if sizes[a] < sizes[b] || (sizes[a] == sizes[b] && a > b) {
                std::mem::swap(&mut a, &mut b);
            }
            sizes[a] = add(sizes[a], sizes[b])?;
            parents[b] = a;
        }
    }
    // First flatten ALL parents while they still form a forest. Reuse sizes
    // for minima only after union is complete; replacing roots with minima
    // before flattening would corrupt the forest for later finds.
    for vertex in 0..n {
        checkpoint()?;
        let representative = root(&mut parents, vertex, checkpoint)?;
        parents[vertex] = representative;
    }
    for size in &mut sizes {
        checkpoint()?;
        *size = usize::MAX;
    }
    for (vertex, &representative) in parents.iter().enumerate() {
        checkpoint()?;
        sizes[representative] = sizes[representative].min(vertex);
    }
    for representative in &mut parents {
        checkpoint()?;
        *representative = sizes[*representative];
    }
    checkpoint()?;
    Ok(KernelOutput {
        values: KernelValues::Components(parents),
        row_count: n,
        witness,
    })
}

pub(super) fn strong<G: Rows>(
    graph: &G,
    admission: &ResultAdmission,
    checkpoint: &mut impl FnMut() -> Result<()>,
) -> Result<KernelOutput> {
    checkpoint()?;
    let n = graph.node_count();
    admission.rows(n)?;
    let mut indices = reserve(n)?;
    let mut lowlinks = reserve(n)?;
    let mut on_stack = reserve(n)?;
    let mut labels = reserve(n)?;
    let mut members = reserve(n)?;
    let mut frames: Vec<Frame<G::Cursor<'_>>> = reserve(n)?;
    for _ in 0..n {
        checkpoint()?;
        indices.push(usize::MAX);
        lowlinks.push(usize::MAX);
        on_stack.push(false);
        labels.push(usize::MAX);
    }
    let mut next_index = 0usize;
    let mut witness = ComplexityWitness {
        algorithm: "strongly_connected_components_iterative_tarjan_cursor".to_owned(),
        complexity_claim: "O(|V| * (1+log(1+H)) + H log(1+|V|)) compressed row visits".to_owned(),
        // queue_peak records active resumable DFS frames, not a BFS queue.
        nodes_touched: 0,
        edges_scanned: 0,
        queue_peak: 0,
    };
    for seed in 0..n {
        checkpoint()?;
        if indices[seed] != usize::MAX {
            continue;
        }
        let row = graph.open(seed)?;
        indices[seed] = next_index;
        lowlinks[seed] = next_index;
        next_index = add(next_index, 1)?;
        on_stack[seed] = true;
        members.push(seed);
        frames.push(Frame { vertex: seed, row });
        witness.nodes_touched = add(witness.nodes_touched, 1)?;
        witness.queue_peak = witness.queue_peak.max(frames.len());
        while let Some(frame) = frames.last_mut() {
            checkpoint()?;
            let source = frame.vertex;
            match frame.row.next()? {
                Some((target, _)) => {
                    checkpoint()?;
                    let index = *indices
                        .get(target)
                        .ok_or(ExecutionError::InvalidUpstreamResult)?;
                    witness.edges_scanned = add(witness.edges_scanned, 1)?;
                    if index == usize::MAX {
                        let row = graph.open(target)?;
                        indices[target] = next_index;
                        lowlinks[target] = next_index;
                        next_index = add(next_index, 1)?;
                        on_stack[target] = true;
                        members.push(target);
                        frames.push(Frame {
                            vertex: target,
                            row,
                        });
                        witness.nodes_touched = add(witness.nodes_touched, 1)?;
                        witness.queue_peak = witness.queue_peak.max(frames.len());
                    } else if on_stack[target] {
                        lowlinks[source] = lowlinks[source].min(index);
                    }
                }
                None => {
                    frames.pop();
                    if lowlinks[source] == indices[source] {
                        // This component is a contiguous suffix. Scan it twice
                        // to find the minimum and assign labels, with no extra
                        // member vector and no uncheckpointed reverse search.
                        let mut start = members.len();
                        let mut minimum = source;
                        loop {
                            checkpoint()?;
                            start = start
                                .checked_sub(1)
                                .ok_or(ExecutionError::InvalidUpstreamResult)?;
                            let member = members[start];
                            minimum = minimum.min(member);
                            if member == source {
                                break;
                            }
                        }
                        for &member in &members[start..] {
                            checkpoint()?;
                            labels[member] = minimum;
                            on_stack[member] = false;
                        }
                        members.truncate(start);
                    }
                    if let Some(parent) = frames.last() {
                        lowlinks[parent.vertex] = lowlinks[parent.vertex].min(lowlinks[source]);
                    }
                }
            }
        }
    }
    for &label in &labels {
        checkpoint()?;
        if label == usize::MAX {
            return Err(ExecutionError::InvalidUpstreamResult.into());
        }
    }
    checkpoint()?;
    Ok(KernelOutput {
        values: KernelValues::Components(labels),
        row_count: n,
        witness,
    })
}
