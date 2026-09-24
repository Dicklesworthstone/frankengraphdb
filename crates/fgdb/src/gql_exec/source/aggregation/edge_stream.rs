//! Pull identified edge matches from one pinned, admitted MVCC generation.
//! Successor/predecessor walks use the existing persistent history indexes;
//! opening does not collect candidate IDs, visible edges, or projected rows.

mod aggregate;
mod expansion;
pub(super) use expansion::next_from_view;

use crate::gql_exec::source::{SourceEvent, edge_properties_at};
use crate::{Database, EmbeddedReadView, ReadError};
use asupersync::fs::Vfs;
use fgdb_gql::algebra::{GlaPlan, GraphValueRow, PreparedGraphPattern};
use fgdb_gql::edge_stream::{
    EdgeScanCursor, EdgeScanError, EdgeScanPlan, EdgeScanRow, EdgeScanSource, EdgeScanSourceError,
    VertexScanRow,
};
use fgdb_gql::{GlaExecutionEvent, GqlQueryError, GqlQueryPolicy};
use fgdb_types::{CommitSeq, EId, QueryCx, VId};

type Cancel = Box<asupersync::error::Error>;
type StreamError = GqlQueryError<EdgeScanError<ReadError>, Cancel>;

/// The database alone constructs this source from a validated immutable view.
/// Public only as the parameter of the returned EdgeScanCursor; there is no
/// raw-source constructor or mutation surface. The cursor borrows QueryCx, not
/// the Database, so writes, compaction and handle drop cannot change its cut.
pub struct SnapshotEdgeSource<'q> {
    view: EmbeddedReadView,
    cx: &'q QueryCx,
    as_of: CommitSeq,
    after: Option<EId>,
}
impl EdgeScanSource for SnapshotEdgeSource<'_> {
    type Error = ReadError;
    fn snapshot_seq(&self) -> CommitSeq {
        self.as_of
    }

    fn next_edge<C>(
        &mut self,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<EId>, EdgeScanSourceError<ReadError, C>> {
        let cx = self.cx;
        cx.with_restriction(|| {
            let mut node = self.view.snapshot.adjacency_index.histories.0.as_deref();
            let mut successor = None;
            // Strict successor, not eid+1: zero and u128::MAX are ordinary IDs.
            // Each visited node is metered; no unmetered rank lookup or ID bag.
            while let Some(current) = node {
                control(GlaExecutionEvent::Work).map_err(EdgeScanSourceError::Control)?;
                if self.after.is_none_or(|after| current.key > after) {
                    successor = Some(current.key);
                    node = current.left.0.as_deref();
                } else {
                    node = current.right.0.as_deref();
                }
            }
            if let Some(eid) = successor {
                self.after = Some(eid);
            }
            Ok(successor)
        })
    }

    fn next_probe_vertex<C>(
        &self,
        after: Option<VId>,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<VId>, fgdb_gql::edge_stream::EdgeExpansionSourceError<ReadError, C>> {
        super::vertex_stream::probe_vertex_from_view(&self.view, self.cx, after, control)
    }

    fn next_incident_edge<C>(
        &self,
        endpoint: VId,
        direction: fgdb_gql::algebra::GlaDirection,
        after: Option<EId>,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<EId>, fgdb_gql::edge_stream::EdgeExpansionSourceError<ReadError, C>> {
        expansion::next(self, endpoint, direction, after, control)
    }

    fn edge<'a, C>(
        &'a self,
        eid: EId,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<EdgeScanRow<'a>>, EdgeScanSourceError<ReadError, C>> {
        edge_from_view(&self.view, self.cx, self.as_of, eid, control)
    }

    fn vertex<'a, C>(
        &'a self,
        vid: VId,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<VertexScanRow<'a>>, EdgeScanSourceError<ReadError, C>> {
        self.cx.with_restriction(|| {
            let snapshot = &self.view.snapshot;
            control(GlaExecutionEvent::Work).map_err(EdgeScanSourceError::Control)?;
            snapshot
                .property_index
                .visible_row(&snapshot.patches, vid, self.as_of, &mut |event| {
                    control(match event {
                        // SnapshotRecords counts candidate EDGE histories once,
                        // not orientations, endpoint histories or their versions.
                        SourceEvent::Work | SourceEvent::SnapshotRecord => GlaExecutionEvent::Work,
                        SourceEvent::ScratchEntry => GlaExecutionEvent::ScratchEntry,
                    })
                })
                .map(|row| {
                    row.map(|row| VertexScanRow {
                        labels: &row.labels,
                        properties: &row.props,
                    })
                })
                .map_err(EdgeScanSourceError::Control)
        })
    }
}

