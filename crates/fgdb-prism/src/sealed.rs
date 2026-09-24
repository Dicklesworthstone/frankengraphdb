//! Fallible analytics rows borrowed from authenticated Strata adjacency images.
//!
//! The upstream `GraphView` slice contract cannot describe a compressed row or
//! report cancellation. This adapter deliberately does not implement it. It
//! retains a canonical vertex directory, degrees, and (when required) Strata's
//! compressed incoming locator index. No edge/property column is copied.
//! The selected vertex directory is supplied by the
//! trusted host after snapshot/label/security admission, not authorized here.

use crate::sealed_control::Control;
use crate::{
    Directedness, FnxSelection, FnxWeightError, ParallelEdgePolicy, ProjectionError,
    ProjectionLimits, ProjectionSpec, SelfLoopPolicy, SnapshotBinding,
};
use fgdb_crypto::{Digest, Hasher};
use fgdb_strata::tiered::sealed::{
    IncomingIndexLimits, IncomingIndexStats, SealedEdge, SealedError, SealedIncomingIndex,
    SealedPartition, SealedScanBudget, SealedScanStep,
};
use fgdb_types::{CommitSeq, EId, QueryCx, VId};
use std::cell::RefCell;
use std::sync::Arc;

#[path = "sealed_direction.rs"]
mod direction;
use direction::Incidences;

/// One selected relation in one already-admitted scalar snapshot. Reversed and
/// undirected projections use the source image's authenticated incoming index.
/// Undirected reciprocal incidences reduce together in canonical EId order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SealedProjectionSpec {
    pub as_of: CommitSeq,
    pub selection: FnxSelection,
    pub projection: ProjectionSpec,
}

/// Frozen visibility for the single selected relation and weight property.
/// This is restriction metadata, NOT proof of authorization. The trusted host
/// supplies admitted vertices and retains its live permit through construction
/// and execution; it must not give an untrusted caller a reusable graph handle.
/// A hidden relation has no descriptors. A hidden property resolves as absent
/// under the selected missing-weight policy, before any scalar is inspected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SealedProjectionMask {
    pub relation_visible: bool,
    pub weight_property_visible: bool,
}

impl SealedProjectionMask {
    pub const UNMASKED: Self = Self {
        relation_visible: true,
        weight_property_visible: true,
    };
}

#[derive(Debug)]
pub enum SealedProjectionError {
    /// A caller's additional live guard refused; the original cause is kept.
    Guard(Box<dyn std::error::Error + Send + Sync>),
    Read(SealedError),
    Projection(ProjectionError),
    Weight {
        edge: EId,
        reason: FnxWeightError,
    },
    RelationRequired,
    UnsupportedDirectedness(Directedness),
    NonCanonicalVertices,
    UnknownOrdinal(usize),
}

impl core::fmt::Display for SealedProjectionError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Guard(_) => f.write_str("Prism live execution guard refused"),
            Self::Read(error) => error.fmt(f),
            Self::Projection(error) => error.fmt(f),
            Self::Weight { reason, .. } => write!(f, "Prism sealed weight refused: {reason:?}"),
            Self::RelationRequired => {
                f.write_str("Prism sealed projection requires one selected relation")
            }
            Self::UnsupportedDirectedness(kind) => write!(
                f,
                "Prism sealed projection has no authenticated incoming family for {kind:?}"
            ),
            Self::NonCanonicalVertices => {
                f.write_str("Prism sealed vertex directory must be strictly ascending")
            }
            Self::UnknownOrdinal(_) => f.write_str("Prism sealed vertex ordinal is absent"),
        }
    }
}

impl core::error::Error for SealedProjectionError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Guard(error) => Some(error.as_ref()),
            Self::Read(error) => Some(error),
            Self::Projection(error) => Some(error),
            _ => None,
        }
    }
}

impl From<ProjectionError> for SealedProjectionError {
    fn from(error: ProjectionError) -> Self {
        Self::Projection(error)
    }
}

impl From<SealedError> for SealedProjectionError {
    fn from(error: SealedError) -> Self {
        Self::Read(error)
    }
}

