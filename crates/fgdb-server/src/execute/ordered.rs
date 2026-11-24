//! External ordering under the same native capability and served-generation
//! owners. Scratch is supplied only by the database's trusted host policy.

use super::*;
use asupersync::fs::{File, UnixVfs};
use core::future::{Future, poll_fn};
use core::pin::Pin;
use core::task::Poll;
use fgdb::NativeSpoolError;
use fgdb_gql::algebra::GraphValueRow;
use fgdb_gql::scan_stream::ScanState;
use fgdb_strata::tiered::memory::{SpillError, SpillFile};
use fgdb_types::QueryCx;
use std::sync::Arc;

type BufferedSource = fgdb::AuthorizedBufferedReadSource<UnixVfs>;
type NativeOrderSession<'a, Backend = fgdb::EmbeddedReadView> =
    fgdb::AuthorizedOrderedReadSession<'a, crate::Symbols, fn() -> u64, File, File, File, Backend>;
type OrderedFiles = (
    SpillFile<File>,
    SpillFile<File>,
    SpillFile<File>,
    crate::ordered::OrderLease,
);

enum OrderSession<'a> {
    Resident(Box<NativeOrderSession<'a>>),
    Buffered(Box<NativeOrderSession<'a, BufferedSource>>),
}

pub(crate) struct OrderedSession<'a> {
    native: OrderSession<'a>,
    generation: Generation,
    // File owners drop before the slot is released. Any already-started
    // foundation syscall finishes against its unlinked, never-reused file.
    _lease: crate::ordered::OrderLease,
    // Cold graph admission spans sorting, completed spool reads and credit
    // waits. Both leases retire only after the native file/source owners.
    _buffered_lease: Option<crate::buffered::BufferedLease>,
}

pub(crate) struct OrderedExecution<'q> {
    native: OrderExecution<'q>,
    generation: Generation,
}