/// Reuse the exact MVCC winner selection for all indexed stream roots.
/// The borrowed fields cannot outlive the already admitted view; this helper
/// does not construct a snapshot, grant authority or advance a source cursor.
pub(super) fn edge_from_view<'a, C>(
    view: &'a EmbeddedReadView,
    cx: &QueryCx,
    as_of: CommitSeq,
    eid: EId,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
) -> Result<Option<EdgeScanRow<'a>>, EdgeScanSourceError<ReadError, C>> {
    cx.with_restriction(|| {
        let snapshot = &view.snapshot;
        let mut node = snapshot.adjacency_index.histories.0.as_deref();
        let mut history = None;
        while let Some(current) = node {
            control(GlaExecutionEvent::Work).map_err(EdgeScanSourceError::Control)?;
            match eid.cmp(&current.key) {
                core::cmp::Ordering::Less => node = current.left.0.as_deref(),
                core::cmp::Ordering::Greater => node = current.right.0.as_deref(),
                core::cmp::Ordering::Equal => {
                    history = Some(&current.value);
                    break;
                }
            }
        }
        let Some(history) = history else {
            return Ok(None);
        };
        let mut node = history.0.as_deref();
        let mut winner = None;
        while let Some(current) = node {
            control(GlaExecutionEvent::Work).map_err(EdgeScanSourceError::Control)?;
            // History keys are (created_at, block, row). Pick the last
            // coordinate at or before the cut, INCLUDING a retirement
            // restatement. Falling back to an older live row resurrects
            // deletes and can mix topology with another version's costs.
            if current.key.0 <= as_of {
                winner = Some(current.key);
                node = current.right.0.as_deref();
            } else {
                node = current.left.0.as_deref();
            }
        }
        let Some((_, block, row)) = winner else {
            return Ok(None);
        };
        control(GlaExecutionEvent::Work).map_err(EdgeScanSourceError::Control)?;
        let entry = &snapshot.blocks[block][row];
        if !entry.visible_at(as_of) {
            return Ok(None);
        }
        Ok(Some(EdgeScanRow {
            source: entry.src,
            target: entry.dst,
            relation: entry.relation,
            properties: edge_properties_at(&snapshot.block_props, block, row),
        }))
    })
}
impl core::fmt::Debug for SnapshotEdgeSource<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SnapshotEdgeSource")
            .field("as_of", &self.as_of)
            .field("generation_and_position", &"[REDACTED]")
            .finish()
    }
}
fn source_error(error: ReadError) -> StreamError {
    GqlQueryError::Source(EdgeScanError::Source(error))
}
fn open<'q>(
    view: EmbeddedReadView,
    cx: &'q QueryCx,
    logical: &GlaPlan<GraphValueRow>,
    as_of: CommitSeq,
    policy: GqlQueryPolicy,
) -> Result<
    EdgeScanCursor<SnapshotEdgeSource<'q>, impl FnMut() -> Result<(), Cancel> + 'q + use<'q>>,
    StreamError,
> {
    view.snapshot.check_frontier(as_of).map_err(source_error)?;
    let plan = EdgeScanPlan::compile(logical)
        .map_err(|error| GqlQueryError::Source(EdgeScanError::Plan(error)))?;
    cx.with_restriction(|| cx.checkpoint())
        .map_err(GqlQueryError::Interrupted)?;
    Ok(EdgeScanCursor::new(
        SnapshotEdgeSource {
            view,
            cx,
            as_of,
            after: None,
        },
        plan,
        policy,
        move || cx.with_restriction(|| cx.checkpoint()),
    ))
}

