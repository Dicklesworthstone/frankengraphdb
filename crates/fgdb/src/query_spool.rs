//! Native pull execution into query-private authenticated result pages. The
//! source cursor alone owns graph semantics; this adapter only frames its
//! existing canonical row encoding and accepts the run after complete success.

use super::*;
use asupersync::io::{AsyncRead, AsyncSeek, AsyncWrite};
use fgdb_gql::scan_stream::{ScanError, ScanKind, ScanState};
use fgdb_gql::{GlaExecutionStats, GqlExecutionStats};
use fgdb_strata::tiered::memory::spill::PagedSpillRun;
use fgdb_strata::tiered::memory::{MemoryCharge, SpillError, SpillFile, TrackedBytes};
use std::sync::Arc;

#[path = "query_spool/aggregate.rs"]
mod aggregate;
#[path = "query_spool/codec.rs"]
mod codec;
#[path = "query_spool/sort.rs"]
mod sort;
pub use aggregate::{
    NativeAggregateSpool, NativeAggregateSpoolCursor, NativeAggregateSpoolError,
    NativeAggregateSpoolRow, PreparedBufferedAggregate,
};
pub use sort::PreparedBufferedOrder;

#[derive(Debug)]
pub enum NativeSpoolError {
    Prepare(Box<QueryError>),
    Execute(Box<GqlQueryError<ScanError<ReadError>, Cancel>>),
    BufferedExecute(Box<GqlQueryError<ScanError<crate::BufferedReadError>, Cancel>>),
    SetExecution(fgdb_gql::GraphSetExecutionError<core::convert::Infallible>),
    Spill(SpillError),
    Encode(fgdb_types::ScalarEncodeError),
    Decode(fgdb_gql::algebra::GraphValueDecodeError),
    RowTooLarge { bytes: usize, limit: usize },
    SortOrder(fgdb_gql::algebra::GraphOrderError),
    SortRunLimit { required: u64, limit: usize },
    SortWorkLimit { attempted: u64, limit: u64 },
    IncompleteCursor,
}
impl core::fmt::Display for NativeSpoolError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Prepare(e) => e.fmt(f),
            Self::Execute(e) => e.fmt(f),
            Self::BufferedExecute(e) => e.fmt(f),
            Self::SetExecution(e) => e.fmt(f),
            Self::Spill(e) => e.fmt(f),
            Self::Encode(e) => e.fmt(f),
            Self::Decode(e) => e.fmt(f),
            Self::SortOrder(e) => e.fmt(f),
            Self::SortRunLimit { required, limit } => {
                write!(f, "result sort needs {required} runs, limit {limit}")
            }
            Self::SortWorkLimit { attempted, limit } => {
                write!(f, "result sort needs {attempted} work units, limit {limit}")
            }
            Self::RowTooLarge { bytes, limit } => write!(
                f,
                "result spool row has {bytes} encoded bytes, limit {limit}"
            ),
            Self::IncompleteCursor => {
                f.write_str("result spool source did not exhaust successfully")
            }
        }
    }
}
impl core::error::Error for NativeSpoolError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Prepare(e) => Some(e.as_ref()),
            Self::Execute(e) => Some(e.as_ref()),
            Self::BufferedExecute(e) => Some(e.as_ref()),
            Self::SetExecution(e) => Some(e),
            Self::Spill(e) => Some(e),
            Self::Encode(e) => Some(e),
            Self::Decode(e) => Some(e),
            Self::SortOrder(e) => Some(e),
            _ => None,
        }
    }
}
impl From<SpillError> for NativeSpoolError {
    fn from(error: SpillError) -> Self {
        Self::Spill(error)
    }
}
impl NativeSpoolError {
    pub fn prepare_error(&self) -> Option<&QueryError> {
        match self {
            Self::Prepare(e) => Some(e),
            _ => None,
        }
    }
    pub fn execution_error(&self) -> Option<&GqlQueryError<ScanError<ReadError>, Cancel>> {
        match self {
            Self::Execute(e) => Some(e),
            _ => None,
        }
    }
    pub fn spill_error(&self) -> Option<&SpillError> {
        match self {
            Self::Spill(e) => Some(e),
            _ => None,
        }
    }
    pub fn buffered_execution_error(
        &self,
    ) -> Option<&GqlQueryError<ScanError<crate::BufferedReadError>, Cancel>> {
        match self {
            Self::BufferedExecute(e) => Some(e),
            _ => None,
        }
    }
}

