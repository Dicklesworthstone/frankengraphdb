//! Exact graph resilience over an admitted simple undirected projection.
//!
//! One iterative lowlink state machine serves decoded and compressed drivers.
//! It never owns adjacency. The drivers retain row positions/cursors and keep
//! every pull fallible and checkpointed. Bridges are sorted by their canonical
//! endpoint ordinals in linear time, reusing the completed DFS arrays for two
//! stable counting passes and an in-place permutation.

use crate::execute::{KernelOutput, KernelValues, admit, reserve};
use crate::{ComplexityWitness, FnxExecutionError, FnxMeter, GraphView, SnapshotGraphView};
use std::mem::size_of;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Step {
    Scalar,
    Open(usize),
    Row(usize),
    Complete,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Initialize,
    Roots,
    Open,
    Row,
    Sort { stage: u8, index: usize, sum: usize },
    Complete,
}

pub(crate) struct Cuts {
    discovery: Vec<usize>,
    low: Vec<usize>,
    parents: Vec<usize>,
    selected: Vec<bool>,
    pairs: Vec<(usize, usize)>,
    bridges: bool,
    nodes: usize,
    next_root: usize,
    root: usize,
    root_children: usize,
    current: usize,
    depth: usize,
    rows: usize,
    row_limit: usize,
    workspace: usize,
    phase: Phase,
    witness: ComplexityWitness,
}

fn reserved<T, C>(
    n: usize,
    workspace: &mut usize,
    limit: usize,
) -> Result<Vec<T>, FnxExecutionError<C>> {
    let bytes = n
        .checked_mul(size_of::<T>())
        .and_then(|n| workspace.checked_add(n))
        .ok_or(FnxExecutionError::SizeOverflow)?;
    admit("kernel workspace bytes", bytes, limit)?;
    let values = reserve(n)?;
    let actual = values
        .capacity()
        .checked_mul(size_of::<T>())
        .and_then(|n| workspace.checked_add(n))
        .ok_or(FnxExecutionError::SizeOverflow)?;
    admit("kernel workspace bytes", actual, limit)?;
    *workspace = actual;
    Ok(values)
}

impl Cuts {
    pub(crate) fn workspace(nodes: usize, bridges: bool) -> Option<usize> {
        let arrays = nodes.checked_mul(3 * size_of::<usize>())?;
        let output = if bridges {
            nodes
                .saturating_sub(1)
                .checked_mul(size_of::<(usize, usize)>())?
        } else {
            nodes.checked_mul(size_of::<bool>())?
        };
        arrays.checked_add(output)
    }

    pub(crate) fn estimated_work(nodes: usize, arcs: usize, columns: usize) -> Option<usize> {
        // Initialization, roots, DFS transitions, two stable counting passes,
        // permutation, and output. Each arc is examined exactly once.
        nodes
            .checked_mul(48usize.checked_add(columns)?)?
            .checked_add(arcs.checked_mul(2)?)?
            .checked_add(32)
    }

    pub(crate) fn new<C>(
        nodes: usize,
        bridges: bool,
        limit: usize,
        row_limit: usize,
    ) -> Result<Self, FnxExecutionError<C>> {
        admit(
            "kernel workspace bytes",
            Self::workspace(nodes, bridges).ok_or(FnxExecutionError::SizeOverflow)?,
            limit,
        )?;
        let mut workspace = 0;
        let discovery = reserved(nodes, &mut workspace, limit)?;
        let low = reserved(nodes, &mut workspace, limit)?;
        let parents = reserved(nodes, &mut workspace, limit)?;
        let selected = reserved(if bridges { 0 } else { nodes }, &mut workspace, limit)?;
        let pairs = reserved(
            if bridges { nodes.saturating_sub(1) } else { 0 },
            &mut workspace,
            limit,
        )?;
        Ok(Self {
            discovery,
            low,
            parents,
            selected,
            pairs,
            bridges,
            nodes,
            next_root: 0,
            root: 0,
            root_children: 0,
            current: 0,
            depth: 0,
            rows: 0,
            row_limit,
            workspace,
            phase: Phase::Initialize,
            witness: ComplexityWitness {
                algorithm: if bridges {
                    "bridges_iterative_lowlink"
                } else {
                    "articulation_points_iterative_lowlink"
                }
                .to_owned(),
                complexity_claim:
                    "O(|V| + |E|) time and O(|V|) integer workspace; no adjacency copy".to_owned(),
                nodes_touched: 0,
                edges_scanned: 0,
                queue_peak: 0,
            },
        })
    }

