//! Grace partitioning of native aggregate INPUT occurrences. Completed groups
//! never accumulate in a graph-wide resident table. Scratch is authenticated,
//! query-private and append-only, and no result handle escapes before every
//! source occurrence, partition and final ordering pass has succeeded.

use super::*;
use fgdb_gql::algebra::{GraphValue, GraphValueDecodeError, GraphValueOrder};
use fgdb_gql::spill_aggregate::{
    EdgeSpillAggregateCursor, SpillAggregateDefinition, SpillAggregatePlan, SpillAggregateState,
    VertexSpillAggregateCursor,
};
use fgdb_gql::stream::VertexScanEvent;
use fgdb_gql::{GraphAggregateRow, GraphAggregateTextSlot, GraphAggregateValue, GraphExactAverage};
use fgdb_strata::tiered::memory::{MemoryCharge, MemoryError, MemoryPool};
use fgdb_types::{CanonicalScalar, CanonicalScalarResolver};

#[path = "aggregate_account.rs"]
mod account;
#[cfg(test)]
#[path = "aggregate_codec_tests.rs"]
mod codec_tests;

type Result<T> = core::result::Result<T, NativeAggregateSpoolError>;

/// Aggregate spill preserves exact aggregate/source errors and the existing
/// scratch error, rather than falling back to an eager query after refusal.
#[derive(Debug)]
pub enum NativeAggregateSpoolError {
    Prepare(Box<QueryError>),
    Execute(
        Box<fgdb_gql::GqlQueryError<fgdb_gql::GraphAggregateError<ScanError<ReadError>>, Cancel>>,
    ),
    Spool(NativeSpoolError),
    Decode(GraphValueDecodeError),
    Unsupported,
    PartitionLimit {
        required: usize,
        limit: usize,
    },
    PartitionDepth,
    InputRows {
        attempted: u64,
        limit: u64,
    },
}
impl core::fmt::Display for NativeAggregateSpoolError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Prepare(error) => error.fmt(f),
            Self::Execute(error) => error.fmt(f),
            Self::Spool(error) => error.fmt(f),
            Self::Decode(error) => error.fmt(f),
            Self::Unsupported => f.write_str("aggregate spill requires plain native COUNT/SUM/AVG/MIN/MAX without DISTINCT, collection, computed input or result clauses"),
            Self::PartitionLimit { required, limit } => write!(f, "ResourceExhausted: aggregate spill needs {required} partitions, limit {limit}"),
            Self::PartitionDepth => f.write_str("ResourceExhausted: aggregate radix partition cannot separate its remaining groups"),
            Self::InputRows { attempted, limit } => write!(f, "aggregate spill needs {attempted} input rows, limit {limit}"),
        }
    }
}
impl core::error::Error for NativeAggregateSpoolError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Prepare(error) => Some(error.as_ref()),
            Self::Execute(error) => Some(error.as_ref()),
            Self::Spool(error) => Some(error),
            Self::Decode(error) => Some(error),
            _ => None,
        }
    }
}
impl From<NativeSpoolError> for NativeAggregateSpoolError {
    fn from(error: NativeSpoolError) -> Self {
        Self::Spool(error)
    }
}
impl From<SpillError> for NativeAggregateSpoolError {
    fn from(error: SpillError) -> Self {
        Self::Spool(error.into())
    }
}
impl From<GraphValueDecodeError> for NativeAggregateSpoolError {
    fn from(error: GraphValueDecodeError) -> Self {
        Self::Decode(error)
    }
}

/// A completed aggregate result. The public schema retains native RETURN
/// order, and the reader returns exact wide numeric domains. The private row
/// envelope is an ephemeral scratch format, never an exported scalar result.
#[derive(Clone)]
pub struct NativeAggregateSpool {
    inner: NativeResultSpool,
    columns: Arc<[String]>,
    slots: Arc<[GraphAggregateTextSlot]>,
    keys: Arc<[String]>,
    aggregates: Arc<[String]>,
}
impl NativeAggregateSpool {
    pub fn columns(&self) -> &[String] {
        &self.columns
    }
    pub fn output_slots(&self) -> &[GraphAggregateTextSlot] {
        &self.slots
    }
    pub fn key_columns(&self) -> &[String] {
        &self.keys
    }
    pub fn aggregate_columns(&self) -> &[String] {
        &self.aggregates
    }
    pub fn snapshot_seq(&self) -> CommitSeq {
        self.inner.snapshot_seq()
    }
    pub fn kind(&self) -> ScanKind {
        self.inner.kind()
    }
    pub fn row_count(&self) -> u64 {
        self.inner.row_count()
    }
    pub fn row_stats(&self) -> GqlExecutionStats {
        self.inner.row_stats()
    }
    pub fn evaluator_stats(&self) -> GlaExecutionStats {
        self.inner.evaluator_stats()
    }
    pub fn reader<'a, F>(
        &self,
        destination: &'a mut SpillFile<F>,
        resolver: Option<&'a (dyn CanonicalScalarResolver + Send + Sync)>,
    ) -> NativeAggregateSpoolCursor<'a, F> {
        NativeAggregateSpoolCursor {
            pool: destination.memory_pool().clone(),
            inner: self.inner.reader(destination),
            keys: self.keys.len(),
            aggregates: self.aggregates.len(),
            resolver,
            failed: false,
        }
    }
}
impl core::fmt::Debug for NativeAggregateSpool {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("NativeAggregateSpool")
            .field("snapshot", &self.snapshot_seq())
            .field("rows", &self.row_count())
            .field("schema_and_values", &"[REDACTED]")
            .finish()
    }
}