enum OrderExecution<'q> {
    Rows(fgdb::AuthorizedOrderExecution<'q, File>),
    Aggregate(fgdb::AuthorizedAggregateOrderExecution<'q, File>),
    BufferedRows(fgdb::AuthorizedOrderExecution<'q, File, BufferedSource>),
    BufferedAggregate(fgdb::AuthorizedAggregateOrderExecution<'q, File, BufferedSource>),
}

pub(crate) struct OrderedCursor<'q> {
    native: OrderSource<'q>,
    generation: Generation,
    failed: bool,
}

enum OrderSource<'q> {
    Rows(fgdb::AuthorizedSpoolCursor<'q, File>),
    Aggregate(fgdb::AuthorizedAggregateSpoolCursor<'q, File>),
    BufferedRows(fgdb::AuthorizedSpoolCursor<'q, File, BufferedSource>),
    BufferedAggregate(fgdb::AuthorizedAggregateSpoolCursor<'q, File, BufferedSource>),
}

pub(crate) struct DifferenceExecution<'q> {
    native: fgdb::AuthorizedDifferenceExecution<'q, File>,
    generation: Generation,
}

pub(crate) struct DifferenceCursor<'q> {
    native: fgdb::AuthorizedDifferenceCursor<'q, File>,
    generation: Generation,
    failed: bool,
}

/// Admit only for a single nonblocking poll. Recovery never waits for client
/// credit or a Pending filesystem operation, and an old result or error cannot
/// cross the post-poll generation check. The native future separately checks
/// the same live capability permit around every poll and final delivery.
pub(super) async fn in_generation<T>(
    generation: &Generation,
    future: impl Future<Output = Result<T, Refusal>>,
) -> Result<T, Refusal> {
    let mut future = std::pin::pin!(future);
    let mut watcher = generation.watcher();
    poll_fn(|context| {
        while watcher.poll_changed(context) {}
        let _operation = match generation.enter() {
            Ok(operation) => operation,
            Err(error) => return Poll::Ready(Err(error.into())),
        };
        let result = future.as_mut().poll(context);
        if let Err(error) = generation.check() {
            return Poll::Ready(Err(error.into()));
        }
        result
    })
    .await
}

/// Authentication and read rights precede even the generation status. The
/// transport captures this fence before polling scratch admission and retains
/// it through every setup/control race, so recovery cannot reopen that request.
pub(crate) fn ordered_generation(
    db: &Served,
    token: &CapabilityToken,
) -> Result<Generation, Refusal> {
    let now = unix_millis();
    db.authority
        .verify_at(token, TRUNK, now)
        .and_then(|capability| capability.begin_read_at(TRUNK, now).map(|_| ()))
        .map_err(|error| Refusal::new(warden_code(error), error.to_string()))?;
    db.db.generation().map_err(Refusal::from)
}

#[cfg(test)]
pub(crate) async fn read_ordered_session<'a>(
    cx: &Cx,
    db: &'a Served,
    token: &CapabilityToken,
    prepared: Option<&PreparedRead>,
    generation: Generation,
) -> Result<(OrderedSession<'a>, Option<fgdb::AuthorizedPreparedRead>), Refusal> {
    read_ordered_session_at(cx, db, token, prepared, generation, &[]).await
}

pub(crate) async fn read_ordered_session_at<'a>(
    cx: &Cx,
    db: &'a Served,
    token: &CapabilityToken,
    prepared: Option<&PreparedRead>,
    generation: Generation,
    cuts: &[Option<fgdb_types::CommitSeq>],
) -> Result<(OrderedSession<'a>, Option<fgdb::AuthorizedPreparedRead>), Refusal> {
    read_ordered_session_selected(cx, db, token, prepared, generation, cuts, None).await
}

pub(crate) async fn read_ordered_session_for<'a>(
    cx: &Cx,
    db: &'a Served,
    token: &CapabilityToken,
    prepared: Option<&PreparedRead>,
    generation: Generation,
    statement: (&str, &fgdb_gql::GqlParameters),
) -> Result<(OrderedSession<'a>, Option<fgdb::AuthorizedPreparedRead>), Refusal> {
    if db.buffered_reads.is_some() {
        return read_buffered_ordered_session_for(cx, db, token, prepared, generation, statement)
            .await;
    }
    read_ordered_session_selected(cx, db, token, prepared, generation, &[], Some(statement)).await
}

/// The configured cold route consumes the same publication/catalog capture
/// as ReadStream and the same private scratch admission as resident ordering.
/// Difference keeps its separate resident multi-cut route.
async fn read_buffered_ordered_session_for<'a>(
    cx: &Cx,
    db: &'a Served,
    token: &CapabilityToken,
    prepared: Option<&PreparedRead>,
    generation: Generation,
    (text, parameters): (&str, &fgdb_gql::GqlParameters),
) -> Result<(OrderedSession<'a>, Option<fgdb::AuthorizedPreparedRead>), Refusal> {
    let contexts = PurposeContexts::narrow_runtime_root(cx);
    let query = crate::query_cx(&contexts);
    let now = unix_millis();
    let capability = db
        .authority
        .verify_at(token, TRUNK, now)
        .map_err(|error| Refusal::new(warden_code(error), error.to_string()))?;
    let mut admission = capability
        .begin_read_at(TRUNK, now)
        .map_err(|error| Refusal::new(warden_code(error), error.to_string()))?;
    generation.check().map_err(Refusal::from)?;
    if let Some(prepared) = prepared {
        prepared.generation.check().map_err(Refusal::from)?;
    }
    let host = db.ordered_reads.as_ref().ok_or_else(|| {
        Refusal::new(
            ErrorCode::Statement,
            "external ordered reads are not enabled for this database",
        )
    })?;
    let captured = super::buffered::capture_prepared_publication_for(
        cx,
        db,
        token,
        prepared,
        &generation,
        text,
        parameters,
    )
    .await?;
    let (scratch, destination, auxiliary, lease) =
        admitted_files(cx, &query, host, &generation, &mut admission).await?;
    let super::buffered::PreparedBufferedPublication {
        publication,
        selected,
        definition,
        lease: buffered_lease,
    } = captured;
    let (native, prepared) = in_generation(&generation, async {
        publication
            .into_authorized_prepared_ordered_read_session_with_resolver(
                &contexts.commit(),
                &query,
                &db.authority,
                token,
                TRUNK,
                selected,
                db.query_policy,
                unix_millis as fn() -> u64,
                |catalog| super::buffered::resolve_publication_catalog(db, &definition, &catalog),
                Arc::clone(&definition.selector),
                Arc::clone(&definition.native),
                definition.lowered.clone(),
                scratch,
                destination,
                auxiliary,
                host.limits(),
            )
            .await
            .map_err(spool_refusal)
    })
    .await?;
    Ok((
        OrderedSession {
            native: OrderSession::Buffered(Box::new(native)),
            generation,
            _lease: lease,
            _buffered_lease: Some(buffered_lease),
        },
        // Raw text was already compiled against the captured catalog too.
        // Reuse that immutable template instead of preparing it a second time.
        Some(prepared),
    ))
}

