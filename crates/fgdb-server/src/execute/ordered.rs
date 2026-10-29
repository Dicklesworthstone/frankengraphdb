//! External ordering under the same native capability and served-generation
//! owners. Scratch is supplied only by the database's trusted host policy.

use super::*;
use asupersync::fs::File;
use core::future::{Future, poll_fn};
use core::pin::Pin;
use core::task::Poll;
use fgdb::NativeSpoolError;
use fgdb_gql::algebra::GraphValueRow;
use fgdb_gql::scan_stream::ScanState;
use fgdb_strata::tiered::memory::SpillError;
use fgdb_types::QueryCx;
use std::sync::Arc;

pub(crate) struct OrderedSession<'a> {
    native: fgdb::AuthorizedOrderedReadSession<'a, crate::Symbols, fn() -> u64, File, File, File>,
    generation: Generation,
    // File owners drop before the slot is released. Any already-started
    // foundation syscall finishes against its unlinked, never-reused file.
    _lease: crate::ordered::OrderLease,
}

pub(crate) struct OrderedExecution<'q> {
    native: fgdb::AuthorizedOrderExecution<'q, File>,
    generation: Generation,
}

pub(crate) struct OrderedCursor<'q> {
    native: fgdb::AuthorizedSpoolCursor<'q, File>,
    generation: Generation,
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

pub(crate) async fn read_ordered_session<'a>(
    cx: &Cx,
    db: &'a Served,
    token: &CapabilityToken,
    prepared: Option<&PreparedRead>,
    generation: Generation,
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
    let host = db.ordered_reads.as_ref().ok_or_else(|| {
        Refusal::new(
            ErrorCode::Statement,
            "external ordered reads are not enabled for this database",
        )
    })?;
    let files = host.files(cx, &query);
    let mut files = std::pin::pin!(files);
    let admitted_files = poll_fn(|context| {
        let check = |admission: &mut fgdb_warden::ExecutionPermit<'_, fgdb_warden::ReadAccess>| {
            query
                .checkpoint()
                .map_err(|error| Refusal::new(ErrorCode::Cancelled, error.to_string()))?;
            admission
                .checkpoint_at(unix_millis())
                .map_err(|error| Refusal::new(warden_code(error), error.to_string()))
        };
        if let Err(error) = check(&mut admission) {
            return Poll::Ready(Err(error));
        }
        let result = files.as_mut().poll(context);
        if let Err(error) = check(&mut admission) {
            return Poll::Ready(Err(error));
        }
        result
    });
    let (scratch, destination, auxiliary, lease) =
        in_generation(&generation, admitted_files).await?;
    let (view, _) = db.db.read_view(cx).await.map_err(Refusal::from)?;
    generation.check().map_err(Refusal::from)?;
    let result = match prepared {
        Some(prepared) => view
            .authorized_prepared_ordered_read_session(
                &query,
                &db.authority,
                token,
                TRUNK,
                db.symbols.clone(),
                db.query_policy,
                unix_millis as fn() -> u64,
                Arc::clone(&prepared.selector),
                Arc::clone(&prepared.native),
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
                db.symbols.clone(),
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
            native,
            generation,
            _lease: lease,
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
        let result = self.native.prepare(cx, text, parameters);
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
        let result = self.native.execute_ordered(cx, prepared, parameters);
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
        let result = self.native.difference_stream(
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
        self.native.columns()
    }

    pub(crate) fn finish(
        self,
    ) -> Pin<Box<dyn Future<Output = Result<OrderedCursor<'q>, Refusal>> + Send + 'q>> {
        Box::pin(async move {
            let Self { native, generation } = self;
            let native = in_generation(
                &generation,
                async move { native.await.map_err(spool_refusal) },
            )
            .await?;
            Ok(OrderedCursor { native, generation })
        })
    }
}

impl OrderedCursor<'_> {
    pub(crate) fn snapshot_seq(&self) -> u64 {
        self.native.snapshot_seq().0
    }
    pub(crate) fn exhausted(&self) -> bool {
        self.native.state() == ScanState::Exhausted
    }
    pub(crate) async fn next(&mut self) -> Result<Option<Vec<WireValue>>, Refusal> {
        let generation = self.generation.clone();
        in_generation(&generation, async {
            let frame = self.native.next_row().await.map_err(spool_refusal)?;
            frame
                .map(|frame| {
                    GraphValueRow::decode_canonical(frame.as_ref())
                        .map(|row| row.values().iter().map(convert::graph).collect())
                        .map_err(|_| {
                            Refusal::new(ErrorCode::Execution, "invalid canonical ordered row")
                        })
                })
                .transpose()
        })
        .await
    }
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

#[cfg(test)]
#[path = "ordered/tests.rs"]
mod tests;