/// One owned result whose decoded allocation reservation follows its lifetime.
/// Dropping it refunds memory; it does not retain a source pin or scratch file.
pub struct NativeAggregateSpoolRow {
    row: GraphAggregateRow,
    _charge: MemoryCharge,
}
impl core::ops::Deref for NativeAggregateSpoolRow {
    type Target = GraphAggregateRow;
    fn deref(&self) -> &Self::Target {
        &self.row
    }
}
impl core::fmt::Debug for NativeAggregateSpoolRow {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("NativeAggregateSpoolRow([REDACTED])")
    }
}

pub struct NativeAggregateSpoolCursor<'a, F> {
    inner: NativeSpoolCursor<'a, F>,
    pool: MemoryPool,
    keys: usize,
    aggregates: usize,
    resolver: Option<&'a (dyn CanonicalScalarResolver + Send + Sync)>,
    failed: bool,
}
impl<F> NativeAggregateSpoolCursor<'_, F> {
    pub fn state(&self) -> ScanState {
        if self.failed {
            ScanState::Failed
        } else {
            self.inner.state()
        }
    }
    pub fn close(&mut self) {
        self.inner.close();
    }
}
impl<F: AsyncRead + AsyncWrite + AsyncSeek + Unpin> NativeAggregateSpoolCursor<'_, F> {
    pub async fn next_row(&mut self, cx: &QueryCx) -> Result<Option<NativeAggregateSpoolRow>> {
        if self.failed {
            return Ok(None);
        }
        // Cancellation while a read is pending, decode refusal and memory
        // refusal permanently fuse this typed wrapper as well as its reader.
        self.failed = true;
        let Some(bytes) = self.inner.next_row(cx).await? else {
            self.failed = false;
            return Ok(None);
        };
        let charge = decoded_reservation(&self.pool, cx, bytes.as_ref(), 2)?;
        let row = decode_row(bytes.as_ref(), self.resolver)?;
        let row = decode_envelope(&row, self.keys, self.aggregates)?;
        self.failed = false;
        Ok(Some(NativeAggregateSpoolRow {
            row,
            _charge: charge,
        }))
    }
}

fn decoded_reservation(
    pool: &MemoryPool,
    cx: &QueryCx,
    bytes: &[u8],
    copies: usize,
) -> Result<MemoryCharge> {
    let budget = account::decoded(bytes, cx)?
        .checked_mul(copies)
        .ok_or(SpillError::SizeOverflow)?;
    pool.reserve(cx, budget)
        .map_err(|error| SpillError::Memory(error).into())
}
fn decode_row(
    bytes: &[u8],
    resolver: Option<&(dyn CanonicalScalarResolver + Send + Sync)>,
) -> Result<GraphValueRow> {
    Ok(match resolver {
        Some(resolver) => GraphValueRow::decode_canonical_with_resolver(bytes, resolver),
        None => GraphValueRow::decode_canonical(bytes),
    }?)
}
fn invalid<T>() -> Result<T> {
    Err(SpillError::InvalidRun.into())
}

fn envelope(row: &GraphAggregateRow) -> GraphValueRow {
    let mut cells = row.keys().to_vec();
    for value in row.values() {
        let (tag, payload) = match value {
            GraphAggregateValue::Count(value) => (
                0,
                GraphValue::Scalar(
                    CanonicalScalar::bytes(value.to_be_bytes().to_vec())
                        .expect("fixed numeric payload"),
                ),
            ),
            GraphAggregateValue::Integer(value) => (
                1,
                GraphValue::Scalar(
                    CanonicalScalar::bytes(value.to_be_bytes().to_vec())
                        .expect("fixed numeric payload"),
                ),
            ),
            GraphAggregateValue::Value(value) => (2, value.clone()),
            GraphAggregateValue::Average(value) => {
                let mut bytes = value.numerator().to_be_bytes().to_vec();
                bytes.extend_from_slice(&value.denominator().to_be_bytes());
                (
                    3,
                    GraphValue::Scalar(
                        CanonicalScalar::bytes(bytes).expect("fixed numeric payload"),
                    ),
                )
            }
        };
        cells.push(GraphValue::List(
            vec![GraphValue::Scalar(CanonicalScalar::Int(tag)), payload].into_boxed_slice(),
        ));
    }
    GraphValueRow::from_owned_values(cells)
}
fn decode_envelope(
    row: &GraphValueRow,
    keys: usize,
    aggregates: usize,
) -> Result<GraphAggregateRow> {
    if row.len()
        != keys
            .checked_add(aggregates)
            .ok_or(SpillError::SizeOverflow)?
    {
        return invalid();
    }
    let mut values = Vec::with_capacity(aggregates);
    for value in &row.values()[keys..] {
        let GraphValue::List(items) = value else {
            return invalid();
        };
        let [GraphValue::Scalar(CanonicalScalar::Int(tag)), payload] = items.as_ref() else {
            return invalid();
        };
        let value = if *tag == 2 {
            GraphAggregateValue::Value(payload.clone())
        } else {
            let GraphValue::Scalar(CanonicalScalar::Bytes(bytes)) = payload else {
                return invalid();
            };
            let bytes = bytes.as_slice();
            match (*tag, bytes.len()) {
                (0, 8) => GraphAggregateValue::Count(u64::from_be_bytes(
                    bytes.try_into().expect("checked length"),
                )),
                (1, 16) => GraphAggregateValue::Integer(i128::from_be_bytes(
                    bytes.try_into().expect("checked length"),
                )),
                (3, 24) => {
                    let numerator =
                        i128::from_be_bytes(bytes[..16].try_into().expect("checked length"));
                    let denominator =
                        u64::from_be_bytes(bytes[16..].try_into().expect("checked length"));
                    let value = GraphExactAverage::new(numerator, denominator)
                        .ok_or(SpillError::InvalidRun)?;
                    if value.numerator() != numerator || value.denominator() != denominator {
                        return invalid();
                    }
                    GraphAggregateValue::Average(value)
                }
                _ => return invalid(),
            }
        };
        values.push(value);
    }
    Ok(GraphAggregateRow::from_group_values(
        row.values()[..keys].to_vec(),
        values,
    ))
}

