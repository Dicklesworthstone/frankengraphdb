//! Actual pull scans over a pinned, already decoded database generation.
//!
//! The persistent history index supplies ordered successors without collecting
//! candidate IDs or restarting at the beginning. Only the chosen candidate's
//! MVCC history is resolved. The source holds an EmbeddedReadView, not a borrow
//! of the mutable Database: writes, compaction, reopen and handle drop cannot
//! change its generation. This is not paging an evidence artifact or materialized
//! graph table, and it does not claim that the pinned generation is out of core.

use crate::{Database, EmbeddedReadView, ReadError};
use crate::gql_exec::source::SourceEvent;
use asupersync::fs::Vfs;
use fgdb_gql::algebra::{GlaPlan, GraphValueRow, PreparedGraphPattern};
use fgdb_gql::stream::{
    VertexScanCursor, VertexScanError, VertexScanEvent, VertexScanPlan,
    VertexScanRow, VertexScanSource, VertexScanSourceError, VertexScanOutput,
};
use fgdb_gql::{GqlQueryError, GqlQueryPolicy, PreparedGqlQuery};
use fgdb_types::{CommitSeq, QueryCx, VId};

type Cancel = Box<asupersync::error::Error>;
type StreamError = GqlQueryError<VertexScanError<ReadError>, Cancel>;

/// Public only because it is a parameter of the returned public cursor. The
/// host alone constructs it from an admitted view and an exact retained cut.
/// No source mutation, raw-snapshot constructor or unchecked cursor import is
/// exposed. The caller normally names the enclosing VertexScanCursor<_, _>.
pub struct SnapshotVertexSource<'q> {
    view: EmbeddedReadView,
    cx: &'q QueryCx,
    as_of: CommitSeq,
    after: Option<VId>,
}
impl VertexScanSource for SnapshotVertexSource<'_> {
    type Error = ReadError;
    fn snapshot_seq(&self) -> CommitSeq { self.as_of }

    fn next_vertex<C>(
        &mut self,
        control: &mut impl FnMut(VertexScanEvent) -> Result<(), C>,
    ) -> Result<Option<VId>, VertexScanSourceError<ReadError, C>> {
        let cx = self.cx;
        cx.with_restriction(|| {
            let mut node = self.view.snapshot.property_index.histories.0.as_deref();
            let mut successor = None;
            // Seek strictly after the last identity. No +1 arithmetic: both
            // VId(0) and VId(u128::MAX) are ordinary legal identities.
            while let Some(current) = node {
                control(VertexScanEvent::Work).map_err(VertexScanSourceError::Control)?;
                if self.after.is_none_or(|after| current.key > after) {
                    successor = Some(current.key);
                    node = current.left.0.as_deref();
                } else {
                    node = current.right.0.as_deref();
                }
            }
            if let Some(vid) = successor { self.after = Some(vid); }
            Ok(successor)
        })
    }

    fn vertex<'a, C>(
        &'a self,
        vid: VId,
        control: &mut impl FnMut(VertexScanEvent) -> Result<(), C>,
    ) -> Result<Option<VertexScanRow<'a>>, VertexScanSourceError<ReadError, C>> {
        self.cx.with_restriction(|| {
            control(VertexScanEvent::Work).map_err(VertexScanSourceError::Control)?;
            let snapshot = &self.view.snapshot;
            snapshot.property_index.visible_row(&snapshot.patches, vid, self.as_of, &mut |event| {
                control(match event {
                    // The cursor charges one candidate history BEFORE this
                    // lookup. Version work is not a second candidate record.
                    SourceEvent::Work | SourceEvent::SnapshotRecord => VertexScanEvent::Work,
                    SourceEvent::ScratchEntry => VertexScanEvent::ScratchEntry,
                })
            }).map(|row| row.map(|row| VertexScanRow { labels: &row.labels, properties: &row.props }))
                .map_err(VertexScanSourceError::Control)
        })
    }
}
impl core::fmt::Debug for SnapshotVertexSource<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SnapshotVertexSource")
            .field("as_of", &self.as_of)
            .field("generation_and_position", &"[REDACTED]").finish()
    }
}