/// A completely evaluated native result, not a snapshot or query certificate.
/// Only the native spool adapters construct it. No graph, query context, open
/// cursor, parameter map or template is retained. Clones share only the column
/// schema and constant-size scratch root. The file remains host-owned.
///
/// Rows use GraphValueRow::canonical_bytes verbatim, each preceded by its
/// big-endian u64 byte length. Pages may split frames. The reader reassembles
/// one authenticated frame at a time, without decoding/reinterpreting values.
/// This is an ephemeral canonical-result/export substrate, not a new durable
/// format, typed-row decoder or capability grant. `sort_into` orders a completed
/// spool through bounded runs; it does not change the original query's page.
#[derive(Clone)]
pub struct NativeResultSpool {
    columns: Arc<[String]>,
    // Private blocking inputs may also contain trailing hidden sort cells.
    // Only the final window's visible-width handle escapes a native adapter.
    encoded_columns: usize,
    snapshot: CommitSeq,
    kind: ScanKind,
    rows: GqlExecutionStats,
    evaluator: GlaExecutionStats,
    max_row_bytes: usize,
    run: PagedSpillRun,
}
impl core::fmt::Debug for NativeResultSpool {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("NativeResultSpool")
            .field("snapshot", &self.snapshot)
            .field("rows", &self.rows.result_rows)
            .field("encoded_bytes", &self.run.len())
            .field("schema_and_data", &"[REDACTED]")
            .finish()
    }
}
impl NativeResultSpool {
    pub fn columns(&self) -> &[String] {
        &self.columns
    }
    pub fn snapshot_seq(&self) -> CommitSeq {
        self.snapshot
    }
    pub fn kind(&self) -> ScanKind {
        self.kind
    }
    pub fn row_count(&self) -> u64 {
        self.rows.result_rows
    }
    pub fn row_stats(&self) -> GqlExecutionStats {
        self.rows
    }
    pub fn evaluator_stats(&self) -> GlaExecutionStats {
        self.evaluator
    }
    pub fn encoded_len(&self) -> usize {
        self.run.len()
    }
    pub fn page_count(&self) -> u64 {
        self.run.page_count()
    }

    /// Create an independent consumption position without reading anything.
    /// The exclusive file borrow prevents changing the scratch owner while
    /// cached pages are held. A foreign file refuses at first page admission.
    /// Close/drop does not drain unread rows and does not retire the spool.
    pub fn reader<'a, F>(&self, scratch: &'a mut SpillFile<F>) -> NativeSpoolCursor<'a, F> {
        NativeSpoolCursor {
            scratch,
            run: self.run.clone(),
            remaining: self.row_count(),
            max_row_bytes: self.max_row_bytes,
            offset: 0,
            page: None,
            state: if self.row_count() == 0 {
                ScanState::Exhausted
            } else {
                ScanState::Open
            },
        }
    }
}

impl PreparedNativeRead {
    /// Materialize an admitted native pull query directly into spill pages.
    ///
    /// Opening is synchronous and uses stream() verbatim. The returned future
    /// borrows only cx and scratch: the writer may advance/compact/drop while
    /// the immutable cursor is drained. An unpolled future reserves no scratch.
    /// Unsupported physical shapes refuse without eager execution or fallback.
    ///
    /// The native cursor owns the SAME cumulative query policy, ordering,
    /// multiplicity, exact historical cut and row/error semantics. One row is
    /// pulled, encoded and written before the next pull. Source failure, quota
    /// refusal, cancellation or drop exposes no partial result handle. Scratch
    /// attempts started before a failure stay fenced, as PagedSpillWriter defines.
    /// File flush is not a database commit, fsync or persistent result receipt.
    ///
    /// page_bytes is 1..=64 KiB. max_row_bytes is checked against the existing
    /// canonical encoder's output BEFORE scratch acceptance. It is NOT a byte
    /// allocation preflight for that encoder: one governed native row and its
    /// transient encoding remain outside the spill pool. Pages/merge metadata
    /// use that pool; complete result size is bounded by SpillLimits. This does
    /// not make the decoded snapshot, evaluator, or every operator out-of-core.
    /// Privileged embedded API, never a Warden-scoped cache or authorization.
    #[allow(clippy::too_many_arguments)]
    pub fn spool<'q, V, F>(
        &self,
        database: &Database<V>,
        cx: &'q QueryCx,
        params: &GqlParameters,
        policy: GqlQueryPolicy,
        scratch: &'q mut SpillFile<F>,
        page_bytes: usize,
        max_row_bytes: usize,
    ) -> impl Future<Output = Result<NativeResultSpool, NativeSpoolError>> + 'q + use<'q, V, F>
    where
        V: Vfs + Clone,
        F: AsyncRead + AsyncWrite + AsyncSeek + Unpin + 'q,
    {
        let opened = self.stream(database, cx, params, policy);
        async move {
            let (columns, cursor) = opened.map_err(|e| NativeSpoolError::Prepare(Box::new(e)))?;
            let encoded_columns = columns.len();
            drain(
                cx,
                columns,
                encoded_columns,
                cursor,
                scratch,
                page_bytes,
                max_row_bytes,
                None,
            )
            .await
        }
    }

    /// The same materialization over an already admitted immutable view.
    /// Temporal statements still select their own exact cut, never its newer
    /// frontier. The completed spool owns no view; the caller's pin is unchanged.
    #[allow(clippy::too_many_arguments)]
    pub fn spool_in_view<'q, F>(
        &self,
        view: &EmbeddedReadView,
        cx: &'q QueryCx,
        params: &GqlParameters,
        policy: GqlQueryPolicy,
        scratch: &'q mut SpillFile<F>,
        page_bytes: usize,
        max_row_bytes: usize,
    ) -> impl Future<Output = Result<NativeResultSpool, NativeSpoolError>> + 'q + use<'q, F>
    where
        F: AsyncRead + AsyncWrite + AsyncSeek + Unpin + 'q,
    {
        let opened = self.stream_in_view(view, cx, params, policy);
        async move {
            let (columns, cursor) = opened.map_err(|e| NativeSpoolError::Prepare(Box::new(e)))?;
            let encoded_columns = columns.len();
            drain(
                cx,
                columns,
                encoded_columns,
                cursor,
                scratch,
                page_bytes,
                max_row_bytes,
                None,
            )
            .await
        }
    }
}