fn checkpoint(cx: &Control<'_>) -> Result<(), SealedProjectionError> {
    cx.checkpoint()
}

fn admit(
    resource: &'static str,
    observed: usize,
    limit: usize,
) -> Result<(), SealedProjectionError> {
    if observed > limit {
        return Err(ProjectionError::LimitExceeded {
            resource,
            limit,
            observed,
        }
        .into());
    }
    Ok(())
}

fn add(left: usize, right: usize) -> Result<usize, SealedProjectionError> {
    left.checked_add(right)
        .ok_or_else(|| ProjectionError::SizeOverflow.into())
}

fn reserve<T>(length: usize) -> Result<Vec<T>, SealedProjectionError> {
    let mut values = Vec::new();
    values
        .try_reserve_exact(length)
        .map_err(|_| ProjectionError::AllocationFailed)?;
    Ok(values)
}

#[derive(Debug)]
struct Projection {
    partition: SealedPartition,
    incoming: Option<SealedIncomingIndex>,
    config: SealedProjectionSpec,
    mask: SealedProjectionMask,
    vertices: Vec<VId>,
    degrees: Vec<usize>,
    binding: SnapshotBinding,
    digest: Digest,
    input_edges: usize,
    edges: usize,
    adjacency_entries: usize,
    scan_bound: usize,
    workspace_bytes: usize,
}

/// O(1)-cloneable, immutable compressed projection. The source root is derived
/// from Strata's opaque anchor, never stamped onto caller-provided image bytes.
/// Isolates in `vertices` survive. A caller-supplied vertex list is a selection,
/// not proof of endpoint visibility or authorization: use the admitted host
/// read view to construct it. Retaining this handle pins the resident source
/// image, not a durable GC lease and not an external-memory working set.
#[derive(Clone, Debug)]
pub struct SealedGraphView(Arc<Projection>);

impl SealedGraphView {
    /// Admit all selected incidences before returning any projection. A reverse
    /// or undirected view also builds Strata's compressed incoming locator index
    /// under the remaining workspace allowance. Its construction peak, not just
    /// its retained bytes, is charged beside the vertex directory and degrees.
    /// `max_input_edges` also caps the source's retained incidence population,
    /// including invisible history and excluded endpoints, so a tiny selected
    /// result cannot conceal an unbounded historical scan.
    pub fn build(
        cx: &QueryCx,
        partition: &SealedPartition,
        vertices: &[VId],
        config: SealedProjectionSpec,
        limits: ProjectionLimits,
    ) -> Result<Self, SealedProjectionError> {
        Self::build_with_checkpoint(
            cx,
            partition,
            vertices,
            config,
            SealedProjectionMask::UNMASKED,
            limits,
            || Ok(()),
        )
    }

    /// Build the same compressed projection with a frozen source mask and an
    /// additional live guard. The guard reaches directory copying, incoming
    /// index construction/sorting, every raw pull (including hidden history),
    /// reductions and final publication. QueryCx cancellation is never replaced.
    /// No guard or permit is stored in the returned graph: authorized hosts must
    /// keep it private and use execute_sealed_with_checkpoint for every call.
    #[allow(clippy::too_many_arguments)]
    pub fn build_with_checkpoint(
        cx: &QueryCx,
        partition: &SealedPartition,
        vertices: &[VId],
        config: SealedProjectionSpec,
        mask: SealedProjectionMask,
        limits: ProjectionLimits,
        guard: impl FnMut() -> Result<(), SealedProjectionError>,
    ) -> Result<Self, SealedProjectionError> {
        let guard = RefCell::new(guard);
        let invoke = || (guard.borrow_mut())();
        let control = Control::new(cx, &invoke);
        Self::build_controlled(&control, partition, vertices, config, mask, limits)
    }