async fn admitted_files(
    cx: &Cx,
    query: &QueryCx,
    host: &crate::ordered::OrderedReads,
    generation: &Generation,
    admission: &mut fgdb_warden::ExecutionPermit<'_, fgdb_warden::ReadAccess>,
) -> Result<OrderedFiles, Refusal> {
    let files = host.files(cx, query);
    let mut files = std::pin::pin!(files);
    let admitted = poll_fn(|context| {
        let check = |admission: &mut fgdb_warden::ExecutionPermit<'_, fgdb_warden::ReadAccess>| {
            query
                .checkpoint()
                .map_err(|error| Refusal::new(ErrorCode::Cancelled, error.to_string()))?;
            admission
                .checkpoint_at(unix_millis())
                .map_err(|error| Refusal::new(warden_code(error), error.to_string()))
        };
        if let Err(error) = check(admission) {
            return Poll::Ready(Err(error));
        }
        let result = files.as_mut().poll(context);
        if let Err(error) = check(admission) {
            return Poll::Ready(Err(error));
        }
        result
    });
    in_generation(generation, admitted).await
}

async fn read_ordered_session_selected<'a>(
    cx: &Cx,
    db: &'a Served,
    token: &CapabilityToken,
    prepared: Option<&PreparedRead>,
    generation: Generation,
    cuts: &[Option<fgdb_types::CommitSeq>],
    statement: Option<(&str, &fgdb_gql::GqlParameters)>,
) -> Result<(OrderedSession<'a>, Option<fgdb::AuthorizedPreparedRead>), Refusal> {
    let contexts = PurposeContexts::narrow_runtime_root(cx);
    let query = crate::query_cx(&contexts);
    // Authentication wins over disabled host configuration, scratch capacity,
    // filesystem details and stale prepared definitions.
    let now = unix_millis();
    let capability = db
        .authority
        .verify_at(token, TRUNK, now)
        .map_err(|error| Refusal::new(warden_code(error), error.to_string()))?;
    let mut admission = capability
        .begin_read_at(TRUNK, now)
        .map_err(|error| Refusal::new(warden_code(error), error.to_string()))?;
    generation.check().map_err(Refusal::from)?;
    if let Some(prepared) = prepared {
        prepared.generation.check().map_err(Refusal::from)?;
    }
    // Snapshot selection is admitted before lowering or parsing. The native
    // session still owns the complete execution bill after scratch admission.
    let selected_cuts;
    let cuts: &[Option<fgdb_types::CommitSeq>] = if let Some((text, parameters)) = statement {
        let source_bytes = if prepared.is_some() {
            0
        } else {
            text.len() as u64
        };
        admission
            .charge_work_at(
                unix_millis(),
                source_bytes.saturating_add(parameters.canonical_byte_len() as u64),
            )
            .map_err(|error| Refusal::new(warden_code(error), error.to_string()))?;
        query
            .checkpoint()
            .map_err(|error| Refusal::new(ErrorCode::Cancelled, error.to_string()))?;
        let cut = query.with_restriction(|| match prepared {
            Some(prepared) => prepared.catalog_cut(parameters),
            None => statement_catalog_cut(text, parameters),
        });
        admission
            .checkpoint_at(unix_millis())
            .map_err(|error| Refusal::new(warden_code(error), error.to_string()))?;
        query
            .checkpoint()
            .map_err(|error| Refusal::new(ErrorCode::Cancelled, error.to_string()))?;
        generation.check().map_err(Refusal::from)?;
        if let Some(prepared) = prepared {
            prepared.generation.check().map_err(Refusal::from)?;
        }
        selected_cuts = [cut?];
        &selected_cuts
    } else {
        cuts
    };
    let host = db.ordered_reads.as_ref().ok_or_else(|| {
        Refusal::new(
            ErrorCode::Statement,
            "external ordered reads are not enabled for this database",
        )
    })?;
    let (scratch, destination, auxiliary, lease) =
        admitted_files(cx, &query, host, &generation, &mut admission).await?;
    let (view, _) = db.db.read_view(cx).await.map_err(Refusal::from)?;
    generation.check().map_err(Refusal::from)?;
    let symbols = symbols_at(&view, &db.symbols, cuts);
    admission
        .checkpoint_at(unix_millis())
        .map_err(|error| Refusal::new(warden_code(error), error.to_string()))?;
    query
        .checkpoint()
        .map_err(|error| Refusal::new(ErrorCode::Cancelled, error.to_string()))?;
    generation.check().map_err(Refusal::from)?;
    if let Some(prepared) = prepared {
        prepared.generation.check().map_err(Refusal::from)?;
    }
    let symbols = symbols?;
    if prepared.is_some_and(|prepared| !same_symbols(&prepared.symbols, &symbols)) {
        return Err(Refusal::new(
            ErrorCode::Conflict,
            "the prepared statement's symbol catalog changed; prepare it again at the selected snapshot",
        ));
    }
    let result = match prepared {
        Some(prepared) => view
            .authorized_prepared_ordered_read_session(
                &query,
                &db.authority,
                token,
                TRUNK,
                symbols.clone(),
                db.query_policy,
                unix_millis as fn() -> u64,
                Arc::clone(&prepared.selector),
                Arc::clone(&prepared.native),
                prepared.lowered.clone(),
                scratch,
                destination,
                auxiliary,
                host.limits(),
            )
            .map(|(session, template)| (session, Some(template))),
        None => view
            .authorized_ordered_read_session(
                &query,
                &db.authority,
                token,
                TRUNK,
                symbols,
                db.query_policy,
                unix_millis as fn() -> u64,
                scratch,
                destination,
                auxiliary,
                host.limits(),
            )
            .map(|session| (session, None)),
    };
    generation.check().map_err(Refusal::from)?;
    let (native, template) = result.map_err(spool_refusal)?;
    Ok((
        OrderedSession {
            native: OrderSession::Resident(Box::new(native)),
            generation,
            _lease: lease,
            _buffered_lease: None,
        },
        template,
    ))
}