fn source_error(error: ReadError) -> StreamError {
    GqlQueryError::Source(VertexScanError::Source(error))
}
fn open<'q, Row: VertexScanOutput>(
    view: EmbeddedReadView,
    cx: &'q QueryCx,
    logical: &GlaPlan<Row>,
    as_of: CommitSeq,
    policy: GqlQueryPolicy,
) -> Result<
    VertexScanCursor<SnapshotVertexSource<'q>, impl FnMut() -> Result<(), Cancel> + 'q + use<'q, Row>, Row>,
    StreamError,
> {
    view.snapshot.check_frontier(as_of).map_err(source_error)?;
    let plan = VertexScanPlan::compile(logical)
        .map_err(|error| GqlQueryError::Source(VertexScanError::Plan(error)))?;
    cx.with_restriction(|| cx.checkpoint()).map_err(GqlQueryError::Interrupted)?;
    let source = SnapshotVertexSource { view, cx, as_of, after: None };
    Ok(VertexScanCursor::new(source, plan, policy, move || cx.with_restriction(|| cx.checkpoint())))
}

impl<V: Vfs + Clone> Database<V> {
    /// Pull correlated property rows in canonical order without a result set.
    /// The first projected column must be the scanned VId; remaining columns
    /// may repeat that identity or read its canonical properties. The leading
    /// unique identity proves whole-row order and DISTINCT without sorting.
    /// Other projections/orderings refuse, rather than quietly changing order.
    pub fn stream_graph_values_governed<'q>(
        &self,
        cx: &'q QueryCx,
        pattern: &PreparedGraphPattern<GraphValueRow>,
        policy: GqlQueryPolicy,
    ) -> Result<
        VertexScanCursor<SnapshotVertexSource<'q>, impl FnMut() -> Result<(), Cancel> + 'q + use<'q, V>, GraphValueRow>,
        StreamError,
    > {
        let view = self.read_session().map_err(source_error)?;
        let as_of = view.frontier();
        open(view, cx, pattern.plan(), as_of, policy)
    }

    pub fn stream_graph_values_governed_at<'q>(
        &self,
        cx: &'q QueryCx,
        pattern: &PreparedGraphPattern<GraphValueRow>,
        as_of: CommitSeq,
        policy: GqlQueryPolicy,
    ) -> Result<
        VertexScanCursor<SnapshotVertexSource<'q>, impl FnMut() -> Result<(), Cancel> + 'q + use<'q, V>, GraphValueRow>,
        StreamError,
    > {
        open(self.read_session().map_err(source_error)?, cx, pattern.plan(), as_of, policy)
    }

    /// Stream the supported single-vertex identity GLA profile. Opening pins
    /// one immutable generation but scans no candidate and builds no row set.
    /// next() is the demand signal; close/drop never drains the unused suffix.
    /// The cursor owns its read view, so the database remains writable while
    /// it is alive. It borrows only QueryCx, not this handle or the definition.
    pub fn stream_graph_vertices_governed<'q>(
        &self,
        cx: &'q QueryCx,
        pattern: &PreparedGraphPattern,
        policy: GqlQueryPolicy,
    ) -> Result<
        VertexScanCursor<SnapshotVertexSource<'q>, impl FnMut() -> Result<(), Cancel> + 'q + use<'q, V>>,
        StreamError,
    > {
        let view = self.read_session().map_err(source_error)?;
        let as_of = view.frontier();
        open(view, cx, pattern.plan(), as_of, policy)
    }

    /// Health and exact-sequence admission precede cursor preparation. A future
    /// cut never becomes a successful empty stream, including under LIMIT 0.
    pub fn stream_graph_vertices_governed_at<'q>(
        &self,
        cx: &'q QueryCx,
        pattern: &PreparedGraphPattern,
        as_of: CommitSeq,
        policy: GqlQueryPolicy,
    ) -> Result<
        VertexScanCursor<SnapshotVertexSource<'q>, impl FnMut() -> Result<(), Cancel> + 'q + use<'q, V>>,
        StreamError,
    > {
        open(self.read_session().map_err(source_error)?, cx, pattern.plan(), as_of, policy)
    }

    /// Owned legacy preparation enters the same GLA scan compiler. There is
    /// no reparse, certificate paging, full-table admission or eager fallback.
    pub fn stream_prepared_query_governed<'q>(
        &self,
        cx: &'q QueryCx,
        query: &PreparedGqlQuery,
        policy: GqlQueryPolicy,
    ) -> Result<
        VertexScanCursor<SnapshotVertexSource<'q>, impl FnMut() -> Result<(), Cancel> + 'q + use<'q, V>>,
        StreamError,
    > {
        let view = self.read_session().map_err(source_error)?;
        let as_of = view.frontier();
        open(view, cx, &GlaPlan::lower(query.plan()), as_of, policy)
    }

    pub fn stream_prepared_query_governed_at<'q>(
        &self,
        cx: &'q QueryCx,
        query: &PreparedGqlQuery,
        as_of: CommitSeq,
        policy: GqlQueryPolicy,
    ) -> Result<
        VertexScanCursor<SnapshotVertexSource<'q>, impl FnMut() -> Result<(), Cancel> + 'q + use<'q, V>>,
        StreamError,
    > {
        let view = self.read_session().map_err(source_error)?;
        view.snapshot.check_frontier(as_of).map_err(source_error)?;
        open(view, cx, &GlaPlan::lower(query.plan()), as_of, policy)
    }
}