type ExecutionError =
    fgdb_gql::GqlQueryError<fgdb_gql::GraphAggregateError<ScanError<ReadError>>, Cancel>;

// The source cursor remains the sole cumulative GQL meter during partition
// reduction. Exhaustion releases its source pin but leaves the meter alive.
trait GroupInput: Send {
    fn next_input(&mut self) -> core::result::Result<Option<GraphValueRow>, ExecutionError>;
    fn charge(&mut self, event: VertexScanEvent) -> core::result::Result<(), ExecutionError>;
    fn finish_result(&mut self) -> core::result::Result<(), ExecutionError>;
    fn exhausted(&self) -> bool;
    fn snapshot_seq(&self) -> CommitSeq;
    fn kind(&self) -> ScanKind;
    fn row_stats(&self) -> GqlExecutionStats;
    fn evaluator_stats(&self) -> GlaExecutionStats;
}
macro_rules! input_adapter {
    ($cursor:ident, $source:ident, $variant:ident, $exhausted:path) => {
        impl<S, F> GroupInput for $cursor<S, F>
        where
            S: $source<Error = ReadError> + Send,
            F: FnMut() -> core::result::Result<(), Cancel> + Send,
        {
            fn next_input(
                &mut self,
            ) -> core::result::Result<Option<GraphValueRow>, ExecutionError> {
                $cursor::next_input(self).map_err(|error| {
                    error.map_source(|error| error.map_source(ScanError::$variant))
                })
            }
            fn charge(
                &mut self,
                event: VertexScanEvent,
            ) -> core::result::Result<(), ExecutionError> {
                $cursor::charge(self, event).map_err(|error| {
                    error.map_source(|error| error.map_source(ScanError::$variant))
                })
            }
            fn finish_result(&mut self) -> core::result::Result<(), ExecutionError> {
                $cursor::finish_result(self).map_err(|error| {
                    error.map_source(|error| error.map_source(ScanError::$variant))
                })
            }
            fn exhausted(&self) -> bool {
                $cursor::state(self) == $exhausted
            }
            fn snapshot_seq(&self) -> CommitSeq {
                $cursor::snapshot_seq(self)
            }
            fn kind(&self) -> ScanKind {
                ScanKind::$variant
            }
            fn row_stats(&self) -> GqlExecutionStats {
                $cursor::row_stats(self)
            }
            fn evaluator_stats(&self) -> GlaExecutionStats {
                $cursor::evaluator_stats(self)
            }
        }
    };
}
input_adapter!(
    VertexSpillAggregateCursor,
    VertexScanSource,
    Vertex,
    fgdb_gql::stream::VertexScanState::Exhausted
);
input_adapter!(
    EdgeSpillAggregateCursor,
    EdgeScanSource,
    Edge,
    fgdb_gql::edge_stream::EdgeScanState::Exhausted
);

struct Work<'a> {
    cx: &'a QueryCx,
    used: u64,
    limit: u64,
}
impl Work<'_> {
    fn charge(&mut self, bytes: usize) -> Result<()> {
        self.cx
            .with_restriction(|| self.cx.checkpoint())
            .map_err(SpillError::Interrupted)?;
        let attempted = self
            .used
            .checked_add(u64::try_from(bytes).map_err(|_| SpillError::SizeOverflow)?)
            .ok_or(SpillError::SizeOverflow)?;
        if attempted > self.limit {
            return Err(NativeSpoolError::SortWorkLimit {
                attempted,
                limit: self.limit,
            }
            .into());
        }
        self.used = attempted;
        Ok(())
    }
    async fn write<F: AsyncRead + AsyncWrite + AsyncSeek + Unpin>(
        &mut self,
        writer: &mut fgdb_strata::tiered::memory::spill::PagedSpillWriter<'_, F>,
        bytes: &[u8],
    ) -> Result<()> {
        self.charge(8)?;
        writer
            .write(self.cx, &(bytes.len() as u64).to_be_bytes())
            .await?;
        for chunk in bytes.chunks(4096) {
            self.charge(chunk.len())?;
            writer.write(self.cx, chunk).await?;
        }
        Ok(())
    }
}