impl OrderedSession<'_> {
    pub(crate) fn generation(&self) -> Generation {
        self.generation.clone()
    }

    pub(crate) fn prepare(
        &mut self,
        cx: &QueryCx,
        text: &str,
        parameters: &fgdb_gql::GqlParameters,
    ) -> Result<fgdb::AuthorizedPreparedRead, Refusal> {
        let _operation = self.generation.enter().map_err(Refusal::from)?;
        let result = match &mut self.native {
            OrderSession::Resident(native) => native.prepare(cx, text, parameters),
            OrderSession::Buffered(native) => native.prepare(cx, text, parameters),
        };
        self.generation.check().map_err(Refusal::from)?;
        result.map_err(query_refusal)
    }

    pub(crate) fn start<'q>(
        &'q mut self,
        cx: &'q QueryCx,
        prepared: &fgdb::AuthorizedPreparedRead,
        parameters: &fgdb_gql::GqlParameters,
    ) -> Result<OrderedExecution<'q>, Refusal> {
        let generation = self.generation.clone();
        let _operation = generation.enter().map_err(Refusal::from)?;
        // Select one admitted compiler before opening a source. Aggregate
        // results retain their exact numeric domain through typed scratch rows.
        let aggregate = matches!(
            prepared.facade_class(),
            fgdb::NativeReadClass::Aggregate
                | fgdb::NativeReadClass::TemporalAggregate
                | fgdb::NativeReadClass::PipelineAggregate
        );
        let result = match (&mut self.native, aggregate) {
            (OrderSession::Resident(native), true) => native
                .execute_ordered_aggregate(cx, prepared, parameters)
                .map(OrderExecution::Aggregate),
            (OrderSession::Resident(native), false) => native
                .execute_ordered(cx, prepared, parameters)
                .map(OrderExecution::Rows),
            (OrderSession::Buffered(native), true) => native
                .execute_ordered_aggregate(cx, prepared, parameters)
                .map(OrderExecution::BufferedAggregate),
            (OrderSession::Buffered(native), false) => native
                .execute_ordered(cx, prepared, parameters)
                .map(OrderExecution::BufferedRows),
        };
        generation.check().map_err(Refusal::from)?;
        Ok(OrderedExecution {
            native: result.map_err(spool_refusal)?,
            generation,
        })
    }

    /// Compare two cuts with the native external-sort difference cursor. The
    /// same scratch lease, scope and generation remain owned until it drops.
    pub(crate) fn start_difference<'q>(
        &'q mut self,
        cx: &'q QueryCx,
        prepared: &fgdb::AuthorizedPreparedRead,
        parameters: &fgdb_gql::GqlParameters,
        before: u64,
        after: u64,
    ) -> Result<DifferenceExecution<'q>, Refusal> {
        let generation = self.generation.clone();
        let _operation = generation.enter().map_err(Refusal::from)?;
        let OrderSession::Resident(native) = &mut self.native else {
            return Err(Refusal::new(
                ErrorCode::Statement,
                "ordered differences require the resident multi-snapshot source",
            ));
        };
        let result = native.difference_stream(
            cx,
            prepared,
            parameters,
            fgdb_types::CommitSeq(before),
            fgdb_types::CommitSeq(after),
        );
        generation.check().map_err(Refusal::from)?;
        Ok(DifferenceExecution {
            native: result.map_err(spool_refusal)?,
            generation,
        })
    }
}

