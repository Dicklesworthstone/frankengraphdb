//! Connectivity without incoming rows or a decoded adjacency cache.
//! WCC uses union-by-size; SCC retains one resumable cursor per active DFS
//! frame. Both publish the minimum snapshot-local ordinal of each component,
//! which the CALL boundary maps back to the full stable VId.
//! WCC over fully admitted adjacency stops reading once unions prove the
//! entire population connected. Unknown providers are scanned completely so
//! an omitted row cannot conceal invalid ordinals or source failures. For
//! admitted symmetric CC, the degree directory proves zero-degree vertices
//! isolated, so only its nonzero-degree population must be joined. Remaining
//! immutable edges cannot change that partition; no inverse graph or sample
//! is needed to omit their compressed scans.
//! Symmetric rows also admit one growing seed component: an edge whose two
//! endpoint rows are both omitted already lies inside that component. Scan the
//! highest-degree seed first, so a late hub is not left for its leaves to join.

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

/// A sufficient, constant-time membership proof for one growing component.
/// Indirect descendants may still be scanned; no extra find, bitmap or sample
/// is needed. Root changes follow the same union-by-size decisions as parents.
#[derive(Default)]
pub(super) struct WeakSeed {
    pub(super) vertex: usize,
    degree: usize,
    representative: usize,
}

impl WeakSeed {
    pub(super) fn consider(&mut self, vertex: usize, degree: usize) {
        // Initialization visits canonical ordinals, so ties keep the minimum.
        if degree > self.degree {
            self.vertex = vertex;
            self.degree = degree;
            self.representative = vertex;
        }
    }

    pub(super) fn contains(&self, parents: &[usize], vertex: usize) -> bool {
        parents[vertex] == self.representative
    }