// A buffered row's reservation must survive encoding and every awaited append,
// even if its source has already exhausted. Resident cursors need no new charge.
// Field order releases the actual row before refunding its reservation.
struct SpoolRow {
    row: GraphValueRow,
    charge: Option<MemoryCharge>,
}
impl SpoolRow {
    fn resident(row: GraphValueRow) -> Self {
        Self { row, charge: None }
    }
    fn buffered((row, charge): (GraphValueRow, MemoryCharge)) -> Self {
        Self {
            row,
            charge: Some(charge),
        }
    }
}

// Type-only adapters for the SAME native cursors. Async sources and resident
// pulls share the complete framing, validation and scratch-acceptance loop.
trait SpoolInput: Send {
    fn pull(&mut self) -> impl Future<Output = Option<Result<SpoolRow, NativeSpoolError>>> + Send;
    fn spool_state(&self) -> ScanState;
    fn spool_stats(&self) -> (CommitSeq, ScanKind, GqlExecutionStats, GlaExecutionStats);
}

impl<VS, VF, ES, EF> SpoolInput for ScanCursor<VS, VF, ES, EF>
where
    VS: VertexScanSource<Error = ReadError> + Send,
    ES: EdgeScanSource<Error = ReadError> + Send,
    VF: FnMut() -> Result<(), Cancel> + Send,
    EF: FnMut() -> Result<(), Cancel> + Send,
{
    async fn pull(&mut self) -> Option<Result<SpoolRow, NativeSpoolError>> {
        self.next().map(|row| {
            row.map(SpoolRow::resident)
                .map_err(|error| NativeSpoolError::Execute(Box::new(error)))
        })
    }
    fn spool_state(&self) -> ScanState {
        self.state()
    }
    fn spool_stats(&self) -> (CommitSeq, ScanKind, GqlExecutionStats, GlaExecutionStats) {
        (
            self.snapshot_seq(),
            self.kind(),
            self.row_stats(),
            self.evaluator_stats(),
        )
    }
}

impl<S, C> SpoolInput for fgdb_gql::stream::VertexScanCursor<S, C, GraphValueRow>
where
    S: VertexScanSource<Error = ReadError> + Send,
    C: FnMut() -> Result<(), Cancel> + Send,
{
    async fn pull(&mut self) -> Option<Result<SpoolRow, NativeSpoolError>> {
        self.next().map(|row| {
            row.map(SpoolRow::resident).map_err(|error| {
                NativeSpoolError::Execute(Box::new(error.map_source(ScanError::Vertex)))
            })
        })
    }
    fn spool_state(&self) -> ScanState {
        self.state()
    }
    fn spool_stats(&self) -> (CommitSeq, ScanKind, GqlExecutionStats, GlaExecutionStats) {
        (
            self.snapshot_seq(),
            ScanKind::Vertex,
            self.row_stats(),
            self.evaluator_stats(),
        )
    }
}