impl<'q> OrderedExecution<'q> {
    pub(crate) fn columns(&self) -> &[String] {
        match &self.native {
            OrderExecution::Rows(execution) => execution.columns(),
            OrderExecution::Aggregate(execution) => execution.columns(),
            OrderExecution::BufferedRows(execution) => execution.columns(),
            OrderExecution::BufferedAggregate(execution) => execution.columns(),
        }
    }

    pub(crate) fn finish(
        self,
    ) -> Pin<Box<dyn Future<Output = Result<OrderedCursor<'q>, Refusal>> + Send + 'q>> {
        Box::pin(async move {
            let Self { native, generation } = self;
            let native = in_generation(&generation, async move {
                match native {
                    OrderExecution::Rows(execution) => execution
                        .await
                        .map(OrderSource::Rows)
                        .map_err(spool_refusal),
                    OrderExecution::Aggregate(execution) => execution
                        .await
                        .map(OrderSource::Aggregate)
                        .map_err(spool_refusal),
                    OrderExecution::BufferedRows(execution) => execution
                        .await
                        .map(OrderSource::BufferedRows)
                        .map_err(spool_refusal),
                    OrderExecution::BufferedAggregate(execution) => execution
                        .await
                        .map(OrderSource::BufferedAggregate)
                        .map_err(spool_refusal),
                }
            })
            .await?;
            Ok(OrderedCursor {
                native,
                generation,
                failed: false,
            })
        })
    }
}