    pub(crate) fn workspace_bytes(&self) -> usize {
        self.workspace
    }
    pub(crate) fn row_count(&self) -> usize {
        self.rows
    }

    pub(crate) fn step<C>(&mut self) -> Result<Step, FnxExecutionError<C>> {
        match self.phase {
            Phase::Initialize => {
                if self.discovery.len() == self.nodes {
                    self.phase = Phase::Roots;
                } else {
                    self.discovery.push(usize::MAX);
                    self.low.push(0);
                    self.parents.push(usize::MAX);
                    if !self.bridges {
                        self.selected.push(false);
                    }
                }
                Ok(Step::Scalar)
            }
            Phase::Roots => {
                if self.next_root == self.nodes {
                    self.phase = if self.bridges && self.pairs.len() > 1 {
                        Phase::Sort {
                            stage: 0,
                            index: 0,
                            sum: 0,
                        }
                    } else {
                        Phase::Complete
                    };
                } else {
                    let root = self.next_root;
                    self.next_root += 1;
                    if self.discovery[root] == usize::MAX {
                        self.root = root;
                        self.root_children = 0;
                        self.enter(root)?;
                    }
                }
                Ok(Step::Scalar)
            }
            Phase::Open => {
                self.phase = Phase::Row;
                Ok(Step::Open(self.current))
            }
            Phase::Row => Ok(Step::Row(self.current)),
            Phase::Sort { stage, index, sum } => {
                self.sort_step(stage, index, sum)?;
                Ok(Step::Scalar)
            }
            Phase::Complete => Ok(Step::Complete),
        }
    }

    fn enter<C>(&mut self, vertex: usize) -> Result<(), FnxExecutionError<C>> {
        let ordinal = self.witness.nodes_touched;
        self.witness.nodes_touched = ordinal
            .checked_add(1)
            .ok_or(FnxExecutionError::SizeOverflow)?;
        self.discovery[vertex] = ordinal;
        self.low[vertex] = ordinal;
        self.current = vertex;
        self.depth = self
            .depth
            .checked_add(1)
            .ok_or(FnxExecutionError::SizeOverflow)?;
        self.witness.queue_peak = self.witness.queue_peak.max(self.depth);
        self.phase = Phase::Open;
        Ok(())
    }

    pub(crate) fn visit<C>(&mut self, neighbor: usize) -> Result<(), FnxExecutionError<C>> {
        if self.phase != Phase::Row || neighbor >= self.nodes {
            return Err(FnxExecutionError::InvalidUpstreamResult);
        }
        self.witness.edges_scanned = self
            .witness
            .edges_scanned
            .checked_add(1)
            .ok_or(FnxExecutionError::SizeOverflow)?;
        let vertex = self.current;
        // Loops cannot connect distinct vertices and do not change lowlinks.
        if neighbor == vertex {
            return Ok(());
        }
        if self.discovery[neighbor] == usize::MAX {
            self.parents[neighbor] = vertex;
            if vertex == self.root {
                self.root_children += 1;
            }
            self.enter(neighbor)?;
        } else if neighbor != self.parents[vertex] {
            self.low[vertex] = self.low[vertex].min(self.discovery[neighbor]);
        }
        Ok(())
    }

    fn select<C>(&mut self, vertex: usize) -> Result<(), FnxExecutionError<C>> {
        if !self.bridges && !self.selected[vertex] {
            let rows = self
                .rows
                .checked_add(1)
                .ok_or(FnxExecutionError::SizeOverflow)?;
            admit("result rows", rows, self.row_limit)?;
            self.selected[vertex] = true;
            self.rows = rows;
        }
        Ok(())
    }