struct Catalog<T> {
    values: Vec<T>,
    _charge: MemoryCharge,
    _extra: Option<MemoryCharge>,
}
impl<T> Catalog<T> {
    fn new(pool: &MemoryPool, cx: &QueryCx, capacity: usize) -> Result<Self> {
        let requested = capacity
            .checked_mul(size_of::<T>())
            .ok_or(SpillError::SizeOverflow)?;
        let charge = pool.reserve(cx, requested).map_err(SpillError::Memory)?;
        let mut values = Vec::new();
        values
            .try_reserve_exact(capacity)
            .map_err(|_| SpillError::Memory(MemoryError::AllocationFailed { requested }))?;
        let actual = values
            .capacity()
            .checked_mul(size_of::<T>())
            .ok_or(SpillError::SizeOverflow)?;
        let extra = if actual > requested {
            Some(
                pool.reserve(cx, actual - requested)
                    .map_err(SpillError::Memory)?,
            )
        } else {
            None
        };
        Ok(Self {
            values,
            _charge: charge,
            _extra: extra,
        })
    }
}

struct Partition {
    spool: NativeResultSpool,
    in_source: bool,
    depth: usize,
}
struct Group {
    exemplar: GraphValueRow,
    state: SpillAggregateState,
    largest: usize,
    _charge: MemoryCharge,
}

struct Opened<'q> {
    input: Box<dyn GroupInput + 'q>,
    definition: SpillAggregateDefinition,
    columns: Vec<String>,
    slots: Vec<GraphAggregateTextSlot>,
}

fn open<'q>(
    prepared: &PreparedNativeRead,
    view: &EmbeddedReadView,
    cx: &'q QueryCx,
    params: &GqlParameters,
    policy: GqlQueryPolicy,
) -> Result<Opened<'q>> {
    let (definition, columns, slots, as_of) = match prepared {
        PreparedNativeRead::Aggregate(prepared) => (
            prepared.bind_parameters(params).map_err(|error| {
                NativeAggregateSpoolError::Prepare(Box::new(QueryError::PatternText(error)))
            })?,
            prepared.columns(),
            prepared.output_slots(),
            view.frontier(),
        ),
        PreparedNativeRead::TemporalAggregate(prepared) => {
            let query = prepared.bind_parameters(params).map_err(|error| {
                NativeAggregateSpoolError::Prepare(Box::new(QueryError::TemporalText(error)))
            })?;
            (
                query.aggregate().clone(),
                prepared.columns(),
                prepared.output_slots(),
                query.as_of(),
            )
        }
        PreparedNativeRead::PipelineAggregate(prepared) => (
            prepared.bind_parameters(params).map_err(|error| {
                NativeAggregateSpoolError::Prepare(Box::new(QueryError::PipelineText(error)))
            })?,
            prepared.columns(),
            prepared.output_slots(),
            view.frontier(),
        ),
        _ => return Err(NativeAggregateSpoolError::Unsupported),
    };
    let plan = SpillAggregatePlan::compile(&definition)
        .map_err(|_| NativeAggregateSpoolError::Unsupported)?;
    let definition = plan.definition().clone();
    if !crate::query::aggregate_stream::valid_layout(
        columns,
        slots,
        definition.key_columns(),
        definition.aggregate_columns(),
    ) {
        return Err(NativeAggregateSpoolError::Unsupported);
    }
    cx.with_restriction(|| cx.checkpoint())
        .map_err(SpillError::Interrupted)?;
    let input: Box<dyn GroupInput + 'q> = match plan {
        SpillAggregatePlan::Vertex(plan) => Box::new(VertexSpillAggregateCursor::new(
            view.vertex_scan_source(cx, as_of).map_err(|error| {
                NativeAggregateSpoolError::Prepare(Box::new(QueryError::Read(error)))
            })?,
            plan,
            policy,
            move || cx.with_restriction(|| cx.checkpoint()),
        )),
        SpillAggregatePlan::Edge(plan) => Box::new(EdgeSpillAggregateCursor::new(
            view.edge_scan_source(cx, as_of).map_err(|error| {
                NativeAggregateSpoolError::Prepare(Box::new(QueryError::Read(error)))
            })?,
            plan,
            policy,
            move || cx.with_restriction(|| cx.checkpoint()),
        )),
    };
    Ok(Opened {
        input,
        definition,
        columns: columns.to_vec(),
        slots: slots.to_vec(),
    })
}

