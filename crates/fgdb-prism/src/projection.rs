use fgdb_crypto::{Digest, Hasher};
use fgdb_types::ids::ObjectId;
use fgdb_types::{CommitSeq, EId, VId};
use fnx_algorithms::GraphView;
use std::mem::size_of;
use std::sync::Arc;

/// An internal, synthetic attribute: a binder resolves the source property
/// before projection. Raw property names never reach an fnx algorithm.
pub const PROJECTED_WEIGHT_ATTRIBUTE: &str = "__fgdb_prism_weight";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdapterPath {
    DecodedCache,
}
impl AdapterPath {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DecodedCache => "DECODED_CACHE",
        }
    }
}

/// Root identity plus the exact historical cut. The root alone is insufficient:
/// one retained generation can answer several different historical cuts.
/// This value does not grant authority or authenticate caller-provided rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SnapshotBinding {
    pub root: ObjectId,
    pub as_of: CommitSeq,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Directedness {
    Directed = 0,
    Reversed = 1,
    /// Both original orientations enter the same explicitly reduced edge group.
    Undirected = 2,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum ParallelEdgePolicy {
    Reject = 0,
    /// Discard multiplicity and weights explicitly, retaining a unit edge.
    CollapseUnit = 1,
    Minimum = 2,
    Maximum = 3,
    /// Add in ascending EId order, refusing non-finite intermediate sums.
    Sum = 4,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum SelfLoopPolicy {
    Keep = 0,
    Drop = 1,
    Reject = 2,
}

/// No Default: the caller must choose all three graph-reduction laws.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProjectionSpec {
    pub directedness: Directedness,
    pub parallel_edges: ParallelEdgePolicy,
    pub self_loops: SelfLoopPolicy,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ProjectionEdge {
    pub eid: EId,
    pub source: VId,
    pub target: VId,
    /// Already resolved according to the caller's missing/coercion policy.
    pub weight: f64,
}

/// Deterministic in-core admission. `max_workspace_bytes` charges a conservative
/// peak bound for this builder's vector backing stores and construction scratch.
/// Allocator metadata/rounding, caller-owned input and fnx working memory are
/// not part of that charge. Allocation failure is separately fallible.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProjectionLimits {
    pub max_vertices: usize,
    pub max_input_edges: usize,
    pub max_adjacency_entries: usize,
    pub max_workspace_bytes: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProjectionError {
    LimitExceeded {
        resource: &'static str,
        limit: usize,
        observed: usize,
    },
    SizeOverflow,
    AllocationFailed,
    DuplicateVertex(VId),
    DuplicateEdge(EId),
    MissingEndpoint { edge: EId, vertex: VId },
    SelfLoop(EId),
    ParallelEdge { source: VId, target: VId },
    NonFiniteWeight(EId),
    WeightOverflow { source: VId, target: VId },
}
impl core::fmt::Display for ProjectionError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::LimitExceeded { resource, limit, observed } => {
                write!(f, "Prism {resource} limit exceeded: {observed} > {limit}")
            }
            Self::SizeOverflow => f.write_str("Prism projection size overflow"),
            Self::AllocationFailed => f.write_str("Prism projection allocation failed"),
            Self::DuplicateVertex(_) => f.write_str("duplicate vertex identity in projection input"),
            Self::DuplicateEdge(_) => f.write_str("duplicate edge identity in projection input"),
            Self::MissingEndpoint { .. } => f.write_str("projection edge endpoint is absent"),
            Self::SelfLoop(_) => f.write_str("self-loop rejected by the projection policy"),
            Self::ParallelEdge { .. } => f.write_str("parallel edges require an explicit reduction"),
            Self::NonFiniteWeight(_) => f.write_str("projection weight must be finite"),
            Self::WeightOverflow { .. } => f.write_str("parallel-edge weight reduction overflow"),
        }
    }
}
impl core::error::Error for ProjectionError {}

struct Rows {
    offsets: Vec<usize>,
    neighbors: Vec<usize>,
}
impl Rows {
    fn row(&self, node: usize) -> Option<&[usize]> {
        let end = node.checked_add(1)?;
        let start = *self.offsets.get(node)?;
        let end = *self.offsets.get(end)?;
        self.neighbors.get(start..end)
    }
}