impl EmbeddedReadView {
    pub fn stream_graph_values_governed<'q>(
        &self,
        cx: &'q QueryCx,
        pattern: &PreparedGraphPattern<GraphValueRow>,
        policy: GqlQueryPolicy,
    ) -> Result<
        VertexScanCursor<SnapshotVertexSource<'q>, impl FnMut() -> Result<(), Cancel> + 'q + use<'q>, GraphValueRow>,
        StreamError,
    > {
        open(self.clone(), cx, pattern.plan(), self.frontier(), policy)
    }

    pub fn stream_graph_values_governed_at<'q>(
        &self,
        cx: &'q QueryCx,
        pattern: &PreparedGraphPattern<GraphValueRow>,
        as_of: CommitSeq,
        policy: GqlQueryPolicy,
    ) -> Result<
        VertexScanCursor<SnapshotVertexSource<'q>, impl FnMut() -> Result<(), Cancel> + 'q + use<'q>, GraphValueRow>,
        StreamError,
    > {
        open(self.clone(), cx, pattern.plan(), as_of, policy)
    }

    pub fn stream_graph_vertices_governed<'q>(
        &self,
        cx: &'q QueryCx,
        pattern: &PreparedGraphPattern,
        policy: GqlQueryPolicy,
    ) -> Result<
        VertexScanCursor<SnapshotVertexSource<'q>, impl FnMut() -> Result<(), Cancel> + 'q + use<'q>>,
        StreamError,
    > {
        open(self.clone(), cx, pattern.plan(), self.frontier(), policy)
    }

    pub fn stream_graph_vertices_governed_at<'q>(
        &self,
        cx: &'q QueryCx,
        pattern: &PreparedGraphPattern,
        as_of: CommitSeq,
        policy: GqlQueryPolicy,
    ) -> Result<
        VertexScanCursor<SnapshotVertexSource<'q>, impl FnMut() -> Result<(), Cancel> + 'q + use<'q>>,
        StreamError,
    > {
        open(self.clone(), cx, pattern.plan(), as_of, policy)
    }

    pub fn stream_prepared_query_governed<'q>(
        &self,
        cx: &'q QueryCx,
        query: &PreparedGqlQuery,
        policy: GqlQueryPolicy,
    ) -> Result<
        VertexScanCursor<SnapshotVertexSource<'q>, impl FnMut() -> Result<(), Cancel> + 'q + use<'q>>,
        StreamError,
    > {
        open(self.clone(), cx, &GlaPlan::lower(query.plan()), self.frontier(), policy)
    }

    pub fn stream_prepared_query_governed_at<'q>(
        &self,
        cx: &'q QueryCx,
        query: &PreparedGqlQuery,
        as_of: CommitSeq,
        policy: GqlQueryPolicy,
    ) -> Result<
        VertexScanCursor<SnapshotVertexSource<'q>, impl FnMut() -> Result<(), Cancel> + 'q + use<'q>>,
        StreamError,
    > {
        self.snapshot.check_frontier(as_of).map_err(source_error)?;
        open(self.clone(), cx, &GlaPlan::lower(query.plan()), as_of, policy)
    }
}

#[cfg(test)]
mod tests;