    fn build_controlled(
        cx: &Control<'_>,
        partition: &SealedPartition,
        vertices: &[VId],
        config: SealedProjectionSpec,
        mask: SealedProjectionMask,
        limits: ProjectionLimits,
    ) -> Result<Self, SealedProjectionError> {
        checkpoint(cx)?;
        if config.selection.relation.is_none() {
            return Err(SealedProjectionError::RelationRequired);
        }
        let scope = partition.anchor().scope();
        if config.as_of < scope.floor || config.as_of > scope.publication {
            return Err(SealedProjectionError::Read(
                SealedError::SnapshotOutsideAnchor {
                    requested: config.as_of,
                    floor: scope.floor,
                    publication: scope.publication,
                },
            ));
        }
        let n = vertices.len();
        admit("vertices", n, limits.max_vertices)?;
        let scan_bound = partition.stats().incidences;
        admit("source incidences", scan_bound, limits.max_input_edges)?;
        let mut workspace_bytes = n
            .checked_mul(std::mem::size_of::<VId>() + std::mem::size_of::<usize>())
            .ok_or(ProjectionError::SizeOverflow)?;
        admit(
            "workspace bytes",
            workspace_bytes,
            limits.max_workspace_bytes,
        )?;
        let mut owned_vertices = reserve(n)?;
        let mut previous = None;
        for &vertex in vertices {
            checkpoint(cx)?;
            if previous.is_some_and(|previous| previous >= vertex) {
                return Err(SealedProjectionError::NonCanonicalVertices);
            }
            owned_vertices.push(vertex);
            previous = Some(vertex);
        }
        let incoming =
            if !mask.relation_visible || config.projection.directedness == Directedness::Directed {
                None
            } else {
                let index = partition.incoming_index_with_checkpoint(
                    cx.query,
                    IncomingIndexLimits {
                        max_incidences: limits.max_input_edges,
                        // This indexes the whole admitted image, including other
                        // relations and unselected endpoints, not merely n vertices.
                        max_rows: limits.max_input_edges,
                        max_workspace_bytes: limits.max_workspace_bytes - workspace_bytes,
                    },
                    || cx.guard(),
                )?;
                workspace_bytes = add(workspace_bytes, index.stats().charged_workspace_bytes)?;
                admit(
                    "workspace bytes",
                    workspace_bytes,
                    limits.max_workspace_bytes,
                )?;
                Some(index)
            };
        let binding = SnapshotBinding {
            root: scope.source_root.0,
            as_of: config.as_of,
        };
        let mut hash = Hasher::new();
        hash.update(b"fgdb:prism:sealed-projection:v2");
        hash.update(&binding.root.0);
        hash.update(&binding.as_of.0.to_le_bytes());
        hash.update(&config.selection.digest().0);
        hash.update(&[
            config.projection.directedness as u8,
            config.projection.parallel_edges as u8,
            config.projection.self_loops as u8,
            u8::from(mask.relation_visible),
            u8::from(mask.weight_property_visible),
        ]);
        hash.update(&(n as u128).to_le_bytes());
        for vertex in &owned_vertices {
            checkpoint(cx)?;
            hash.update(&vertex.0.to_le_bytes());
        }
        let mut degrees = reserve(n)?;
        let mut edges = 0usize;
        let mut adjacency_entries = 0usize;
        let mut input_edges = 0usize;
        for source in 0..n {
            checkpoint(cx)?;
            hash.update(&[0]);
            hash.update(&owned_vertices[source].0.to_le_bytes());
            let mut cursor = SealedNeighborCursor::open(
                cx,
                partition,
                incoming.as_ref(),
                &owned_vertices,
                &config,
                mask,
                source,
                None,
            )?;
            let mut degree = 0usize;
            {
                // Raw EIDs participate even when their weights collapse to the
                // same aggregate. Dropped loops are tagged without reading a
                // property that the reduction policy explicitly discards.
                let mut observe = |eid: EId, target: VId, weight: Option<f64>| {
                    // Each undirected original appears at two endpoints, except
                    // a loop. Count it once, but bind both ordered row views.
                    if config.projection.directedness != Directedness::Undirected
                        || owned_vertices[source] <= target
                    {
                        input_edges = add(input_edges, 1)?;
                        admit("input edges", input_edges, limits.max_input_edges)?;
                    }
                    hash.update(&[1]);
                    hash.update(&eid.0.to_le_bytes());
                    hash.update(&target.0.to_le_bytes());
                    match weight {
                        Some(weight) => {
                            hash.update(&[1]);
                            hash.update(&weight.to_bits().to_le_bytes());
                        }
                        None => {
                            hash.update(&[0]);
                        }
                    }
                    Ok(())
                };
                while let Some((target, _)) = cursor.next_observed(cx, &mut observe)? {
                    degree = add(degree, 1)?;
                    adjacency_entries = add(adjacency_entries, 1)?;
                    admit(
                        "adjacency entries",
                        adjacency_entries,
                        limits.max_adjacency_entries,
                    )?;
                    if config.projection.directedness != Directedness::Undirected
                        || source <= target
                    {
                        edges = add(edges, 1)?;
                    }
                }
            }
            hash.update(&[2]);
            hash.update(&(degree as u128).to_le_bytes());
            degrees.push(degree);
        }
        hash.update(&(input_edges as u128).to_le_bytes());
        hash.update(&(edges as u128).to_le_bytes());
        checkpoint(cx)?;
        Ok(Self(Arc::new(Projection {
            partition: partition.clone(),
            incoming,
            config,
            mask,
            vertices: owned_vertices,
            degrees,
            binding,
            digest: hash.finalize(),
            input_edges,
            edges,
            adjacency_entries,
            scan_bound,
            workspace_bytes,
        })))
    }