struct Projection {
    binding: SnapshotBinding,
    spec: ProjectionSpec,
    digest: Digest,
    vertices: Vec<VId>,
    // ASCII hex, fixed width and therefore lexical order == VId order. No
    // per-node String allocations or hash table are needed for name lookup.
    names: Vec<[u8; 32]>,
    outgoing: Rows,
    incoming: Option<Rows>,
    weights: Vec<f64>,
    edges: usize,
    input_edges: usize,
    workspace_bytes: usize,
}

/// Immutable, clone-shared, snapshot-local fnx adjacency. No mutable adjacency
/// or ordinal map can escape and invalidate fnx's borrowed-row contract.
#[derive(Clone)]
pub struct SnapshotGraphView(Arc<Projection>);
impl core::fmt::Debug for SnapshotGraphView {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SnapshotGraphView")
            .field("adapter", &AdapterPath::DecodedCache)
            .finish_non_exhaustive()
    }
}

fn limit(resource: &'static str, observed: usize, maximum: usize) -> Result<(), ProjectionError> {
    if observed > maximum {
        Err(ProjectionError::LimitExceeded { resource, limit: maximum, observed })
    } else {
        Ok(())
    }
}
fn add(a: usize, b: usize) -> Result<usize, ProjectionError> {
    a.checked_add(b).ok_or(ProjectionError::SizeOverflow)
}
fn mul(a: usize, b: usize) -> Result<usize, ProjectionError> {
    a.checked_mul(b).ok_or(ProjectionError::SizeOverflow)
}
fn reserved<T>(len: usize) -> Result<Vec<T>, ProjectionError> {
    let mut result = Vec::new();
    result.try_reserve_exact(len).map_err(|_| ProjectionError::AllocationFailed)?;
    Ok(result)
}
fn filled<T: Clone>(len: usize, value: T) -> Result<Vec<T>, ProjectionError> {
    let mut result = reserved(len)?;
    result.resize(len, value);
    Ok(result)
}
fn clone_slice<T: Clone>(values: &[T]) -> Result<Vec<T>, ProjectionError> {
    let mut result = reserved(values.len())?;
    result.extend_from_slice(values);
    Ok(result)
}
fn offsets(counts: &[usize]) -> Result<Vec<usize>, ProjectionError> {
    let mut result = reserved(add(counts.len(), 1)?)?;
    result.push(0);
    for &count in counts {
        result.push(add(*result.last().expect("initial zero"), count)?);
    }
    Ok(result)
}
fn name(vid: VId) -> [u8; 32] {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut result = [0; 32];
    for (i, byte) in vid.0.to_be_bytes().iter().copied().enumerate() {
        result[2 * i] = HEX[usize::from(byte >> 4)];
        result[2 * i + 1] = HEX[usize::from(byte & 15)];
    }
    result
}
fn canonical_weight(weight: f64) -> f64 {
    if weight == 0.0 { 0.0 } else { weight }
}
fn ordinal(vertices: &[VId], edge: EId, vertex: VId) -> Result<usize, ProjectionError> {
    vertices.binary_search(&vertex).map_err(|_| ProjectionError::MissingEndpoint { edge, vertex })
}