impl PreparedNativeRead {
    /// Execute plain native grouped/global COUNT, SUM, AVG, MIN and MAX using
    /// bounded grace partitions of input occurrences. This does not run the
    /// resident aggregate cursor or materialize completed groups before spill.
    ///
    /// The compiler refuses DISTINCT, COLLECT, computed/relational input and
    /// result clauses before source opening. Every partition preserves original
    /// occurrence order within each group. Final rows use canonical key order;
    /// exact u64 counts, i128 sums, rational averages and binary64 promotion are
    /// the ordinary reducer's domains. Empty global input yields one row.
    ///
    /// Three distinct append-only files should share one memory pool. At most
    /// group_capacity owned group states and max_partitions metadata entries
    /// are admitted, with conservative pool charges before decoded retention.
    /// A partition that exceeds group/memory capacity is repartitioned. A single
    /// group that cannot fit, hash-depth exhaustion, disk/run/work limits and
    /// cancellation refuse without publishing a result. No quota is refunded
    /// for abandoned reduction attempts or completed intermediate runs.
    ///
    /// max_row_bytes bounds every full input frame and result envelope;
    /// max_input_rows bounds matched occurrences, independently of result quota.
    /// The source/reducer share one cumulative GQL meter; max_work_units covers
    /// additional partition, codec, sort and copy work. The decoded snapshot and
    /// one source-projected row are governed by native source policies, not the
    /// scratch pool. Artifact-bound scalars use only the supplied resolver.
    /// The completed handle always belongs to destination, even after sorting.
    #[allow(clippy::too_many_arguments)]
    pub fn spool_aggregate_in_view<'q, A, B, C>(
        &self,
        view: &EmbeddedReadView,
        cx: &'q QueryCx,
        params: &GqlParameters,
        policy: GqlQueryPolicy,
        source: &'q mut SpillFile<A>,
        partition: &'q mut SpillFile<B>,
        destination: &'q mut SpillFile<C>,
        group_capacity: usize,
        max_partitions: usize,
        run_rows: usize,
        max_runs: usize,
        page_bytes: usize,
        max_row_bytes: usize,
        max_input_rows: u64,
        max_work_units: u64,
        resolver: Option<&'q (dyn CanonicalScalarResolver + Send + Sync)>,
    ) -> impl Future<Output = Result<(NativeAggregateSpool, u64)>> + 'q + use<'q, A, B, C>
    where
        A: AsyncRead + AsyncWrite + AsyncSeek + Unpin + 'q,
        B: AsyncRead + AsyncWrite + AsyncSeek + Unpin + 'q,
        C: AsyncRead + AsyncWrite + AsyncSeek + Unpin + 'q,
    {
        let opened = open(self, view, cx, params, policy);
        async move {
            execute(
                opened?,
                cx,
                source,
                partition,
                destination,
                group_capacity,
                max_partitions,
                run_rows,
                max_runs,
                page_bytes,
                max_row_bytes,
                max_input_rows,
                max_work_units,
                resolver,
            )
            .await
        }
    }
}