    pub fn binding(&self) -> SnapshotBinding {
        self.0.binding
    }
    pub fn spec(&self) -> ProjectionSpec {
        self.0.config.projection
    }
    pub fn selection(&self) -> FnxSelection {
        self.0.config.selection
    }
    pub fn digest(&self) -> Digest {
        self.0.digest
    }
    pub fn node_count(&self) -> usize {
        self.0.vertices.len()
    }
    pub fn edge_count(&self) -> usize {
        self.0.edges
    }
    /// Outgoing row entries actually visited by a kernel. An undirected
    /// non-loop has two entries but contributes only one logical edge.
    pub fn adjacency_entry_count(&self) -> usize {
        self.0.adjacency_entries
    }
    /// The exact source index retained by reversed/undirected projections.
    /// Includes construction-peak evidence; not an allocator/RSS guarantee.
    pub fn incoming_index_stats(&self) -> Option<IncomingIndexStats> {
        self.0.incoming.as_ref().map(SealedIncomingIndex::stats)
    }
    pub fn input_edge_count(&self) -> usize {
        self.0.input_edges
    }
    pub fn vertex_ids(&self) -> &[VId] {
        &self.0.vertices
    }
    pub fn vertex_id(&self, ordinal: usize) -> Option<VId> {
        self.0.vertices.get(ordinal).copied()
    }
    pub fn vertex_ordinal(&self, vertex: VId) -> Option<usize> {
        self.0.vertices.binary_search(&vertex).ok()
    }
    pub fn degree(&self, ordinal: usize) -> Option<usize> {
        self.0.degrees.get(ordinal).copied()
    }
    pub fn charged_workspace_bytes(&self) -> usize {
        self.0.workspace_bytes
    }
    /// Conservative retained-incidence bound, not the reduced simple-edge count.
    pub fn scan_incidence_bound(&self) -> usize {
        self.0.scan_bound
    }
    /// Internal physical scan bound for repeat-row kernels. Includes invisible
    /// versions and excluded neighbors; never exposed as a visible degree.
    pub(crate) fn retained_row_incidence_bound(
        &self,
        ordinal: usize,
    ) -> Result<usize, SealedProjectionError> {
        let vertex = self
            .vertex_id(ordinal)
            .ok_or(SealedProjectionError::UnknownOrdinal(ordinal))?;
        let relation = self
            .0
            .config
            .selection
            .relation
            .ok_or(SealedProjectionError::RelationRequired)?;
        if !self.0.mask.relation_visible {
            return Ok(0);
        }
        let direction = self.0.config.projection.directedness;
        let outgoing = if direction == Directedness::Reversed {
            0
        } else {
            self.0.partition.retained_row_len(vertex, relation)
        };
        let incoming = if direction == Directedness::Directed {
            0
        } else {
            self.0
                .incoming
                .as_ref()
                .ok_or(SealedProjectionError::UnsupportedDirectedness(direction))?
                .retained_row_len(vertex, relation)
        };
        add(outgoing, incoming)
    }