    pub(super) fn joined(&mut self, parent: usize, child: usize) {
        if self.representative == child {
            self.representative = parent;
        }
    }
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
    let admitted = graph.has_validated_adjacency();
    let symmetric = admitted && graph.is_undirected();
    let mut remaining = if symmetric { 0 } else { n };
    let mut seed = WeakSeed::default();
    for vertex in 0..n {
        checkpoint()?;
        if symmetric {
            let degree = graph
                .degree(vertex)
                .ok_or(ExecutionError::InvalidUpstreamResult)?;
            if degree != 0 {
                remaining = add(remaining, 1)?;
            }
            seed.consider(vertex, degree);
        }
        parents.push(vertex);
        sizes.push(1usize);
    }
    let mut witness = ComplexityWitness {
        algorithm: "weakly_connected_components_union_find_seed_component_v3".to_owned(),
        complexity_claim:
            "O(|V| log(1+H) + (H+|V|) log(1+|V|)) compressed row visits and bounded union-find"
                .to_owned(),
        nodes_touched: 0,
        edges_scanned: 0,
        queue_peak: 0,
    };
    // Remaining counts eligible components: each successful union joins two,
    // so no union can take this count below one. Symmetric
    // zero-degree vertices cannot be endpoints; directed WCC includes ALL n.
    let first = (symmetric && n != 0).then_some(seed.vertex);
    let sources = first
        .into_iter()
        .chain((0..n).filter(move |&vertex| Some(vertex) != first));
    for source in sources {
        // Finish the current row before omitting later row opens entirely.
        if admitted && remaining <= 1 {
            break;
        }
        checkpoint()?;
        // Every omitted source belongs to the same monotonically growing
        // component. An edge between distinct final components cannot have
        // BOTH endpoints omitted: the other endpoint's symmetric row is read.
        // Directed WCC has no such reverse-incidence guarantee.
        if symmetric && Some(source) != first && seed.contains(&parents, source) {
            continue;
        }
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
            seed.joined(a, b);
            remaining = remaining
                .checked_sub(1)
                .ok_or(ExecutionError::InvalidUpstreamResult)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    struct CountedRows {
        rows: Vec<Vec<usize>>,
        symmetric: bool,
        admitted: bool,
        opened: Cell<usize>,
        visited: Cell<usize>,
    }
    struct CountedRow<'a> {
        row: std::slice::Iter<'a, usize>,
        visited: &'a Cell<usize>,
    }
    impl Cursor for CountedRow<'_> {
        fn next(&mut self) -> Result<Option<(usize, f64)>> {
            Ok(self.row.next().map(|&neighbor| {
                self.visited.set(self.visited.get() + 1);
                (neighbor, -1.0)
            }))
        }
    }
    impl Rows for CountedRows {
        type Cursor<'a> = CountedRow<'a>;
        fn node_count(&self) -> usize {
            self.rows.len()
        }
        fn degree(&self, source: usize) -> Option<usize> {
            self.rows.get(source).map(Vec::len)
        }
        fn has_validated_adjacency(&self) -> bool {
            self.admitted
        }
        fn is_undirected(&self) -> bool {
            self.symmetric
        }
        fn open(&self, source: usize) -> Result<CountedRow<'_>> {
            self.opened.set(self.opened.get() + 1);
            Ok(CountedRow {
                row: self
                    .rows
                    .get(source)
                    .ok_or(ExecutionError::InvalidUpstreamResult)?
                    .iter(),
                visited: &self.visited,
            })
        }
    }
    fn counted(rows: Vec<Vec<usize>>, symmetric: bool) -> CountedRows {
        // The test provider earns the same omission preconditions as the
        // production constructor instead of treating directedness as proof.
        for (source, row) in rows.iter().enumerate() {
            for &target in row {
                assert!(target < rows.len());
                if symmetric {
                    assert_eq!(
                        row.iter().filter(|&&vertex| vertex == target).count(),
                        rows[target]
                            .iter()
                            .filter(|&&vertex| vertex == source)
                            .count()
                    );
                }
            }
        }
        CountedRows {
            rows,
            symmetric,
            admitted: true,
            opened: Cell::new(0),
            visited: Cell::new(0),
        }
    }
    fn execute(graph: &CountedRows) -> KernelOutput {
        let admission = ResultAdmission {
            row_bytes: 0,
            column_bytes: 0,
            max_rows: graph.node_count(),
            max_bytes: 0,
            delivery: super::super::Delivery::Collected,
        };
        let output = weak(graph, &admission, &mut || Ok(())).unwrap();
        assert_eq!(output.witness.nodes_touched, graph.opened.get());
        assert_eq!(output.witness.edges_scanned, graph.visited.get());
        output
    }

    #[test]
    fn complete_undirected_population_skips_remaining_rows_but_keeps_isolates() {
        let (n, active) = (129, 65);
        let rows: Vec<_> = (0..n)
            .map(|source| {
                if source < active {
                    (0..active).filter(|&target| target != source).collect()
                } else {
                    Vec::new()
                }
            })
            .collect();
        for symmetric in [false, true] {
            let graph = counted(rows.clone(), symmetric);
            let output = execute(&graph);
            let KernelValues::Components(labels) = output.values else {
                panic!("components")
            };
            let expected: Vec<_> = (0..n)
                .map(|vertex| if vertex < active { 0 } else { vertex })
                .collect();
            assert_eq!(labels, expected);
            if symmetric {
                assert_eq!(graph.opened.get(), 1);
                assert_eq!(graph.visited.get(), active - 1);
            } else {
                // Outdegree zero does not prove a directed vertex isolated.
                assert_eq!(graph.opened.get(), n);
                assert_eq!(graph.visited.get(), active * (active - 1));
            }
        }
    }

    #[test]
    fn directed_sinks_and_late_incoming_bridges_remain_in_completion_population() {
        let n = 65;
        let mut forward = vec![Vec::new(); n];
        forward[0] = (1..n).collect();
        let mut reversed = vec![Vec::new(); n];
        for row in &mut reversed[1..] {
            row.push(0);
        }
        for (rows, opened) in [(forward, 1), (reversed, n)] {
            let graph = counted(rows, false);
            let output = execute(&graph);
            let KernelValues::Components(labels) = output.values else {
                panic!("components")
            };
            assert_eq!(labels, vec![0; n]);
            assert_eq!(graph.opened.get(), opened);
            assert_eq!(graph.visited.get(), n - 1);
        }
    }

    #[test]
    fn seed_component_skips_only_proven_rows_in_disconnected_and_loop_populations() {
        let n = 65;
        let rows: Vec<_> = (0..n)
            .map(|source| {
                if source == n - 1 {
                    vec![source]
                } else {
                    (0..n - 1)
                        .filter(|&target| source / 32 == target / 32 && source != target)
                        .collect()
                }
            })
            .collect();
        let graph = counted(rows, true);
        let output = execute(&graph);
        let KernelValues::Components(labels) = output.values else {
            panic!("components")
        };
        assert_eq!(
            labels,
            (0..n)
                .map(|vertex| if vertex < 32 {
                    0
                } else if vertex < 64 {
                    32
                } else {
                    64
                })
                .collect::<Vec<_>>()
        );
        // The seed clique needs only its first row. Every row in the other
        // clique and the loop-only component remains outside the seed proof.
        assert_eq!(graph.opened.get(), 1 + 32 + 1);
        assert_eq!(graph.visited.get(), 31 + 32 * 31 + 1);
        for (rows, labels) in [
            (vec![], vec![]),
            (vec![vec![]], vec![0]),
            (vec![vec![0]], vec![0]),
        ] {
            for symmetric in [false, true] {
                let graph = counted(rows.clone(), symmetric);
                let output = execute(&graph);
                let KernelValues::Components(actual) = output.values else {
                    panic!("components")
                };
                assert_eq!(actual, labels);
                assert_eq!(graph.opened.get(), 0);
                assert_eq!(graph.visited.get(), 0);
            }
        }
    }

    fn reference(rows: &[Vec<usize>]) -> Vec<usize> {
        let mut labels = vec![usize::MAX; rows.len()];
        for source in 0..rows.len() {
            if labels[source] != usize::MAX {
                continue;
            }
            labels[source] = source;
            let mut queue = std::collections::VecDeque::from([source]);
            while let Some(vertex) = queue.pop_front() {
                for &target in &rows[vertex] {
                    if labels[target] == usize::MAX {
                        labels[target] = source;
                        queue.push_back(target);
                    }
                }
            }
        }
        labels
    }

    #[test]
    fn every_five_vertex_undirected_topology_matches_independent_reachability() {
        let n = 5;
        let pairs: Vec<_> = (0..n)
            .flat_map(|left| (left + 1..n).map(move |right| (left, right)))
            .collect();
        for mask in 0usize..1 << pairs.len() {
            for loops in [false, true] {
                let mut rows = vec![Vec::new(); n];
                for (bit, &(left, right)) in pairs.iter().enumerate() {
                    if mask & (1 << bit) != 0 {
                        rows[left].push(right);
                        rows[right].push(left);
                    }
                }
                if loops {
                    for (vertex, row) in rows.iter_mut().enumerate() {
                        row.push(vertex);
                    }
                }
                for row in &mut rows {
                    row.sort_unstable();
                }
                let expected = reference(&rows);
                let arcs: usize = rows.iter().map(Vec::len).sum();
                for admitted in [false, true] {
                    let mut graph = counted(rows.clone(), true);
                    graph.admitted = admitted;
                    let KernelValues::Components(actual) = execute(&graph).values else {
                        panic!("components")
                    };
                    assert_eq!(
                        actual, expected,
                        "mask={mask}, loops={loops}, admitted={admitted}"
                    );
                    assert!(graph.opened.get() <= n);
                    assert!(graph.visited.get() <= arcs);
                    if !admitted {
                        assert_eq!(graph.opened.get(), n);
                        assert_eq!(graph.visited.get(), arcs);
                    }
                }
            }
        }
    }

    #[test]
    fn unadmitted_rows_cannot_hide_invalid_targets_after_connectivity_is_proven() {
        for rows in [
            vec![vec![usize::MAX]],
            vec![vec![1, 2], vec![], vec![usize::MAX]],
        ] {
            for symmetric in [false, true] {
                let graph = CountedRows {
                    rows: rows.clone(),
                    symmetric,
                    admitted: false,
                    opened: Cell::new(0),
                    visited: Cell::new(0),
                };
                let admission = ResultAdmission {
                    row_bytes: 0,
                    column_bytes: 0,
                    max_rows: graph.node_count(),
                    max_bytes: 0,
                    delivery: super::super::Delivery::Collected,
                };
                assert!(matches!(
                    weak(&graph, &admission, &mut || Ok(())),
                    Err(super::super::FnxSealedExecutionError::Execution(
                        ExecutionError::InvalidUpstreamResult
                    ))
                ));
                assert_eq!(graph.opened.get(), rows.len());
            }
        }
    }

    #[test]
    fn unadmitted_symmetric_hint_does_not_hide_a_one_way_bridge() {
        let graph = CountedRows {
            rows: vec![vec![1], vec![2], vec![], vec![]],
            symmetric: true,
            admitted: false,
            opened: Cell::new(0),
            visited: Cell::new(0),
        };
        let KernelValues::Components(labels) = execute(&graph).values else {
            panic!("components")
        };
        assert_eq!(labels, vec![0, 0, 0, 3]);
        assert_eq!(graph.opened.get(), 4);
        assert_eq!(graph.visited.get(), 2);
    }

    #[test]
    fn maximum_degree_seed_avoids_leaf_scans_for_early_and_late_stars() {
        let n = 1_025;
        for center in [0, n / 2, n - 1] {
            let mut rows = vec![Vec::new(); n];
            for leaf in 0..n {
                if leaf != center {
                    rows[center].push(leaf);
                    rows[leaf].push(center);
                }
            }
            let graph = counted(rows, true);
            let KernelValues::Components(actual) = execute(&graph).values else {
                panic!("components")
            };
            assert_eq!(actual, vec![0; n]);
            assert_eq!(graph.opened.get(), 1, "center={center}");
            assert_eq!(graph.visited.get(), n - 1, "center={center}");
        }
    }

    #[test]
    fn seed_scan_and_omitted_rows_keep_every_live_cancellation_boundary() {
        let n = 10;
        let rows: Vec<Vec<_>> = (0..n)
            .map(|source| {
                (0..n)
                    .filter(|&target| source / 5 == target / 5 && source != target)
                    .collect()
            })
            .collect();
        let admission = ResultAdmission {
            row_bytes: 0,
            column_bytes: 0,
            max_rows: n,
            max_bytes: 0,
            delivery: super::super::Delivery::Collected,
        };
        let mut calls = 0;
        weak(&counted(rows.clone(), true), &admission, &mut || {
            calls += 1;
            Ok(())
        })
        .unwrap();
        for stop in 1..=calls {
            let mut observed = 0;
            let result = weak(&counted(rows.clone(), true), &admission, &mut || {
                observed += 1;
                if observed == stop {
                    Err(ExecutionError::InvalidUpstreamResult.into())
                } else {
                    Ok(())
                }
            });
            assert!(result.is_err(), "stop={stop}");
            assert_eq!(observed, stop, "no callback after the refusing boundary");
        }
    }
}