impl SnapshotGraphView {
    /// Build from one already-selected snapshot. Input order is immaterial;
    /// VId order fixes ordinals, EId order fixes floating-point reductions.
    /// Input scratch is admitted before copying; reduced adjacency is admitted
    /// before allocating the cache. No partially built cache can escape.
    pub fn build(
        binding: SnapshotBinding,
        vertices: &[VId],
        edges: &[ProjectionEdge],
        spec: ProjectionSpec,
        limits: ProjectionLimits,
    ) -> Result<Self, ProjectionError> {
        let n = vertices.len();
        let e = edges.len();
        limit("vertices", n, limits.max_vertices)?;
        limit("input edges", e, limits.max_input_edges)?;
        let input_bytes = add(mul(n, size_of::<VId>())?, mul(e, size_of::<ProjectionEdge>())?)?;
        limit("workspace bytes", input_bytes, limits.max_workspace_bytes)?;

        let mut vertices = clone_slice(vertices)?;
        vertices.sort_unstable();
        for pair in vertices.windows(2) {
            if pair[0] == pair[1] {
                return Err(ProjectionError::DuplicateVertex(pair[0]));
            }
        }
        let mut work = clone_slice(edges)?;
        work.sort_unstable_by_key(|edge| edge.eid);
        for pair in work.windows(2) {
            if pair[0].eid == pair[1].eid {
                return Err(ProjectionError::DuplicateEdge(pair[0].eid));
            }
        }
        for edge in &mut work {
            ordinal(&vertices, edge.eid, edge.source)?;
            ordinal(&vertices, edge.eid, edge.target)?;
            if edge.source == edge.target {
                match spec.self_loops {
                    SelfLoopPolicy::Reject => return Err(ProjectionError::SelfLoop(edge.eid)),
                    SelfLoopPolicy::Drop => continue,
                    SelfLoopPolicy::Keep => {}
                }
            }
            if spec.parallel_edges == ParallelEdgePolicy::CollapseUnit {
                // This policy explicitly discards weights, even for singleton
                // groups. Do not make observation of a discarded value matter.
                edge.weight = 1.0;
            } else if !edge.weight.is_finite() {
                return Err(ProjectionError::NonFiniteWeight(edge.eid));
            }
            edge.weight = canonical_weight(edge.weight);
            match spec.directedness {
                Directedness::Reversed => std::mem::swap(&mut edge.source, &mut edge.target),
                Directedness::Undirected if edge.target < edge.source => {
                    std::mem::swap(&mut edge.source, &mut edge.target);
                }
                _ => {}
            }
        }
        if spec.self_loops == SelfLoopPolicy::Drop {
            work.retain(|edge| edge.source != edge.target);
        }

        // Hash input identity as well as the output topology. Two different
        // parallel-edge populations must not share an evidence binding merely
        // because their aggregate weight happens to coincide.
        let mut hash = Hasher::new();
        hash.update(b"fgdb:prism:decoded-projection:v1");
        hash.update(&binding.root.0);
        hash.update(&binding.as_of.0.to_le_bytes());
        hash.update(&[spec.directedness as u8, spec.parallel_edges as u8, spec.self_loops as u8]);
        hash.update(&(u64::try_from(n).map_err(|_| ProjectionError::SizeOverflow)?).to_le_bytes());
        for vertex in &vertices {
            hash.update(&vertex.0.to_le_bytes());
        }
        hash.update(&(u64::try_from(e).map_err(|_| ProjectionError::SizeOverflow)?).to_le_bytes());
        hash.update(&(u64::try_from(work.len()).map_err(|_| ProjectionError::SizeOverflow)?).to_le_bytes());
        for edge in &work {
            hash.update(&edge.eid.0.to_le_bytes());
            hash.update(&edge.source.0.to_le_bytes());
            hash.update(&edge.target.0.to_le_bytes());
            hash.update(&edge.weight.to_bits().to_le_bytes());
        }

        work.sort_unstable_by_key(|edge| (edge.source, edge.target, edge.eid));
        let mut kept = 0;
        for i in 0..work.len() {
            let edge = work[i];
            if kept != 0 && work[kept - 1].source == edge.source && work[kept - 1].target == edge.target {
                let previous = &mut work[kept - 1];
                previous.weight = match spec.parallel_edges {
                    ParallelEdgePolicy::Reject => return Err(ProjectionError::ParallelEdge { source: edge.source, target: edge.target }),
                    ParallelEdgePolicy::CollapseUnit => 1.0,
                    ParallelEdgePolicy::Minimum => previous.weight.min(edge.weight),
                    ParallelEdgePolicy::Maximum => previous.weight.max(edge.weight),
                    ParallelEdgePolicy::Sum => previous.weight + edge.weight,
                };
                if !previous.weight.is_finite() {
                    return Err(ProjectionError::WeightOverflow { source: edge.source, target: edge.target });
                }
                previous.weight = canonical_weight(previous.weight);
            } else {
                work[kept] = edge;
                kept += 1;
            }
        }
        work.truncate(kept);

        let directed = spec.directedness != Directedness::Undirected;
        let loops = work.iter().filter(|edge| edge.source == edge.target).count();
        let arcs = if directed { kept } else { add(kept, kept - loops)? };
        limit("adjacency entries", arcs, limits.max_adjacency_entries)?;
        let faces = if directed { 2 } else { 1 };
        let mut bytes = add(input_bytes, mul(n, size_of::<[u8; 32]>())?)?;
        // Degree arrays, row offsets and construction cursors for each face.
        bytes = add(bytes, mul(mul(add(mul(n, 3)?, 1)?, faces)?, size_of::<usize>())?)?;
        bytes = add(bytes, mul(mul(arcs, faces)?, size_of::<usize>())?)?;
        bytes = add(bytes, mul(arcs, size_of::<f64>())?)?;
        limit("workspace bytes", bytes, limits.max_workspace_bytes)?;

        let mut out_counts = filled(n, 0usize)?;
        let mut in_counts = if directed { filled(n, 0usize)? } else { Vec::new() };
        for edge in &work {
            let s = ordinal(&vertices, edge.eid, edge.source)?;
            let t = ordinal(&vertices, edge.eid, edge.target)?;
            out_counts[s] = add(out_counts[s], 1)?;
            if directed {
                in_counts[t] = add(in_counts[t], 1)?;
            } else if s != t {
                out_counts[t] = add(out_counts[t], 1)?;
            }
        }
        let out_offsets = offsets(&out_counts)?;
        let out_len = *out_offsets.last().expect("initial zero");
        let mut out_next = clone_slice(&out_offsets[..n])?;
        let mut out_nodes = filled(out_len, 0usize)?;
        let mut weights = filled(out_len, 0.0)?;
        let in_offsets = if directed { offsets(&in_counts)? } else { Vec::new() };
        let mut in_next = if directed { clone_slice(&in_offsets[..n])? } else { Vec::new() };
        let mut in_nodes = if directed { filled(kept, 0usize)? } else { Vec::new() };
        for edge in &work {
            let s = ordinal(&vertices, edge.eid, edge.source)?;
            let t = ordinal(&vertices, edge.eid, edge.target)?;
            let slot = out_next[s];
            out_nodes[slot] = t;
            weights[slot] = edge.weight;
            out_next[s] += 1;
            if directed {
                in_nodes[in_next[t]] = s;
                in_next[t] += 1;
            } else if s != t {
                let slot = out_next[t];
                out_nodes[slot] = s;
                weights[slot] = edge.weight;
                out_next[t] += 1;
            }
        }
        // Sorted normalized pairs and source-order scatter produce sorted
        // rows on BOTH faces. Binary edge lookup therefore needs no hash map.
        let mut names = reserved(n)?;
        names.extend(vertices.iter().copied().map(name));
        let incoming = directed.then_some(Rows { offsets: in_offsets, neighbors: in_nodes });
        Ok(Self(Arc::new(Projection {
            binding,
            spec,
            digest: hash.finalize(),
            vertices,
            names,
            outgoing: Rows { offsets: out_offsets, neighbors: out_nodes },
            incoming,
            weights,
            edges: kept,
            input_edges: e,
            workspace_bytes: bytes,
        })))
    }