    pub(crate) fn end_row<C>(&mut self) -> Result<(), FnxExecutionError<C>> {
        if self.phase != Phase::Row || self.depth == 0 {
            return Err(FnxExecutionError::InvalidUpstreamResult);
        }
        let vertex = self.current;
        let parent = self.parents[vertex];
        if parent == usize::MAX {
            if self.root_children > 1 {
                self.select(vertex)?;
            }
            self.phase = Phase::Roots;
        } else {
            let low = self.low[vertex];
            self.low[parent] = self.low[parent].min(low);
            if parent != self.root && low >= self.discovery[parent] {
                self.select(parent)?;
            }
            if self.bridges && low > self.discovery[parent] {
                let rows = self
                    .rows
                    .checked_add(1)
                    .ok_or(FnxExecutionError::SizeOverflow)?;
                admit("result rows", rows, self.row_limit)?;
                if rows >= self.nodes || rows > self.pairs.capacity() {
                    return Err(FnxExecutionError::InvalidUpstreamResult);
                }
                self.pairs.push((parent.min(vertex), parent.max(vertex)));
                self.rows = rows;
            }
            self.current = parent;
        }
        self.depth -= 1;
        Ok(())
    }

    fn sort_step<C>(
        &mut self,
        stage: u8,
        index: usize,
        sum: usize,
    ) -> Result<(), FnxExecutionError<C>> {
        let count = self.pairs.len();
        let limit = if matches!(stage, 0 | 2 | 4 | 6) {
            self.nodes
        } else {
            count
        };
        if index == limit {
            self.phase = if stage == 9 {
                Phase::Complete
            } else {
                Phase::Sort {
                    stage: stage + 1,
                    index: 0,
                    sum: 0,
                }
            };
            return Ok(());
        }
        let mut next_sum = sum;
        let mut next_index = index + 1;
        match stage {
            0 | 4 => self.low[index] = 0,
            1 => self.low[self.pairs[index].1] += 1,
            2 | 6 => {
                next_sum = sum
                    .checked_add(self.low[index])
                    .ok_or(FnxExecutionError::SizeOverflow)?;
                self.low[index] = sum;
            }
            3 => {
                let key = self.pairs[index].1;
                self.parents[self.low[key]] = index;
                self.low[key] += 1;
            }
            5 => self.low[self.pairs[self.parents[index]].0] += 1,
            7 => {
                let original = self.parents[index];
                let key = self.pairs[original].0;
                self.discovery[self.low[key]] = original;
                self.low[key] += 1;
            }
            8 => self.parents[self.discovery[index]] = index,
            9 => {
                let target = self.parents[index];
                if target != index {
                    self.pairs.swap(index, target);
                    self.parents.swap(index, target);
                    next_index = index;
                }
            }
            _ => return Err(FnxExecutionError::InvalidUpstreamResult),
        }
        self.phase = Phase::Sort {
            stage,
            index: next_index,
            sum: next_sum,
        };
        Ok(())
    }

    pub(crate) fn finish<C>(self) -> Result<KernelOutput, FnxExecutionError<C>> {
        if self.phase != Phase::Complete
            || self.depth != 0
            || self.witness.nodes_touched != self.nodes
        {
            return Err(FnxExecutionError::InvalidUpstreamResult);
        }
        Ok(KernelOutput {
            values: if self.bridges {
                KernelValues::EndpointPairs(self.pairs)
            } else {
                KernelValues::SelectedVertices(self.selected)
            },
            row_count: self.rows,
            witness: self.witness,
        })
    }
}