impl<V: Vfs + Clone> SpoolInput for crate::BufferedQueryCursor<'_, '_, V> {
    async fn pull(&mut self) -> Option<Result<SpoolRow, NativeSpoolError>> {
        self.next().await.map(|row| {
            row.map(|row| SpoolRow::buffered(row.into_parts()))
                .map_err(|error| {
                    NativeSpoolError::BufferedExecute(Box::new(error.map_source(ScanError::Vertex)))
                })
        })
    }
    fn spool_state(&self) -> ScanState {
        self.state()
    }
    fn spool_stats(&self) -> (CommitSeq, ScanKind, GqlExecutionStats, GlaExecutionStats) {
        (
            self.snapshot_seq(),
            ScanKind::Vertex,
            self.row_stats(),
            self.evaluator_stats(),
        )
    }
}

impl<S, C> SpoolInput for fgdb_gql::edge_stream::AsyncEdgeScanCursor<S, C>
where
    S: fgdb_gql::edge_stream::AsyncEdgeScanSource<
            Error = crate::BufferedReadError,
            OutputGuard = MemoryCharge,
        >,
    C: FnMut() -> Result<(), Cancel> + Send,
{
    async fn pull(&mut self) -> Option<Result<SpoolRow, NativeSpoolError>> {
        self.next().await.map(|row| {
            row.map(|row| SpoolRow::buffered(row.into_parts()))
                .map_err(|error| {
                    NativeSpoolError::BufferedExecute(Box::new(error.map_source(ScanError::Edge)))
                })
        })
    }
    fn spool_state(&self) -> ScanState {
        use fgdb_gql::edge_stream::EdgeScanState;
        match self.state() {
            EdgeScanState::Open => ScanState::Open,
            EdgeScanState::Exhausted => ScanState::Exhausted,
            EdgeScanState::Closed => ScanState::Closed,
            EdgeScanState::Failed => ScanState::Failed,
        }
    }
    fn spool_stats(&self) -> (CommitSeq, ScanKind, GqlExecutionStats, GlaExecutionStats) {
        (
            self.snapshot_seq(),
            ScanKind::Edge,
            self.row_stats(),
            self.evaluator_stats(),
        )
    }
}

#[allow(clippy::too_many_arguments)]
async fn drain<I, F>(
    cx: &QueryCx,
    columns: Vec<String>,
    encoded_columns: usize,
    mut cursor: I,
    scratch: &mut SpillFile<F>,
    page_bytes: usize,
    max_row_bytes: usize,
    mut encoding_work: Option<&mut sort::Work<'_>>,
) -> Result<NativeResultSpool, NativeSpoolError>
where
    I: SpoolInput,
    F: AsyncRead + AsyncWrite + AsyncSeek + Unpin,
{
    let (snapshot, kind, _, _) = cursor.spool_stats();
    let columns: Arc<[String]> = columns.into();
    let pool = scratch.memory_pool().clone();
    let mut writer = scratch.paged_writer(cx, page_bytes)?;
    let mut count = 0_u64;
    let mut largest = 0;
    while let Some(row) = cursor.pull().await {
        let row = row?;
        if row.row.len() != encoded_columns {
            return Err(SpillError::InvalidRun.into());
        }
        cx.with_restriction(|| cx.checkpoint())
            .map_err(SpillError::Interrupted)?;
        // Keep this tuple intact: fields drop in declaration order on errors,
        // cancellation and unwind too, so bytes always die before the charge.
        let encoded = if row.charge.is_some() {
            let work = encoding_work
                .as_deref_mut()
                .ok_or(NativeSpoolError::IncompleteCursor)?;
            let (bytes, charge) = codec::encode(&pool, work, &row.row, max_row_bytes)?;
            (bytes, Some(charge))
        } else {
            (
                row.row
                    .canonical_bytes()
                    .map_err(NativeSpoolError::Encode)?,
                None,
            )
        };
        let bytes = &encoded.0;
        if bytes.len() > max_row_bytes {
            return Err(NativeSpoolError::RowTooLarge {
                bytes: bytes.len(),
                limit: max_row_bytes,
            });
        }
        let len = u64::try_from(bytes.len()).map_err(|_| SpillError::SizeOverflow)?;
        writer.write(cx, &len.to_be_bytes()).await?;
        writer.write(cx, bytes).await?;
        largest = largest.max(bytes.len());
        count = count.checked_add(1).ok_or(SpillError::SizeOverflow)?;
        drop(encoded);
        drop(row);
    }
    let (_, _, rows, evaluator) = cursor.spool_stats();
    if cursor.spool_state() != ScanState::Exhausted || rows.result_rows != count {
        return Err(NativeSpoolError::IncompleteCursor);
    }
    drop(cursor); // release the native source BEFORE accepting detached results
    let run = writer.finish(cx).await?;
    Ok(NativeResultSpool {
        columns,
        encoded_columns,
        snapshot,
        kind,
        rows,
        evaluator,
        max_row_bytes: largest,
        run,
    })
}