fn execute_error(error: ExecutionError) -> NativeAggregateSpoolError {
    NativeAggregateSpoolError::Execute(Box::new(error))
}
fn row_limit(bytes: usize, limit: usize) -> Result<()> {
    if bytes > limit {
        return Err(NativeSpoolError::RowTooLarge { bytes, limit }.into());
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn drain_input<F: AsyncRead + AsyncWrite + AsyncSeek + Unpin>(
    input: &mut dyn GroupInput,
    definition: &SpillAggregateDefinition,
    file: &mut SpillFile<F>,
    page_bytes: usize,
    max_row_bytes: usize,
    max_input_rows: u64,
    work: &mut Work<'_>,
) -> Result<NativeResultSpool> {
    let pool = file.memory_pool().clone();
    let mut writer = file.paged_writer(work.cx, page_bytes)?;
    let mut count = 0_u64;
    let mut largest = 0;
    while let Some(row) = input.next_input().map_err(execute_error)? {
        work.charge(1)?;
        let attempted = count.checked_add(1).ok_or(SpillError::SizeOverflow)?;
        if attempted > max_input_rows {
            return Err(NativeAggregateSpoolError::InputRows {
                attempted,
                limit: max_input_rows,
            });
        }
        if row.len() != definition.input_width() {
            return invalid();
        }
        let (bytes, _codec) = account::encode(&pool, work, &row, max_row_bytes)?;
        largest = largest.max(bytes.len());
        work.write(&mut writer, &bytes).await?;
        count = attempted;
    }
    if !input.exhausted() || input.row_stats().result_rows != 0 {
        return Err(NativeSpoolError::IncompleteCursor.into());
    }
    let run = writer.finish(work.cx).await?;
    Ok(NativeResultSpool {
        columns: Arc::from([]),
        encoded_columns: definition.input_width(),
        snapshot: input.snapshot_seq(),
        kind: input.kind(),
        rows: GqlExecutionStats {
            snapshot_records: input.row_stats().snapshot_records,
            result_rows: count,
        },
        evaluator: input.evaluator_stats(),
        max_row_bytes: largest,
        run,
    })
}

fn group_charge_bytes(definition: &SpillAggregateDefinition, largest: usize) -> Result<usize> {
    // Exemplar + completed-row key + envelope key, and two copies of each
    // retained extremum while the lossless envelope is encoded. Numeric cells
    // and their promotion boxes are already covered by fixed state bytes.
    largest
        .checked_mul(
            definition
                .extremum_count()
                .checked_mul(2)
                .and_then(|count| count.checked_add(3))
                .ok_or(SpillError::SizeOverflow)?,
        )
        .ok_or(SpillError::SizeOverflow)?
        .checked_add(definition.state_resident_bytes())
        .and_then(|bytes| bytes.checked_add(fixed_output_bytes(definition)))
        .ok_or_else(|| SpillError::SizeOverflow.into())
}

fn fixed_output_bytes(definition: &SpillAggregateDefinition) -> usize {
    // At most MAX_PATTERN_VERTICES cells: completed values, outer envelope
    // vector growth/boxing, two list cells and a fixed numeric payload per
    // aggregate coexist BEFORE the byte encoder workspace is acquired.
    definition.aggregate_columns().len()
        * (6 * size_of::<GraphValue>() + 2 * size_of::<GraphAggregateValue>() + 64)
        + size_of::<GraphAggregateRow>()
        + size_of::<GraphValueRow>()
}

// A probe is speculative private work, never partial output. On overflow its
// bounded table is dropped before repartitioning the complete original run.
#[allow(clippy::too_many_arguments)]
async fn reduce_partition<F: AsyncRead + AsyncWrite + AsyncSeek + Unpin>(
    spool: &NativeResultSpool,
    file: &mut SpillFile<F>,
    definition: &SpillAggregateDefinition,
    input: &mut dyn GroupInput,
    capacity: usize,
    work: &mut Work<'_>,
    resolver: Option<&(dyn CanonicalScalarResolver + Send + Sync)>,
) -> Result<Option<Catalog<Group>>> {
    let pool = file.memory_pool().clone();
    let mut groups = Catalog::<Group>::new(&pool, work.cx, capacity)?;
    let mut reader = spool.reader(file);
    loop {
        let bytes = match reader.next_row(work.cx).await {
            Ok(Some(bytes)) => bytes,
            Ok(None) => break,
            Err(SpillError::Memory(MemoryError::ResourceExhausted { .. }))
                if !groups.values.is_empty() =>
            {
                return Ok(None);
            }
            Err(error) => return Err(error.into()),
        };
        work.charge(bytes.len())?;
        let resident = account::decoded(bytes.as_ref(), work.cx)?;
        let _decoded = match pool.reserve(work.cx, resident) {
            Ok(charge) => charge,
            Err(MemoryError::ResourceExhausted { .. }) if !groups.values.is_empty() => {
                return Ok(None);
            }
            Err(error) => return Err(SpillError::Memory(error).into()),
        };
        let row = decode_row(bytes.as_ref(), resolver)?;
        if row.len() != definition.input_width() {
            return invalid();
        }
        let mut existing = None;
        for (at, group) in groups.values.iter().enumerate() {
            work.charge(bytes.len().saturating_add(group.largest))?;
            if definition
                .group_key_columns()
                .iter()
                .all(|&key| row.values()[key] == group.exemplar.values()[key])
            {
                existing = Some(at);
                break;
            }
        }
        if let Some(at) = existing {
            let largest = groups.values[at].largest.max(resident);
            if largest > groups.values[at].largest {
                // The replacement charge overlaps the predecessor until the
                // stronger reservation exists. There is no uncharged interval.
                let charge = match pool.reserve(work.cx, group_charge_bytes(definition, largest)?) {
                    Ok(charge) => charge,
                    Err(MemoryError::ResourceExhausted { .. }) if groups.values.len() > 1 => {
                        return Ok(None);
                    }
                    Err(error) => return Err(SpillError::Memory(error).into()),
                };
                groups.values[at]._charge = charge;
                groups.values[at].largest = largest;
            }
            definition
                .update(&mut groups.values[at].state, &row, &mut |event| {
                    input.charge(event)
                })
                .map_err(execute_error)?;
        } else {
            if groups.values.len() == capacity {
                return Ok(None);
            }
            let charge = match pool.reserve(work.cx, group_charge_bytes(definition, resident)?) {
                Ok(charge) => charge,
                Err(MemoryError::ResourceExhausted { .. }) if !groups.values.is_empty() => {
                    return Ok(None);
                }
                Err(error) => return Err(SpillError::Memory(error).into()),
            };
            let mut state = definition
                .new_state(&mut |event| input.charge(event))
                .map_err(execute_error)?;
            definition
                .update(&mut state, &row, &mut |event| input.charge(event))
                .map_err(execute_error)?;
            groups.values.push(Group {
                exemplar: row,
                state,
                largest: resident,
                _charge: charge,
            });
        }
    }
    if reader.state() != ScanState::Exhausted {
        return Err(NativeSpoolError::IncompleteCursor.into());
    }
    Ok(Some(groups))
}

fn key_hash(
    row: &GraphValueRow,
    definition: &SpillAggregateDefinition,
    pool: &MemoryPool,
    work: &mut Work<'_>,
) -> Result<[u8; 32]> {
    if row.len() != definition.input_width() {
        return invalid();
    }
    let mut hash = fgdb_crypto::Hasher::new();
    hash.update(b"fgdb.native-aggregate.partition.v1");
    // The selected values' combined canonical bytes are no larger than the
    // complete row. Reserve one codec workspace before allocating any key.
    let (len, nodes) = account::encoded_shape(row, work.cx)?;
    work.charge(len.checked_add(nodes).ok_or(SpillError::SizeOverflow)?)?;
    let budget = len
        .checked_mul(8)
        .and_then(|bytes| {
            nodes
                .checked_mul(128)
                .and_then(|node_bytes| bytes.checked_add(node_bytes))
        })
        .ok_or(SpillError::SizeOverflow)?;
    let _codec = pool.reserve(work.cx, budget).map_err(SpillError::Memory)?;
    for &key in definition.group_key_columns() {
        let bytes = row.values()[key]
            .canonical_bytes()
            .map_err(NativeSpoolError::Encode)?;
        work.charge(8)?;
        hash.update(&(bytes.len() as u64).to_be_bytes());
        for chunk in bytes.chunks(4096) {
            work.charge(chunk.len())?;
            hash.update(chunk);
        }
    }
    Ok(hash.finalize().0)
}

#[allow(clippy::too_many_arguments)]
async fn split_partition<A, B>(
    parent: &Partition,
    source: &mut SpillFile<A>,
    destination: &mut SpillFile<B>,
    definition: &SpillAggregateDefinition,
    page_bytes: usize,
    work: &mut Work<'_>,
    resolver: Option<&(dyn CanonicalScalarResolver + Send + Sync)>,
) -> Result<[NativeResultSpool; 2]>
where
    A: AsyncRead + AsyncWrite + AsyncSeek + Unpin,
    B: AsyncRead + AsyncWrite + AsyncSeek + Unpin,
{
    if parent.depth >= 256 {
        return Err(NativeAggregateSpoolError::PartitionDepth);
    }
    let pool = source.memory_pool().clone();
    let mut children = [None, None];
    // PagedSpillWriter owns an exclusive file borrow. Two bounded read passes
    // avoid an unbounded open-writer/file catalog and preserve source order.
    for selected in [0_u8, 1] {
        let mut reader = parent.spool.reader(source);
        let mut writer = destination.paged_writer(work.cx, page_bytes)?;
        let mut rows = 0_u64;
        while let Some(bytes) = reader.next_row(work.cx).await? {
            work.charge(bytes.len())?;
            let _decoded = decoded_reservation(&pool, work.cx, bytes.as_ref(), 1)?;
            let row = decode_row(bytes.as_ref(), resolver)?;
            let hash = key_hash(&row, definition, &pool, work)?;
            if (hash[parent.depth / 8] >> (parent.depth % 8)) & 1 == selected {
                work.write(&mut writer, bytes.as_ref()).await?;
                rows = rows.checked_add(1).ok_or(SpillError::SizeOverflow)?;
            }
        }
        if reader.state() != ScanState::Exhausted {
            return Err(NativeSpoolError::IncompleteCursor.into());
        }
        let run = writer.finish(work.cx).await?;
        let mut child = parent.spool.clone();
        child.run = run;
        child.rows.result_rows = rows;
        children[usize::from(selected)] = Some(child);
    }
    let [Some(left), Some(right)] = children else {
        return invalid();
    };
    if left.row_count().checked_add(right.row_count()) != Some(parent.spool.row_count()) {
        return invalid();
    }
    Ok([left, right])
}

async fn copy_result<A, B>(
    spool: &NativeResultSpool,
    source: &mut SpillFile<A>,
    destination: &mut SpillFile<B>,
    page_bytes: usize,
    work: &mut Work<'_>,
) -> Result<NativeResultSpool>
where
    A: AsyncRead + AsyncWrite + AsyncSeek + Unpin,
    B: AsyncRead + AsyncWrite + AsyncSeek + Unpin,
{
    let mut reader = spool.reader(source);
    let mut writer = destination.paged_writer(work.cx, page_bytes)?;
    while let Some(bytes) = reader.next_row(work.cx).await? {
        work.write(&mut writer, bytes.as_ref()).await?;
    }
    if reader.state() != ScanState::Exhausted {
        return Err(NativeSpoolError::IncompleteCursor.into());
    }
    let mut output = spool.clone();
    output.run = writer.finish(work.cx).await?;
    Ok(output)
}

#[allow(clippy::too_many_arguments)]
async fn execute<A, B, C>(
    mut opened: Opened<'_>,
    cx: &QueryCx,
    source: &mut SpillFile<A>,
    partition: &mut SpillFile<B>,
    destination: &mut SpillFile<C>,
    group_capacity: usize,
    max_partitions: usize,
    run_rows: usize,
    max_runs: usize,
    page_bytes: usize,
    max_row_bytes: usize,
    max_input_rows: u64,
    max_work_units: u64,
    resolver: Option<&(dyn CanonicalScalarResolver + Send + Sync)>,
) -> Result<(NativeAggregateSpool, u64)>
where
    A: AsyncRead + AsyncWrite + AsyncSeek + Unpin,
    B: AsyncRead + AsyncWrite + AsyncSeek + Unpin,
    C: AsyncRead + AsyncWrite + AsyncSeek + Unpin,
{
    if group_capacity == 0
        || max_partitions == 0
        || run_rows == 0
        || max_runs == 0
        || page_bytes == 0
        || page_bytes > 64 * 1024
        || max_row_bytes == 0
    {
        return Err(SpillError::InvalidLimits.into());
    }
    let mut work = Work {
        cx,
        used: 0,
        limit: max_work_units,
    };
    work.charge(1)?;
    let pool = source.memory_pool().clone();
    // Binary depth-first traversal retains at most one pending sibling per
    // hash bit plus the active level. max_partitions caps TOTAL created runs;
    // it must not turn a small query into a huge upfront resident allocation.
    let mut pending = Catalog::<Partition>::new(&pool, cx, max_partitions.min(257))?;
    let initial = drain_input(
        opened.input.as_mut(),
        &opened.definition,
        source,
        page_bytes,
        max_row_bytes,
        max_input_rows,
        &mut work,
    )
    .await?;
    let mut partitions = 1_usize;
    let mut largest_output = 0;
    let mut result_writer = destination.paged_writer(cx, page_bytes)?;
    if initial.row_count() == 0 && opened.definition.group_key_columns().is_empty() {
        let _charge = pool
            .reserve(
                cx,
                opened
                    .definition
                    .state_resident_bytes()
                    .checked_add(fixed_output_bytes(&opened.definition))
                    .ok_or(SpillError::SizeOverflow)?,
            )
            .map_err(SpillError::Memory)?;
        let state = opened
            .definition
            .new_state(&mut |event| opened.input.charge(event))
            .map_err(execute_error)?;
        let row = opened
            .definition
            .finish(Vec::new(), state, &mut |event| opened.input.charge(event))
            .map_err(execute_error)?;
        opened.input.finish_result().map_err(execute_error)?;
        let envelope = envelope(&row);
        let (row, _codec) = account::encode(&pool, &mut work, &envelope, max_row_bytes)?;
        largest_output = row.len();
        work.write(&mut result_writer, &row).await?;
    }
    if initial.row_count() != 0 {
        pending.values.push(Partition {
            spool: initial,
            in_source: true,
            depth: 0,
        });
    }
    while let Some(next) = pending.values.pop() {
        work.charge(1)?;
        let groups = if next.in_source {
            reduce_partition(
                &next.spool,
                source,
                &opened.definition,
                opened.input.as_mut(),
                group_capacity,
                &mut work,
                resolver,
            )
            .await?
        } else {
            reduce_partition(
                &next.spool,
                partition,
                &opened.definition,
                opened.input.as_mut(),
                group_capacity,
                &mut work,
                resolver,
            )
            .await?
        };
        if let Some(mut groups) = groups {
            for group in groups.values.drain(..) {
                work.charge(group.largest)?;
                let keys = opened
                    .definition
                    .group_key_columns()
                    .iter()
                    .map(|&at| group.exemplar.values()[at].clone())
                    .collect();
                let row = opened
                    .definition
                    .finish(keys, group.state, &mut |event| opened.input.charge(event))
                    .map_err(execute_error)?;
                opened.input.finish_result().map_err(execute_error)?;
                let envelope = envelope(&row);
                let (row, _codec) = account::encode(&pool, &mut work, &envelope, max_row_bytes)?;
                largest_output = largest_output.max(row.len());
                work.write(&mut result_writer, &row).await?;
            }
        } else {
            let required = partitions.checked_add(2).ok_or(SpillError::SizeOverflow)?;
            if required > max_partitions {
                return Err(NativeAggregateSpoolError::PartitionLimit {
                    required,
                    limit: max_partitions,
                });
            }
            let children = if next.in_source {
                split_partition(
                    &next,
                    source,
                    partition,
                    &opened.definition,
                    page_bytes,
                    &mut work,
                    resolver,
                )
                .await?
            } else {
                split_partition(
                    &next,
                    partition,
                    source,
                    &opened.definition,
                    page_bytes,
                    &mut work,
                    resolver,
                )
                .await?
            };
            partitions = required;
            for child in children {
                if child.row_count() != 0 {
                    pending.values.push(Partition {
                        spool: child,
                        in_source: !next.in_source,
                        depth: next.depth + 1,
                    });
                }
            }
        }
    }
    drop(pending);
    let run = result_writer.finish(cx).await?;
    let mut spool = NativeResultSpool {
        columns: Arc::from([]),
        encoded_columns: opened.definition.key_columns().len()
            + opened.definition.aggregate_columns().len(),
        snapshot: opened.input.snapshot_seq(),
        kind: opened.input.kind(),
        rows: opened.input.row_stats(),
        evaluator: opened.input.evaluator_stats(),
        max_row_bytes: largest_output,
        run,
    };
    drop(opened.input);
    if !opened.definition.group_key_columns().is_empty() && spool.row_count() > 1 {
        let order: Vec<_> = (0..opened.definition.key_columns().len())
            .map(|at| GraphValueOrder::ascending(at).with_nulls_first(true))
            .collect();
        let (sorted, used) = spool
            .sort_into(
                cx,
                destination,
                source,
                &order,
                run_rows,
                max_runs,
                page_bytes,
                work.limit
                    .checked_sub(work.used)
                    .ok_or(SpillError::SizeOverflow)?,
            )
            .await?;
        work.used = work
            .used
            .checked_add(used)
            .ok_or(SpillError::SizeOverflow)?;
        spool = copy_result(&sorted, source, destination, page_bytes, &mut work).await?;
    }
    Ok((
        NativeAggregateSpool {
            inner: spool,
            columns: opened.columns.into(),
            slots: opened.slots.into(),
            keys: opened.definition.key_columns().to_vec().into(),
            aggregates: opened.definition.aggregate_columns().to_vec().into(),
        },
        work.used,
    ))
}