    pub fn binding(&self) -> SnapshotBinding { self.0.binding }
    pub fn spec(&self) -> ProjectionSpec { self.0.spec }
    pub fn digest(&self) -> Digest { self.0.digest }
    pub fn adapter_path(&self) -> AdapterPath { AdapterPath::DecodedCache }
    pub fn vertex_ids(&self) -> &[VId] { &self.0.vertices }
    pub fn vertex_id(&self, ordinal: usize) -> Option<VId> { self.0.vertices.get(ordinal).copied() }
    pub fn vertex_ordinal(&self, vertex: VId) -> Option<usize> { self.0.vertices.binary_search(&vertex).ok() }
    pub fn input_edge_count(&self) -> usize { self.0.input_edges }
    pub fn charged_workspace_bytes(&self) -> usize { self.0.workspace_bytes }
    pub fn shares_cache_with(&self, other: &Self) -> bool { Arc::ptr_eq(&self.0, &other.0) }

    /// Borrow one canonical outgoing row and its position-aligned weights.
    /// The two slices have equal length and share the immutable cache lifetime;
    /// no per-edge lookup, allocation, string conversion or adjacency copy is
    /// needed. This borrows DECODED_CACHE, not compressed Strata storage.
    pub fn projected_row(&self, source: usize) -> Option<(&[usize], &[f64])> {
        let end = source.checked_add(1)?;
        let start = *self.0.outgoing.offsets.get(source)?;
        let end = *self.0.outgoing.offsets.get(end)?;
        Some((self.0.outgoing.neighbors.get(start..end)?, self.0.weights.get(start..end)?))
    }