/// Reads canonical row frames, not typed values, from one completed result.
/// Keeps at most one authenticated page between pulls. Output allocations use
/// the SAME pool and require room for a full row plus its current page. Earlier
/// accepted rows remain delivered if a later page is corrupt. An error is
/// returned once and fuses the cursor; a dropped in-flight read also fuses it.
pub struct NativeSpoolCursor<'a, F> {
    scratch: &'a mut SpillFile<F>,
    run: PagedSpillRun,
    remaining: u64,
    max_row_bytes: usize,
    offset: usize,
    page: Option<(u64, TrackedBytes)>,
    state: ScanState,
}
impl<F> NativeSpoolCursor<'_, F> {
    pub fn state(&self) -> ScanState {
        self.state
    }
    pub fn close(&mut self) {
        if self.state == ScanState::Open {
            self.state = ScanState::Closed;
        }
        self.page = None;
    }
}
struct ReadAttempt<'a, 'file, F>(&'a mut NativeSpoolCursor<'file, F>);
impl<F> Drop for ReadAttempt<'_, '_, F> {
    fn drop(&mut self) {
        if self.0.state == ScanState::Failed {
            self.0.page = None;
        }
    }
}
impl<F: AsyncRead + AsyncWrite + AsyncSeek + Unpin> NativeSpoolCursor<'_, F> {
    pub async fn next_row(&mut self, cx: &QueryCx) -> Result<Option<TrackedBytes>, SpillError> {
        if self.state != ScanState::Open {
            return Ok(None);
        }
        self.state = ScanState::Failed;
        let attempt = ReadAttempt(self);
        let output = attempt.0.read_row(cx).await?;
        attempt.0.remaining -= 1;
        attempt.0.state = if attempt.0.remaining == 0 {
            attempt.0.page = None;
            ScanState::Exhausted
        } else {
            ScanState::Open
        };
        Ok(Some(output))
    }

    async fn read_row(&mut self, cx: &QueryCx) -> Result<TrackedBytes, SpillError> {
        let mut length = [0; 8];
        self.fill(cx, &mut length).await?;
        let len =
            usize::try_from(u64::from_be_bytes(length)).map_err(|_| SpillError::InvalidRun)?;
        if len == 0
            || len > self.max_row_bytes
            || self
                .offset
                .checked_add(len)
                .is_none_or(|end| end > self.run.len())
        {
            return Err(SpillError::InvalidRun);
        }
        let mut output = self.scratch.memory_pool().allocate_zeroed(cx, len)?;
        self.fill(cx, output.as_mut()).await?;
        if self.remaining == 1 && self.offset != self.run.len() {
            return Err(SpillError::InvalidRun);
        }
        cx.with_restriction(|| cx.checkpoint())
            .map_err(SpillError::Interrupted)?;
        Ok(output)
    }

    async fn fill(&mut self, cx: &QueryCx, mut output: &mut [u8]) -> Result<(), SpillError> {
        while !output.is_empty() {
            cx.with_restriction(|| cx.checkpoint())
                .map_err(SpillError::Interrupted)?;
            let page = (self.offset / self.run.page_bytes()) as u64;
            if self.page.as_ref().is_none_or(|(stored, _)| *stored != page) {
                self.page = None; // refund predecessor BEFORE admitting successor
                let bytes = self.scratch.restore_page(cx, &self.run, page).await?;
                self.page = Some((page, bytes));
            }
            let bytes = &self.page.as_ref().expect("admitted page").1;
            let at = self.offset % self.run.page_bytes();
            let available = bytes.len().checked_sub(at).ok_or(SpillError::InvalidRun)?;
            let count = output.len().min(available);
            if count == 0 {
                return Err(SpillError::InvalidRun);
            }
            output[..count].copy_from_slice(&bytes.as_ref()[at..at + count]);
            self.offset = self
                .offset
                .checked_add(count)
                .ok_or(SpillError::SizeOverflow)?;
            output = &mut output[count..];
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "query_spool_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "query_spool/buffered_tests.rs"]
mod buffered_tests;
