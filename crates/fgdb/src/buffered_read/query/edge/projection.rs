//! Local ownership of computed single-edge rows and their endpoint fields.
//! The native cursor alone owns orientation, order, projection and pagination.

use super::*;
use crate::buffered_read::query::BufferedQueryRow;
use crate::buffered_read::query::projection::{
    BufferedProjectionError, BufferedProjectionOutputGuard, BufferedProjectionRow,
    reserve_evaluation, retain_evaluation,
};
use fgdb_gql::edge_stream::{AsyncEdgeProjectionCursor, AsyncEdgeProjectionPlan, EdgeScanState};
use fgdb_gql::scan_stream::ScanError;
use fgdb_gql::{GlaExecutionStats, GqlExecutionStats};
use std::sync::{Arc, Mutex};

pub(in crate::buffered_read::query) struct Cursor<'view, 'q, V: Vfs + Clone> {
    inner: AsyncEdgeProjectionCursor<ProjectionSource<'view, 'q, V>, Checkpoint<'q>>,
}

impl<V: Vfs + Clone> Cursor<'_, '_, V> {
    pub(in crate::buffered_read::query) fn snapshot_seq(&self) -> CommitSeq {
        self.inner.snapshot_seq()
    }

    pub(in crate::buffered_read::query) fn state(&self) -> EdgeScanState {
        self.inner.state()
    }

    pub(in crate::buffered_read::query) fn row_stats(&self) -> GqlExecutionStats {
        self.inner.row_stats()
    }

    pub(in crate::buffered_read::query) fn evaluator_stats(&self) -> GlaExecutionStats {
        self.inner.evaluator_stats()
    }

    pub(in crate::buffered_read::query) fn close(&mut self) {
        self.inner.close();
    }

    pub(in crate::buffered_read::query) async fn next(
        &mut self,
    ) -> Option<Result<BufferedProjectionRow, BufferedProjectionError>> {
        self.inner
            .next()
            .await
            .map(|result| result.map(|output| BufferedQueryRow::from_parts(output.into_parts())))
    }
}

struct ProjectionRecord {
    image: Image,
    // The same owner stays with both orientations and all delivered outputs.
    scratch: Arc<Mutex<MemoryCharge>>,
}

impl AsyncEdgeScanRecord for ProjectionRecord {
    fn edge(&self) -> EdgeScanRow<'_> {
        self.image.edge()
    }

    fn vertex(&self, vid: VId) -> Option<VertexScanRow<'_>> {
        self.image.vertex(vid)
    }
}

struct ProjectionSource<'view, 'q, V: Vfs> {
    inner: Source<'view, 'q, V>,
}

impl<V: Vfs + Clone> AsyncEdgeScanSource for ProjectionSource<'_, '_, V> {
    type Error = BufferedReadError;
    type Record = ProjectionRecord;
    type OutputGuard = BufferedProjectionOutputGuard;

    fn snapshot_seq(&self) -> CommitSeq {
        self.inner.snapshot_seq()
    }

    async fn next_candidate<C: Send>(
        &mut self,
        relation: EdgeRelation,
        control: &mut (impl FnMut(AsyncEdgeScanEvent) -> Result<(), C> + Send),
    ) -> Result<Option<AsyncEdgeCandidate<Self::Record>>, EdgeScanSourceError<Self::Error, C>> {
        self.inner
            .next_candidate(relation, control)
            .await?
            .map(|candidate| {
                let record = candidate
                    .record
                    .map(|record| {
                        let scratch = retain_evaluation(self.inner.cx, record.scratch.into_inner())
                            .map_err(EdgeScanSourceError::Source)?;
                        Ok::<_, EdgeScanSourceError<Self::Error, C>>(ProjectionRecord {
                            image: record.image,
                            scratch,
                        })
                    })
                    .transpose()?;
                Ok(AsyncEdgeCandidate {
                    eid: candidate.eid,
                    record,
                })
            })
            .transpose()
    }

    fn evaluation_event(
        &self,
        record: &Self::Record,
        event: GlaExecutionEvent,
    ) -> Result<(), Self::Error> {
        if event == GlaExecutionEvent::ScratchEntry {
            reserve_evaluation(self.inner.cx, &record.scratch)?;
        }
        Ok(())
    }

    fn reserve_output<C>(
        &self,
        record: &Self::Record,
        columns: usize,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Self::OutputGuard, EdgeScanSourceError<Self::Error, C>> {
        let copy = self.inner.reserve_row_output(record, columns, control)?;
        Ok(BufferedProjectionOutputGuard::new(
            copy,
            Arc::clone(&record.scratch),
        ))
    }
}

impl<V: Vfs + Clone> BufferedReadView<V> {
    pub(in crate::buffered_read::query) fn open_edge_projection<'view, 'q>(
        &'view mut self,
        cx: &'q QueryCx,
        plan: AsyncEdgeProjectionPlan,
        at: CommitSeq,
        policy: GqlQueryPolicy,
    ) -> Result<Cursor<'view, 'q, V>, BufferedProjectionError> {
        let (mut source, checkpoint) = self
            .edge_input_source(cx, at, plan.root_relation())
            .map_err(|error| error.map_source(ScanError::Edge))?;
        // This complete projection proof admits finite endpoint fields too.
        // Do not use the ordinary edge-only optimization, which conservatively
        // falls back to full records when its demand includes endpoint fields.
        source.property_demand = Some(plan.selected_property_demand());
        Ok(Cursor {
            inner: AsyncEdgeProjectionCursor::new(
                ProjectionSource { inner: source },
                plan,
                policy,
                checkpoint,
            ),
        })
    }
}