    pub fn shares_storage_with(&self, partition: &SealedPartition) -> bool {
        self.0.partition.shares_image_with(partition)
    }

    pub fn neighbor_cursor(
        &self,
        cx: &QueryCx,
        source: usize,
    ) -> Result<SealedNeighborCursor<'_>, SealedProjectionError> {
        self.neighbor_cursor_from(cx, source, None)
    }

    /// Seek in Strata's EF column by a stable identity, not by a lossy cast of
    /// that identity into a machine-sized or storage-local ordinal.
    pub fn neighbor_cursor_from(
        &self,
        cx: &QueryCx,
        source: usize,
        lower_bound: Option<VId>,
    ) -> Result<SealedNeighborCursor<'_>, SealedProjectionError> {
        self.neighbor_cursor_controlled(&Control::new(cx, &|| Ok(())), source, lower_bound)
    }

    pub(crate) fn neighbor_cursor_controlled(
        &self,
        cx: &Control<'_>,
        source: usize,
        lower_bound: Option<VId>,
    ) -> Result<SealedNeighborCursor<'_>, SealedProjectionError> {
        SealedNeighborCursor::open(
            cx,
            &self.0.partition,
            self.0.incoming.as_ref(),
            &self.0.vertices,
            &self.0.config,
            self.0.mask,
            source,
            lower_bound,
        )
    }
}

/// Up to two compressed incidence cursors and constant-sized lookahead. `next` is
/// fallible even though construction admitted the image: cancellation must not
/// be confused with EOF. After an error this cursor cannot resume a prefix.
pub struct SealedNeighborCursor<'a> {
    raw: Incidences<'a>,
    vertices: &'a [VId],
    config: &'a SealedProjectionSpec,
    mask: SealedProjectionMask,
    source: VId,
    pending: Option<(usize, f64)>,
    accumulating: Option<(usize, f64)>,
    finished: bool,
}

impl<'a> SealedNeighborCursor<'a> {
    #[allow(clippy::too_many_arguments)]
    fn open(
        cx: &Control<'_>,
        partition: &'a SealedPartition,
        incoming: Option<&'a SealedIncomingIndex>,
        vertices: &'a [VId],
        config: &'a SealedProjectionSpec,
        mask: SealedProjectionMask,
        source: usize,
        lower_bound: Option<VId>,
    ) -> Result<Self, SealedProjectionError> {
        checkpoint(cx)?;
        let source = vertices
            .get(source)
            .copied()
            .ok_or(SealedProjectionError::UnknownOrdinal(source))?;
        let raw = Incidences::open(
            cx,
            partition,
            incoming,
            source,
            config,
            mask.relation_visible,
            lower_bound,
        )?;
        Ok(Self {
            raw,
            vertices,
            config,
            mask,
            source,
            pending: None,
            accumulating: None,
            finished: false,
        })
    }

    pub fn next(&mut self, cx: &QueryCx) -> Result<Option<(usize, f64)>, SealedProjectionError> {
        self.next_controlled(&Control::new(cx, &|| Ok(())))
    }

    pub(crate) fn next_controlled(
        &mut self,
        cx: &Control<'_>,
    ) -> Result<Option<(usize, f64)>, SealedProjectionError> {
        self.next_observed(cx, &mut |_, _, _| Ok(()))
    }

