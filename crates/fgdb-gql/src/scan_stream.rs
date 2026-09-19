//! A fixed choice of an existing vertex or identified-edge pull operator.
//!
//! Native text preparation can choose a source shape without exposing two
//! unrelated return types. This sum does not prepare, retry, meter, prefetch or
//! collect anything: the chosen cursor keeps its source and cumulative policy.

use crate::algebra::GraphValueRow;
use crate::edge_stream::{EdgeScanCursor, EdgeScanError, EdgeScanSource, EdgeScanState};
use crate::stream::{VertexScanCursor, VertexScanError, VertexScanSource};
use crate::{GlaExecutionStats, GqlExecutionStats, GqlQueryError};
use fgdb_types::CommitSeq;

// Both physical cursors implement the same lifecycle. Reuse its existing type
// so consumers of the native vertex stream retain their state comparisons.
pub use crate::stream::VertexScanState as ScanState;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScanKind {
    Vertex,
    Edge,
}

/// Preserve the original operator/source error rather than stringifying it.
/// Resource and interruption errors stay outside this sum in GqlQueryError.
#[derive(Debug)]
pub enum ScanError<E> {
    Vertex(VertexScanError<E>),
    Edge(EdgeScanError<E>),
}
impl<E: core::fmt::Display> core::fmt::Display for ScanError<E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Vertex(error) => error.fmt(f),
            Self::Edge(error) => error.fmt(f),
        }
    }
}
impl<E: core::error::Error + 'static> core::error::Error for ScanError<E> {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Vertex(error) => Some(error),
            Self::Edge(error) => Some(error),
        }
    }
}

/// One already-open physical scan, not an iterator over an eager result bag.
/// Both variants emit GraphValueRow and preserve their native order, counters,
/// late-error behavior and source lifetime. Switching physical strategies is
/// never an error-recovery mechanism; the host chooses before opening a source.
/// Source errors have one shared domain; checkpoint closures may differ while
/// retaining the same interruption type. No extra allocation or meter is added.
pub enum ScanCursor<VS, VF, ES, EF> {
    Vertex(VertexScanCursor<VS, VF, GraphValueRow>),
    Edge(EdgeScanCursor<ES, EF>),
}
impl<VS, VF, ES, EF> ScanCursor<VS, VF, ES, EF>
where
    VS: VertexScanSource,
    ES: EdgeScanSource<Error = VS::Error>,
{
    #[must_use]
    pub fn kind(&self) -> ScanKind {
        match self { Self::Vertex(_) => ScanKind::Vertex, Self::Edge(_) => ScanKind::Edge }
    }
    #[must_use]
    pub fn snapshot_seq(&self) -> CommitSeq {
        match self { Self::Vertex(cursor) => cursor.snapshot_seq(), Self::Edge(cursor) => cursor.snapshot_seq() }
    }
    #[must_use]
    pub fn row_stats(&self) -> GqlExecutionStats {
        match self { Self::Vertex(cursor) => cursor.row_stats(), Self::Edge(cursor) => cursor.row_stats() }
    }
    #[must_use]
    pub fn evaluator_stats(&self) -> GlaExecutionStats {
        match self { Self::Vertex(cursor) => cursor.evaluator_stats(), Self::Edge(cursor) => cursor.evaluator_stats() }
    }
    #[must_use]
    pub fn state(&self) -> ScanState {
        match self {
            Self::Vertex(cursor) => cursor.state(),
            Self::Edge(cursor) => match cursor.state() {
                EdgeScanState::Open => ScanState::Open,
                EdgeScanState::Exhausted => ScanState::Exhausted,
                EdgeScanState::Closed => ScanState::Closed,
                EdgeScanState::Failed => ScanState::Failed,
            },
        }
    }
    /// Idempotent; the selected cursor releases its pin without another pull.
    pub fn close(&mut self) {
        match self { Self::Vertex(cursor) => cursor.close(), Self::Edge(cursor) => cursor.close() }
    }
}
impl<VS, VF, ES, EF, C> Iterator for ScanCursor<VS, VF, ES, EF>
where
    VS: VertexScanSource,
    ES: EdgeScanSource<Error = VS::Error>,
    VF: FnMut() -> Result<(), C>,
    EF: FnMut() -> Result<(), C>,
{
    type Item = Result<GraphValueRow, GqlQueryError<ScanError<VS::Error>, C>>;
    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Vertex(cursor) => cursor.next().map(|row| row.map_err(|error| error.map_source(ScanError::Vertex))),
            Self::Edge(cursor) => cursor.next().map(|row| row.map_err(|error| error.map_source(ScanError::Edge))),
        }
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        match self { Self::Vertex(cursor) => cursor.size_hint(), Self::Edge(cursor) => cursor.size_hint() }
    }
}
impl<VS, VF, ES, EF, C> std::iter::FusedIterator for ScanCursor<VS, VF, ES, EF>
where
    VS: VertexScanSource,
    ES: EdgeScanSource<Error = VS::Error>,
    VF: FnMut() -> Result<(), C>,
    EF: FnMut() -> Result<(), C>,
{}
impl<VS, VF, ES, EF> core::fmt::Debug for ScanCursor<VS, VF, ES, EF> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Vertex(cursor) => f.debug_tuple("ScanCursor::Vertex").field(cursor).finish(),
            Self::Edge(cursor) => f.debug_tuple("ScanCursor::Edge").field(cursor).finish(),
        }
    }
}