impl<V: Vfs + Clone> Database<V> {
    /// Stream connected fixed-edge GLA matches with a complete identity prefix.
    /// The prefix is root edge, root source, then each expansion's edge in GLA
    /// order. Chains, branches and identity closures seek indexed incidence;
    /// arbitrary ordering and unsupported scopes refuse during preparation.
    /// Both vertex and edge predicates/properties use the same pinned image.
    /// Opening checks health, cut and physical shape but scans no candidate.
    /// LIMIT/close/drop provides backpressure without collecting a result bag.
    /// The pinned decoded generation remains resident: this is not out-of-core
    /// storage, FreeJoin/WCOJ, transaction streaming or a durable token.
    pub fn stream_graph_edges_governed<'q>(
        &self,
        cx: &'q QueryCx,
        pattern: &PreparedGraphPattern<GraphValueRow>,
        policy: GqlQueryPolicy,
    ) -> Result<
        EdgeScanCursor<
            SnapshotEdgeSource<'q>,
            impl FnMut() -> Result<(), Cancel> + 'q + use<'q, V>,
        >,
        StreamError,
    > {
        let view = self.read_session().map_err(source_error)?;
        let as_of = view.frontier();
        open(view, cx, pattern.plan(), as_of, policy)
    }

    /// Exact-cut admission precedes query quota/cancellation refusal, including
    /// LIMIT 0. SnapshotRecords counts examined candidate histories; unlike the
    /// eager executor it need not count the entire source before the first row.
    pub fn stream_graph_edges_governed_at<'q>(
        &self,
        cx: &'q QueryCx,
        pattern: &PreparedGraphPattern<GraphValueRow>,
        as_of: CommitSeq,
        policy: GqlQueryPolicy,
    ) -> Result<
        EdgeScanCursor<
            SnapshotEdgeSource<'q>,
            impl FnMut() -> Result<(), Cancel> + 'q + use<'q, V>,
        >,
        StreamError,
    > {
        open(
            self.read_session().map_err(source_error)?,
            cx,
            pattern.plan(),
            as_of,
            policy,
        )
    }
}
impl EmbeddedReadView {
    /// Internal source authority for an already admitted immutable generation.
    /// No index is traversed here. Restricted adapters must still enforce
    /// their own capability on every root, incidence, endpoint and field read.
    pub(crate) fn edge_scan_source<'q>(
        &self,
        cx: &'q QueryCx,
        as_of: CommitSeq,
    ) -> Result<SnapshotEdgeSource<'q>, ReadError> {
        self.snapshot.check_frontier(as_of)?;
        Ok(SnapshotEdgeSource {
            view: self.clone(),
            cx,
            as_of,
            after: None,
        })
    }

    pub fn stream_graph_edges_governed<'q>(
        &self,
        cx: &'q QueryCx,
        pattern: &PreparedGraphPattern<GraphValueRow>,
        policy: GqlQueryPolicy,
    ) -> Result<
        EdgeScanCursor<SnapshotEdgeSource<'q>, impl FnMut() -> Result<(), Cancel> + 'q + use<'q>>,
        StreamError,
    > {
        open(self.clone(), cx, pattern.plan(), self.frontier(), policy)
    }

    pub fn stream_graph_edges_governed_at<'q>(
        &self,
        cx: &'q QueryCx,
        pattern: &PreparedGraphPattern<GraphValueRow>,
        as_of: CommitSeq,
        policy: GqlQueryPolicy,
    ) -> Result<
        EdgeScanCursor<SnapshotEdgeSource<'q>, impl FnMut() -> Result<(), Cancel> + 'q + use<'q>>,
        StreamError,
    > {
        open(self.clone(), cx, pattern.plan(), as_of, policy)
    }
}

#[cfg(test)]
mod tests;