    fn next_observed(
        &mut self,
        cx: &Control<'_>,
        observe: &mut impl FnMut(EId, VId, Option<f64>) -> Result<(), SealedProjectionError>,
    ) -> Result<Option<(usize, f64)>, SealedProjectionError> {
        loop {
            match self.next_budgeted_observed(
                cx,
                &mut SealedScanBudget::new(usize::MAX),
                observe,
            )? {
                SealedScanStep::Item(value) => return Ok(Some(value)),
                SealedScanStep::End => return Ok(None),
                SealedScanStep::Yield => {}
            }
        }
    }

    /// Pull a COMPLETE reduced neighbor under shared raw-scan fuel. Invisible
    /// versions, excluded endpoints, discarded loops and parallel-edge groups
    /// can all yield without losing their precise cursor/reduction positions.
    /// Partial sums never escape as neighbors. Mixing this method with `next`
    /// preserves order and bits; neither method reopens or rescans a prefix.
    /// Yield itself does not schedule a task: the owning driver must do that.
    pub fn next_budgeted(
        &mut self,
        cx: &QueryCx,
        budget: &mut SealedScanBudget,
    ) -> Result<SealedScanStep<(usize, f64)>, SealedProjectionError> {
        self.next_budgeted_controlled(&Control::new(cx, &|| Ok(())), budget)
    }

    pub(crate) fn next_budgeted_controlled(
        &mut self,
        cx: &Control<'_>,
        budget: &mut SealedScanBudget,
    ) -> Result<SealedScanStep<(usize, f64)>, SealedProjectionError> {
        self.next_budgeted_observed(cx, budget, &mut |_, _, _| Ok(()))
    }

    fn next_budgeted_observed(
        &mut self,
        cx: &Control<'_>,
        budget: &mut SealedScanBudget,
        observe: &mut impl FnMut(EId, VId, Option<f64>) -> Result<(), SealedProjectionError>,
    ) -> Result<SealedScanStep<(usize, f64)>, SealedProjectionError> {
        if self.finished {
            return Ok(SealedScanStep::End);
        }
        let result = self.next_inner(cx, budget, observe);
        if result.is_err() || matches!(&result, Ok(SealedScanStep::End)) {
            self.finished = true;
            self.pending = None;
            self.accumulating = None;
        }
        result
    }

    fn next_inner(
        &mut self,
        cx: &Control<'_>,
        budget: &mut SealedScanBudget,
        observe: &mut impl FnMut(EId, VId, Option<f64>) -> Result<(), SealedProjectionError>,
    ) -> Result<SealedScanStep<(usize, f64)>, SealedProjectionError> {
        checkpoint(cx)?;
        if budget.remaining() == 0 {
            return Ok(SealedScanStep::Yield);
        }
        if self.accumulating.is_none() {
            let first = match self.pending.take() {
                Some(value) => SealedScanStep::Item(value),
                None => self.read_selected(cx, budget, observe)?,
            };
            match first {
                SealedScanStep::Item(value) => self.accumulating = Some(value),
                other => return Ok(other),
            }
        }
        loop {
            let next = self.read_selected(cx, budget, observe)?;
            let (next_target, next_weight) = match next {
                SealedScanStep::Yield => return Ok(SealedScanStep::Yield),
                SealedScanStep::End => {
                    return Ok(SealedScanStep::Item(
                        self.accumulating.take().expect("started a neighbor group"),
                    ));
                }
                SealedScanStep::Item(value) => value,
            };
            let (target, weight) = self.accumulating.expect("started a neighbor group");
            if target != next_target {
                self.pending = Some((next_target, next_weight));
                return Ok(SealedScanStep::Item(
                    self.accumulating.take().expect("started a neighbor group"),
                ));
            }
            let weight = reduce_weight(
                self.config.projection.parallel_edges,
                self.source,
                self.vertices[target],
                weight,
                next_weight,
            )?;
            self.accumulating = Some((target, weight));
        }
    }