impl OrderedCursor<'_> {
    pub(crate) fn snapshot_seq(&self) -> u64 {
        match &self.native {
            OrderSource::Rows(cursor) => cursor.snapshot_seq().0,
            OrderSource::Aggregate(cursor) => cursor.snapshot_seq().0,
            OrderSource::BufferedRows(cursor) => cursor.snapshot_seq().0,
            OrderSource::BufferedAggregate(cursor) => cursor.snapshot_seq().0,
        }
    }
    pub(crate) fn exhausted(&self) -> bool {
        let state = match &self.native {
            OrderSource::Rows(cursor) => cursor.state(),
            OrderSource::Aggregate(cursor) => cursor.state(),
            OrderSource::BufferedRows(cursor) => cursor.state(),
            OrderSource::BufferedAggregate(cursor) => cursor.state(),
        };
        !self.failed && state == ScanState::Exhausted
    }
    pub(crate) async fn next(&mut self) -> Result<Option<Vec<WireValue>>, Refusal> {
        if self.failed {
            return Ok(None);
        }
        // A dropped pull, generation refusal or decode failure permanently
        // fuses delivery; a later None cannot certify a refused prefix.
        self.failed = true;
        let generation = self.generation.clone();
        let result = in_generation(&generation, async {
            match &mut self.native {
                OrderSource::Rows(cursor) => {
                    let frame = cursor.next_row().await.map_err(spool_refusal)?;
                    frame
                        .map(|frame| decode_ordered_row(frame.as_ref()))
                        .transpose()
                }
                OrderSource::BufferedRows(cursor) => {
                    let frame = cursor.next_row().await.map_err(spool_refusal)?;
                    frame
                        .map(|frame| decode_ordered_row(frame.as_ref()))
                        .transpose()
                }
                OrderSource::Aggregate(cursor) => {
                    let row = cursor.next_row().await.map_err(spool_refusal)?;
                    Ok(row.map(|row| convert_ordered_aggregate(&row, cursor.output_slots())))
                }
                OrderSource::BufferedAggregate(cursor) => {
                    let row = cursor.next_row().await.map_err(spool_refusal)?;
                    Ok(row.map(|row| convert_ordered_aggregate(&row, cursor.output_slots())))
                }
            }
        })
        .await;
        if result.is_ok() {
            self.failed = false;
        }
        result
    }
}

fn decode_ordered_row(frame: &[u8]) -> Result<Vec<WireValue>, Refusal> {
    GraphValueRow::decode_canonical(frame)
        .map(|row| row.values().iter().map(convert::graph).collect())
        .map_err(|_| Refusal::new(ErrorCode::Execution, "invalid canonical ordered row"))
}

fn convert_ordered_aggregate(
    row: &fgdb::NativeAggregateSpoolRow,
    slots: &[fgdb_gql::GraphAggregateTextSlot],
) -> Vec<WireValue> {
    // Output ordinals are validated by the native compiler. Counts, wide sums
    // and rational averages never narrow through an intermediate GraphValue.
    slots
        .iter()
        .map(|slot| match *slot {
            fgdb_gql::GraphAggregateTextSlot::GroupKey(at) => convert::graph(&row.keys()[at]),
            fgdb_gql::GraphAggregateTextSlot::Aggregate(at) => convert::cell(&row.values()[at]),
        })
        .collect()
}

impl<'q> DifferenceExecution<'q> {
    pub(crate) fn columns(&self) -> &[String] {
        self.native.columns()
    }

    pub(crate) fn finish(
        self,
    ) -> Pin<Box<dyn Future<Output = Result<DifferenceCursor<'q>, Refusal>> + Send + 'q>> {
        Box::pin(async move {
            let Self { native, generation } = self;
            let native = in_generation(
                &generation,
                async move { native.await.map_err(spool_refusal) },
            )
            .await?;
            Ok(DifferenceCursor {
                native,
                generation,
                failed: false,
            })
        })
    }
}