    pub fn projected_weight(&self, source: usize, target: usize) -> Option<f64> {
        let row = self.0.outgoing.row(source)?;
        let index = row.binary_search(&target).ok()?;
        self.0.weights.get(self.0.outgoing.offsets[source] + index).copied()
    }
}

impl GraphView for SnapshotGraphView {
    fn nodes_ordered(&self) -> Vec<&str> {
        self.0.names.iter().map(|bytes| std::str::from_utf8(bytes).expect("hex name is ASCII")).collect()
    }
    fn get_node_index(&self, node: &str) -> Option<usize> {
        // Reject aliases (uppercase, short spellings, signs) so fnx strings have
        // exactly one inverse into the stable-identity domain.
        if node.len() != 32 || !node.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
            return None;
        }
        self.vertex_ordinal(VId(u128::from_str_radix(node, 16).ok()?))
    }
    fn get_node_name(&self, index: usize) -> Option<&str> {
        std::str::from_utf8(self.0.names.get(index)?).ok()
    }
    fn neighbors_indices(&self, node: usize) -> Option<&[usize]> { self.0.outgoing.row(node) }
    fn in_neighbors_indices(&self, node: usize) -> Option<&[usize]> {
        self.0.incoming.as_ref().unwrap_or(&self.0.outgoing).row(node)
    }
    fn neighbors_iter(&self, node: &str) -> Option<Box<dyn Iterator<Item = &str> + '_>> {
        let row = self.neighbors_indices(self.get_node_index(node)?)?;
        Some(Box::new(row.iter().map(move |&i| self.get_node_name(i).expect("validated ordinal"))))
    }
    fn in_neighbors_iter(&self, node: &str) -> Option<Box<dyn Iterator<Item = &str> + '_>> {
        let row = self.in_neighbors_indices(self.get_node_index(node)?)?;
        Some(Box::new(row.iter().map(move |&i| self.get_node_name(i).expect("validated ordinal"))))
    }
    fn neighbor_count(&self, node: &str) -> usize {
        self.get_node_index(node).and_then(|i| self.neighbors_indices(i)).map_or(0, <[usize]>::len)
    }
    fn edge_weight(&self, source: &str, target: &str, attr: Option<&str>) -> f64 {
        match (self.get_node_index(source), self.get_node_index(target)) {
            (Some(s), Some(t)) => self.edge_weight_by_indices(s, t, attr),
            _ => 1.0,
        }
    }
    fn edge_weight_by_indices(&self, source: usize, target: usize, attr: Option<&str>) -> f64 {
        if attr == Some(PROJECTED_WEIGHT_ATTRIBUTE) {
            self.projected_weight(source, target).unwrap_or(1.0)
        } else {
            // Match upstream's unweighted/missing-attribute law. Public Prism
            // call binding only passes the registered synthetic attribute.
            1.0
        }
    }
    fn has_node(&self, node: &str) -> bool { self.get_node_index(node).is_some() }
    fn has_edge(&self, source: &str, target: &str) -> bool {
        match (self.get_node_index(source), self.get_node_index(target)) {
            (Some(s), Some(t)) => self.projected_weight(s, t).is_some(),
            _ => false,
        }
    }
    fn is_directed(&self) -> bool { self.0.spec.directedness != Directedness::Undirected }
    fn node_count(&self) -> usize { self.0.vertices.len() }
    fn edge_count(&self) -> usize { self.0.edges }
}