pub(crate) fn run<C>(
    graph: &SnapshotGraphView,
    bridges: bool,
    row_limit: usize,
    checkpoint: &mut impl FnxMeter<C>,
) -> Result<(KernelOutput, usize), FnxExecutionError<C>> {
    checkpoint.unit().map_err(FnxExecutionError::Cancelled)?;
    let nodes = graph.node_count();
    let mut state = Cuts::new(nodes, bridges, usize::MAX, row_limit)?;
    let mut workspace = state.workspace_bytes();
    // Decoded slices resume by integer position. Compressed execution instead
    // keeps only the active DFS depth's authenticated row cursors.
    let mut positions = reserved(nodes, &mut workspace, usize::MAX)?;
    for _ in 0..nodes {
        checkpoint.unit().map_err(FnxExecutionError::Cancelled)?;
        positions.push(0usize);
    }
    loop {
        checkpoint.unit().map_err(FnxExecutionError::Cancelled)?;
        match state.step()? {
            Step::Scalar | Step::Open(_) => {}
            Step::Complete => break,
            Step::Row(vertex) => {
                let row = graph
                    .neighbors_indices(vertex)
                    .ok_or(FnxExecutionError::InvalidUpstreamResult)?;
                if let Some(&neighbor) = row.get(positions[vertex]) {
                    positions[vertex] += 1;
                    state.visit(neighbor)?;
                } else {
                    state.end_row()?;
                }
            }
        }
    }
    Ok((state.finish()?, workspace))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drive(
        rows: &[Vec<usize>],
        bridges: bool,
        row_limit: usize,
    ) -> Result<KernelOutput, FnxExecutionError<()>> {
        let mut state = Cuts::new(rows.len(), bridges, usize::MAX, row_limit)?;
        let mut positions = vec![0usize; rows.len()];
        let mut operations = 0usize;
        loop {
            operations += 1;
            match state.step()? {
                Step::Scalar | Step::Open(_) => {}
                Step::Complete => break,
                Step::Row(vertex) => {
                    if let Some(&target) = rows[vertex].get(positions[vertex]) {
                        positions[vertex] += 1;
                        state.visit(target)?;
                    } else {
                        state.end_row()?;
                    }
                }
            }
        }
        let arcs = rows.iter().map(Vec::len).sum();
        assert!(operations <= Cuts::estimated_work(rows.len(), arcs, 2).unwrap());
        let output = state.finish()?;
        assert_eq!(output.witness.nodes_touched, rows.len());
        assert_eq!(output.witness.edges_scanned, arcs);
        Ok(output)
    }

    fn components(
        rows: &[Vec<usize>],
        removed_vertex: Option<usize>,
        removed_edge: Option<(usize, usize)>,
    ) -> usize {
        let mut seen = vec![false; rows.len()];
        let mut components = 0;
        for root in 0..rows.len() {
            if Some(root) == removed_vertex || seen[root] {
                continue;
            }
            components += 1;
            seen[root] = true;
            let mut queue = vec![root];
            let mut head = 0;
            while head < queue.len() {
                let vertex = queue[head];
                head += 1;
                for &target in &rows[vertex] {
                    if Some(target) == removed_vertex
                        || Some((vertex.min(target), vertex.max(target))) == removed_edge
                        || seen[target]
                    {
                        continue;
                    }
                    seen[target] = true;
                    queue.push(target);
                }
            }
        }
        components
    }

    fn check(rows: &[Vec<usize>]) {
        let baseline = components(rows, None, None);
        let expected_vertices: Vec<_> = (0..rows.len())
            .map(|vertex| components(rows, Some(vertex), None) > baseline)
            .collect();
        let mut expected_pairs = Vec::new();
        for (vertex, row) in rows.iter().enumerate() {
            for &neighbor in row {
                if vertex < neighbor && components(rows, None, Some((vertex, neighbor))) > baseline
                {
                    expected_pairs.push((vertex, neighbor));
                }
            }
        }
        expected_pairs.sort_unstable();
        let vertices = drive(rows, false, usize::MAX).unwrap();
        let pairs = drive(rows, true, usize::MAX).unwrap();
        let KernelValues::SelectedVertices(actual_vertices) = vertices.values else {
            panic!("cut vertices");
        };
        let KernelValues::EndpointPairs(actual_pairs) = pairs.values else {
            panic!("bridge pairs");
        };
        assert_eq!(actual_vertices, expected_vertices, "rows={rows:?}");
        assert_eq!(actual_pairs, expected_pairs, "rows={rows:?}");
        assert_eq!(
            vertices.row_count,
            expected_vertices
                .iter()
                .filter(|&&selected| selected)
                .count()
        );
        assert_eq!(pairs.row_count, expected_pairs.len());
    }

    #[test]
    fn all_small_graphs_match_independent_remove_one_oracle() {
        for n in 0..=5 {
            let candidates: Vec<_> = (0..n)
                .flat_map(|v| (v + 1..n).map(move |u| (v, u)))
                .collect();
            for mask in 0usize..1 << candidates.len() {
                let mut rows = vec![Vec::new(); n];
                for (bit, &(v, u)) in candidates.iter().enumerate() {
                    if mask & (1 << bit) != 0 {
                        rows[v].push(u);
                        rows[u].push(v);
                    }
                }
                check(&rows);
            }
        }
    }

    #[test]
    fn every_four_vertex_topology_and_loop_assignment_is_exact() {
        let candidates: Vec<_> = (0..4).flat_map(|v| (v..4).map(move |u| (v, u))).collect();
        for mask in 0usize..1 << candidates.len() {
            let mut rows = vec![Vec::new(); 4];
            for (bit, &(v, u)) in candidates.iter().enumerate() {
                if mask & (1 << bit) != 0 {
                    rows[v].push(u);
                    if v != u {
                        rows[u].push(v);
                    }
                }
            }
            check(&rows);
            for row in &mut rows {
                row.reverse();
            }
            check(&rows);
        }
    }

    #[test]
    fn deep_path_uses_no_rust_recursion_and_counts_actual_cutsets() {
        let n = 32768;
        let mut rows = vec![Vec::new(); n];
        for vertex in 1..n {
            rows[vertex - 1].push(vertex);
            rows[vertex].push(vertex - 1);
        }
        let result = drive(&rows, false, n - 2).unwrap();
        assert_eq!(result.row_count, n - 2);
        assert_eq!(result.witness.queue_peak, n);
        let result = drive(&rows, true, n - 1).unwrap();
        let KernelValues::EndpointPairs(pairs) = result.values else {
            panic!("pairs");
        };
        assert_eq!(
            pairs,
            (1..n)
                .map(|vertex| (vertex - 1, vertex))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn sparse_output_admission_uses_cutset_not_vertex_population() {
        let n = 32;
        let mut ring = vec![Vec::new(); n];
        let mut star = vec![Vec::new(); n];
        for vertex in 0..n {
            ring[vertex].push((vertex + 1) % n);
            ring[(vertex + 1) % n].push(vertex);
            if vertex > 0 {
                star[0].push(vertex);
                star[vertex].push(0);
            }
        }
        assert_eq!(drive(&ring, false, 0).unwrap().row_count, 0);
        assert_eq!(drive(&ring, true, 0).unwrap().row_count, 0);
        assert_eq!(drive(&star, false, 1).unwrap().row_count, 1);
        assert!(matches!(
            drive(&star, false, 0),
            Err(FnxExecutionError::LimitExceeded {
                resource: "result rows",
                requested: 1,
                limit: 0
            })
        ));
        assert!(
            matches!(drive(&star, true, n - 2), Err(FnxExecutionError::LimitExceeded { resource: "result rows", requested, limit }) if requested == n-1 && limit == n-2)
        );
    }

    #[test]
    fn canonical_pairs_survive_nonmonotone_dfs_order_and_disconnected_roots() {
        let mut rows = vec![Vec::new(); 14];
        for (u, v) in [
            (0, 8),
            (8, 3),
            (3, 9),
            (9, 2),
            (3, 7),
            (7, 4),
            (0, 5),
            (5, 6),
            (5, 1),
            (10, 12),
            (12, 11),
        ] {
            rows[u].push(v);
            rows[v].push(u);
        }
        check(&rows);
        for row in &mut rows {
            row.reverse();
        }
        check(&rows);
    }

    #[test]
    fn minimum_workspace_and_invalid_late_ordinal_refuse() {
        for bridges in [false, true] {
            let minimum = Cuts::workspace(100, bridges).unwrap();
            let state = Cuts::new::<()>(100, bridges, minimum, usize::MAX).unwrap();
            assert_eq!(state.workspace_bytes(), minimum);
            assert!(matches!(
                Cuts::new::<()>(100, bridges, minimum - 1, usize::MAX),
                Err(FnxExecutionError::LimitExceeded {
                    resource: "kernel workspace bytes",
                    ..
                })
            ));
            assert!(matches!(
                drive(&[vec![1], vec![0], vec![3]], bridges, usize::MAX),
                Err(FnxExecutionError::InvalidUpstreamResult)
            ));
        }
    }
}