impl DifferenceCursor<'_> {
    pub(crate) fn before(&self) -> u64 {
        self.native.before().0
    }

    pub(crate) fn after(&self) -> u64 {
        self.native.after().0
    }

    pub(crate) fn exhausted(&self) -> bool {
        !self.failed && self.native.state() == ScanState::Exhausted
    }

    pub(crate) async fn next(&mut self) -> Result<Option<(i128, Vec<WireValue>)>, Refusal> {
        if self.failed {
            return Ok(None);
        }
        // Native pulls fuse on dropped or failed I/O. Extend that boundary to
        // generation fencing and canonical decoding as well, so a later None
        // can never turn a refused prefix into a complete difference.
        self.failed = true;
        let generation = self.generation.clone();
        let result = in_generation(&generation, async {
            let change = self.native.next_change().await.map_err(spool_refusal)?;
            change
                .map(|(weight, frame)| {
                    GraphValueRow::decode_canonical(frame.as_ref())
                        .map(|row| (weight, row.values().iter().map(convert::graph).collect()))
                        .map_err(|_| {
                            Refusal::new(ErrorCode::Execution, "invalid canonical difference row")
                        })
                })
                .transpose()
        })
        .await;
        if result.is_ok() {
            self.failed = false;
        }
        result
    }
}

pub(crate) fn spool_refusal(error: impl Into<NativeSpoolError>) -> Refusal {
    let error = match error.into() {
        NativeSpoolError::Authorized(error) | NativeSpoolError::Prepare(error) => {
            return query_refusal(*error);
        }
        NativeSpoolError::Aggregate(error) => return aggregate_spool_refusal(*error),
        NativeSpoolError::BufferedRead(error) => {
            return super::buffered::refusal(fgdb::AuthorizedBufferedError::Read(*error));
        }
        NativeSpoolError::BufferedSession(error) => return super::buffered::refusal(*error),
        error => error,
    };
    let code = match &error {
        NativeSpoolError::SortRunLimit { .. }
        | NativeSpoolError::SortWorkLimit { .. }
        | NativeSpoolError::RowTooLarge { .. }
        | NativeSpoolError::Spill(
            SpillError::FileLimit { .. }
            | SpillError::RunLimit { .. }
            | SpillError::RunTooLarge { .. }
            | SpillError::Memory(_)
            | SpillError::SizeOverflow,
        ) => ErrorCode::Budget,
        NativeSpoolError::Spill(SpillError::Interrupted(_)) => ErrorCode::Cancelled,
        NativeSpoolError::SortOrder(_) | NativeSpoolError::Spill(SpillError::InvalidLimits) => {
            ErrorCode::Statement
        }
        _ => ErrorCode::Execution,
    };
    Refusal::new(code, error.to_string())
}

fn aggregate_spool_refusal(error: fgdb::NativeAggregateSpoolError) -> Refusal {
    use fgdb::NativeAggregateSpoolError;
    let error = match error {
        NativeAggregateSpoolError::Prepare(error) => return query_refusal(*error),
        NativeAggregateSpoolError::Spool(error) => return spool_refusal(error),
        error => error,
    };
    let code = match &error {
        NativeAggregateSpoolError::Execute(error) if gql_budget(error) => ErrorCode::Budget,
        NativeAggregateSpoolError::BufferedExecute(error) if gql_budget(error) => ErrorCode::Budget,
        NativeAggregateSpoolError::Execute(error)
            if matches!(error.as_ref(), GqlQueryError::Interrupted(_)) =>
        {
            ErrorCode::Cancelled
        }
        NativeAggregateSpoolError::BufferedExecute(error)
            if matches!(error.as_ref(), GqlQueryError::Interrupted(_)) =>
        {
            ErrorCode::Cancelled
        }
        NativeAggregateSpoolError::PartitionLimit { .. }
        | NativeAggregateSpoolError::PartitionDepth
        | NativeAggregateSpoolError::InputRows { .. } => ErrorCode::Budget,
        NativeAggregateSpoolError::Unsupported => ErrorCode::Statement,
        _ => ErrorCode::Execution,
    };
    Refusal::new(code, error.to_string())
}

#[cfg(test)]
#[path = "ordered/tests.rs"]
mod tests;