    fn read_selected(
        &mut self,
        cx: &Control<'_>,
        budget: &mut SealedScanBudget,
        observe: &mut impl FnMut(EId, VId, Option<f64>) -> Result<(), SealedProjectionError>,
    ) -> Result<SealedScanStep<(usize, f64)>, SealedProjectionError> {
        loop {
            let (neighbor, edge) = match self.raw.next_budgeted(cx, budget)? {
                SealedScanStep::Item(value) => value,
                SealedScanStep::Yield => return Ok(SealedScanStep::Yield),
                SealedScanStep::End => return Ok(SealedScanStep::End),
            };
            // The source was admitted before opening this descriptor. Mask an
            // excluded endpoint BEFORE resolving or inspecting its properties.
            let Ok(target) = self.vertices.binary_search(&neighbor) else {
                continue;
            };
            let weight = selected_weight(self.mask.weight_property_visible, self.config, &edge)?;
            observe(edge.entry.eid, neighbor, weight)?;
            if let Some(weight) = weight {
                return Ok(SealedScanStep::Item((target, weight)));
            }
        }
    }
}

fn selected_weight(
    property_visible: bool,
    config: &SealedProjectionSpec,
    edge: &SealedEdge<'_>,
) -> Result<Option<f64>, SealedProjectionError> {
    if edge.entry.src == edge.entry.dst {
        match config.projection.self_loops {
            SelfLoopPolicy::Drop => return Ok(None),
            SelfLoopPolicy::Reject => return Err(ProjectionError::SelfLoop(edge.entry.eid).into()),
            SelfLoopPolicy::Keep => {}
        }
    }
    if config.projection.parallel_edges == ParallelEdgePolicy::CollapseUnit {
        return Ok(Some(1.0));
    }
    let weight_spec = config.selection.weight;
    let value = weight_spec
        .property_key()
        .filter(|_| property_visible)
        .and_then(|key| {
            edge.properties
                .binary_search_by_key(&key, |(key, _)| *key)
                .ok()
                .map(|index| &edge.properties[index].1)
        });
    weight_spec
        .resolve(value)
        .map(Some)
        .map_err(|reason| SealedProjectionError::Weight {
            edge: edge.entry.eid,
            reason,
        })
}

