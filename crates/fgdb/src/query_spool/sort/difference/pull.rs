//! One resumable merge owner shared by visitor and pull delivery.

use super::*;

type Change = (i128, TrackedBytes);
type NextChange = VisitResult<Option<Change>>;

/// A backpressured external result difference. Construct with
/// NativeResultSpool::difference_stream and infer its file-backed cursor type.
/// A returned prefix is provisional until state() is Exhausted. Earlier frames
/// remain owned by the consumer after an error; they are not a complete delta.
/// Dropping/closing this cursor performs no I/O, draining or acknowledgement.
pub struct NativeSpoolDifference<'a, B> {
    scratch: &'a mut SpillFile<B>,
    columns: Arc<[String]>,
    before: CommitSeq,
    after: CommitSeq,
    merge: Option<Merge>,
    max_changed_rows: u64,
    changed: u64,
    work: Work<'a>,
    state: ScanState,
}

// Keys and every retained row/page are owned together. next_change takes this
// owner OUT of the cursor before any callback/await. Cancellation or unwind
// therefore drops all traversal state and cannot resume a half-counted group.
struct Merge {
    left: Input,
    right: Input,
    a: Option<TrackedBytes>,
    b: Option<TrackedBytes>,
    keys: ChargedVec<GraphValueOrder>,
    initialized: bool,
    width: usize,
}

impl<'a, B> NativeSpoolDifference<'a, B> {
    pub(super) fn new(
        before: &NativeResultSpool,
        after: &NativeResultSpool,
        scratch: &'a mut SpillFile<B>,
        keys: ChargedVec<GraphValueOrder>,
        max_changed_rows: u64,
        work: Work<'a>,
    ) -> Self {
        let input = |spool: &NativeResultSpool| {
            Input::new(
                Run {
                    pages: spool.run.clone(),
                    rows: spool.row_count(),
                },
                spool.max_row_bytes,
            )
        };
        Self {
            scratch,
            columns: Arc::clone(&before.columns),
            before: before.snapshot_seq(),
            after: after.snapshot_seq(),
            merge: Some(Merge {
                left: input(before),
                right: input(after),
                a: None,
                b: None,
                keys,
                initialized: false,
                width: before.encoded_columns,
            }),
            max_changed_rows,
            changed: 0,
            work,
            state: ScanState::Open,
        }
    }

    pub fn columns(&self) -> &[String] {
        &self.columns
    }
    pub fn before(&self) -> CommitSeq {
        self.before
    }
    pub fn after(&self) -> CommitSeq {
        self.after
    }
    pub fn state(&self) -> ScanState {
        self.state
    }
    pub fn changed_rows(&self) -> u64 {
        self.changed
    }
    /// Successful charges from opening and delivery, including work spent by a
    /// failed or cancelled pull. Query execution costs are not included.
    pub fn work_units(&self) -> u64 {
        self.work.used
    }
    /// Release cached frames/pages and key capacity without reading a suffix.
    /// Terminal state and cumulative counters are never reset by close.
    pub fn close(&mut self) {
        self.merge = None;
        if self.state == ScanState::Open {
            self.state = ScanState::Closed;
        }
    }

    pub(super) fn visit_completed(&mut self) -> VisitResult<()> {
        native(self.work.charge(1))
    }
}

impl<B: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send> NativeSpoolDifference<'_, B> {
    /// Pull one complete nonzero signed group, or certify successful exhaustion.
    /// Errors are returned once. Check state() to distinguish a later fused None
    /// from Exhausted. No new policy is accepted, so paging cannot reset limits.
    pub fn next_change(&mut self) -> crate::SendFuture<'_, NextChange> {
        Box::pin(self.next_inner())
    }

    // The visitor uses the same state machine directly, avoiding a boxed
    // future allocation per tuple when no externally stored pull is needed.
    pub(super) async fn next_inner(&mut self) -> NextChange {
        if self.state != ScanState::Open {
            return Ok(None);
        }
        self.state = ScanState::Failed;
        let mut merge = self
            .merge
            .take()
            .ok_or(GqlQueryError::Source(NativeSpoolError::IncompleteCursor))?;
        let result = merge.advance(self.scratch, &mut self.work).await?;
        if result.is_some() {
            let next = self
                .changed
                .checked_add(1)
                .ok_or(GqlQueryError::Source(SpillError::SizeOverflow.into()))?;
            GqlExecutionBudget::result_rows(self.max_changed_rows)
                .check(GqlBudgetDimension::ResultRows, next)
                .map_err(GqlQueryError::Rows)?;
            native(self.work.charge(1))?;
            // All fallible work is finished. Restore the exact continuation
            // only when its complete group can leave this call.
            self.changed = next;
            self.merge = Some(merge);
            self.state = ScanState::Open;
        } else {
            self.state = ScanState::Exhausted;
        }
        Ok(result)
    }
}

impl<B> core::fmt::Debug for NativeSpoolDifference<'_, B> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("NativeSpoolDifference")
            .field("before", &self.before)
            .field("after", &self.after)
            .field("changed_rows", &self.changed)
            .field("state", &self.state)
            .field("schema_and_data", &"[REDACTED]")
            .finish()
    }
}

impl Merge {
    async fn advance<B: AsyncRead + AsyncWrite + AsyncSeek + Unpin>(
        &mut self,
        scratch: &mut SpillFile<B>,
        work: &mut Work<'_>,
    ) -> NextChange {
        if !self.initialized {
            self.a = pull(&mut self.left, scratch, work).await?;
            self.b = pull(&mut self.right, scratch, work).await?;
            self.initialized = true;
        }
        let keys = &self.keys.values;
        while self.a.is_some() || self.b.is_some() {
            native(work.charge(1))?;
            let take_left = match (&self.a, &self.b) {
                (Some(a), Some(b)) => {
                    native(canonical::compare(a.as_ref(), b.as_ref(), keys, self.width, work))?
                        != Ordering::Greater
                }
                (Some(_), None) => true,
                _ => false,
            };
            let row = if take_left { self.a.take() } else { self.b.take() }
                .ok_or(GqlQueryError::Source(NativeSpoolError::IncompleteCursor))?;
            let mut weight = if take_left {
                self.a = pull(&mut self.left, scratch, work).await?;
                -1_i128
            } else {
                self.b = pull(&mut self.right, scratch, work).await?;
                1_i128
            };
            for (head, input, sign) in [
                (&mut self.a, &mut self.left, -1_i128),
                (&mut self.b, &mut self.right, 1_i128),
            ] {
                while let Some(next) = head.as_ref() {
                    native(work.charge(1))?;
                    match native(canonical::compare(next.as_ref(), row.as_ref(), keys, self.width, work))? {
                        Ordering::Less => {
                            return Err(GqlQueryError::Source(SpillError::InvalidRun.into()));
                        }
                        Ordering::Greater => break,
                        Ordering::Equal => {}
                    }
                    weight = weight
                        .checked_add(sign)
                        .ok_or(GqlQueryError::Source(SpillError::SizeOverflow.into()))?;
                    // Release this duplicate BEFORE reading its successor.
                    *head = None;
                    *head = pull(input, scratch, work).await?;
                }
            }
            if weight != 0 {
                return Ok(Some((weight, row)));
            }
        }
        native(work.charge(1))?;
        Ok(None)
    }
}

#[cfg(test)]
#[path = "pull_tests.rs"]
mod tests;