fn reduce_weight(
    policy: ParallelEdgePolicy,
    source: VId,
    target: VId,
    previous: f64,
    next: f64,
) -> Result<f64, ProjectionError> {
    let weight = match policy {
        ParallelEdgePolicy::Reject => return Err(ProjectionError::ParallelEdge { source, target }),
        ParallelEdgePolicy::CollapseUnit => 1.0,
        ParallelEdgePolicy::Minimum => previous.min(next),
        ParallelEdgePolicy::Maximum => previous.max(next),
        ParallelEdgePolicy::Sum => previous + next,
    };
    if !weight.is_finite() {
        return Err(ProjectionError::WeightOverflow { source, target });
    }
    Ok(if weight == 0.0 { 0.0 } else { weight })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FnxWeightSpec, MissingWeightPolicy};
    use fgdb_delta_types::{PropertyKeyId, RelationId};
    use fgdb_strata::AdjacencyEntry;
    use fgdb_types::CanonicalScalar;

    fn config() -> SealedProjectionSpec {
        SealedProjectionSpec {
            as_of: CommitSeq(3),
            selection: FnxSelection {
                vertex_label: None,
                relation: Some(RelationId(7)),
                weight: FnxWeightSpec::Property {
                    key: PropertyKeyId(2),
                    missing: MissingWeightPolicy::Reject,
                },
            },
            projection: ProjectionSpec {
                directedness: Directedness::Directed,
                parallel_edges: ParallelEdgePolicy::Sum,
                self_loops: SelfLoopPolicy::Keep,
            },
        }
    }

    fn entry(target: VId) -> AdjacencyEntry {
        AdjacencyEntry {
            src: VId(1),
            relation: RelationId(7),
            dst: target,
            eid: EId(u128::MAX),
            created_at: CommitSeq(1),
            retired_at: None,
        }
    }

    #[test]
    fn selected_properties_use_exact_numeric_resolution() {
        let properties = [(PropertyKeyId(2), CanonicalScalar::Int(17))];
        let edge = SealedEdge {
            entry: entry(VId(u128::MAX)),
            properties: &properties,
        };
        assert_eq!(selected_weight(true, &config(), &edge).unwrap(), Some(17.0));
        let properties = [(PropertyKeyId(2), CanonicalScalar::Int(i64::MAX))];
        let edge = SealedEdge {
            entry: entry(VId(2)),
            properties: &properties,
        };
        assert!(matches!(
            selected_weight(true, &config(), &edge),
            Err(SealedProjectionError::Weight {
                reason: FnxWeightError::InexactInteger,
                ..
            })
        ));
    }

    #[test]
    fn discarded_weights_and_loops_are_not_observed() {
        let edge = SealedEdge {
            entry: entry(VId(2)),
            properties: &[],
        };
        let mut config = config();
        assert!(selected_weight(true, &config, &edge).is_err());
        config.projection.parallel_edges = ParallelEdgePolicy::CollapseUnit;
        assert_eq!(selected_weight(true, &config, &edge).unwrap(), Some(1.0));
        config.projection.parallel_edges = ParallelEdgePolicy::Sum;
        config.projection.self_loops = SelfLoopPolicy::Drop;
        let edge = SealedEdge {
            entry: entry(VId(1)),
            properties: &[],
        };
        assert_eq!(selected_weight(true, &config, &edge).unwrap(), None);
        config.projection.self_loops = SelfLoopPolicy::Reject;
        assert!(matches!(
            selected_weight(true, &config, &edge),
            Err(SealedProjectionError::Projection(
                ProjectionError::SelfLoop(_)
            ))
        ));
    }

    #[test]
    fn reductions_match_the_decoded_projection_laws() {
        let source = VId(1);
        let target = VId(u128::MAX);
        for (policy, expected) in [
            (ParallelEdgePolicy::CollapseUnit, 1.0),
            (ParallelEdgePolicy::Minimum, 2.0),
            (ParallelEdgePolicy::Maximum, 7.0),
            (ParallelEdgePolicy::Sum, 9.0),
        ] {
            assert_eq!(
                reduce_weight(policy, source, target, 7.0, 2.0).unwrap(),
                expected
            );
        }
        assert!(matches!(
            reduce_weight(ParallelEdgePolicy::Reject, source, target, 7.0, 2.0),
            Err(ProjectionError::ParallelEdge { .. })
        ));
        assert!(matches!(
            reduce_weight(ParallelEdgePolicy::Sum, source, target, f64::MAX, f64::MAX),
            Err(ProjectionError::WeightOverflow { .. })
        ));
        assert_eq!(
            reduce_weight(ParallelEdgePolicy::Sum, source, target, -0.0, -0.0)
                .unwrap()
                .to_bits(),
            0
        );
    }

    #[test]
    fn forbidden_weight_properties_are_absent_before_numeric_observation() {
        let properties = [(PropertyKeyId(2), CanonicalScalar::Bool(true))];
        let edge = SealedEdge {
            entry: entry(VId(2)),
            properties: &properties,
        };
        let mut config = config();
        assert!(matches!(
            selected_weight(true, &config, &edge),
            Err(SealedProjectionError::Weight {
                reason: FnxWeightError::NotNumeric,
                ..
            })
        ));
        assert!(matches!(
            selected_weight(false, &config, &edge),
            Err(SealedProjectionError::Weight {
                reason: FnxWeightError::Missing,
                ..
            })
        ));
        for (missing, expected) in [
            (MissingWeightPolicy::Unit, 1.0),
            (MissingWeightPolicy::Zero, 0.0),
        ] {
            config.selection.weight = FnxWeightSpec::Property {
                key: PropertyKeyId(2),
                missing,
            };
            assert_eq!(
                selected_weight(false, &config, &edge).unwrap(),
                Some(expected)
            );
        }
        config.projection.parallel_edges = ParallelEdgePolicy::CollapseUnit;
        assert_eq!(selected_weight(true, &config, &edge).unwrap(), Some(1.0));
        assert_eq!(selected_weight(false, &config, &edge).unwrap(), Some(1.0));
    }
}
