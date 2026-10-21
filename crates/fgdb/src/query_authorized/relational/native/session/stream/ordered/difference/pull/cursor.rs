//! Pull cursor and opening-future lifecycle.
use super::*;

impl<F> core::fmt::Debug for AuthorizedDifferenceExecution<'_, F> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("AuthorizedDifferenceExecution([REDACTED])")
    }
}
impl<F> AuthorizedDifferenceExecution<'_, F> {
    pub fn columns(&self) -> &[String] {
        &self.columns
    }
    pub fn before(&self) -> CommitSeq {
        self.before
    }
    pub fn after(&self) -> CommitSeq {
        self.after
    }
}
impl<'q, F> Future for AuthorizedDifferenceExecution<'q, F> {
    type Output = Result<AuthorizedDifferenceCursor<'q, F>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.get_mut().future.as_mut().poll(cx)
    }
}

impl<F> core::fmt::Debug for AuthorizedDifferenceCursor<'_, F> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AuthorizedDifferenceCursor")
            .field("before", &self.before)
            .field("after", &self.after)
            .field("state", &self.state)
            .field("authority_rows_and_scratch", &"[REDACTED]")
            .finish()
    }
}
impl<F> AuthorizedDifferenceCursor<'_, F> {
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
    pub fn close(&mut self) {
        if self.state == ScanState::Open {
            self.state = ScanState::Closed;
            if let Some(guard) = &mut self.guard {
                guard.armed = false;
            }
        }
        if let Some(inner) = &mut self.inner {
            inner.close();
        }
        self.inner.take();
        self.control.take();
        self.guard.take();
    }

    // Preserve the callback facade's live-check and unwind boundary while it
    // drains this same pull cursor. Pending callbacks do not hold the guard
    // armed between polls; panic unwind does.
    pub(in super::super) async fn checked_callback<T>(
        &mut self,
        future: impl Future<Output = Result<T>>,
    ) -> Result<T> {
        let guard = self
            .guard
            .as_mut()
            .expect("open difference cursor guards its pin");
        let control = self
            .control
            .as_mut()
            .expect("open difference cursor owns its permit");
        checked_future(guard, || control(Control::Checkpoint), future).await
    }
}
impl<F: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send> AuthorizedDifferenceCursor<'_, F> {
    /// Pull one exact nonzero `(after - before)` tuple. A successful row already
    /// passed both native changed-row admission and the signed capability's row
    /// admission. Multiplicity is never expanded into repeated frames.
    ///
    /// `Ok(None)` certifies completeness only when `state()` is Exhausted. After
    /// any error or dropped in-flight pull, later calls return `Ok(None)` with
    /// Failed state and never reinterpret the prefix as complete.
    pub async fn next_change(&mut self) -> Result<Option<(i128, TrackedBytes)>> {
        if self.state != ScanState::Open {
            return Ok(None);
        }
        self.state = ScanState::Failed;
        let mut inner = self
            .inner
            .take()
            .expect("open difference cursor owns its merge");
        let mut control = self
            .control
            .take()
            .expect("open difference cursor owns its permit");
        let guard = self
            .guard
            .as_mut()
            .expect("open difference cursor guards its pin");
        let outcome = checked_future(guard, || control(Control::Checkpoint), async {
            inner.next_change().await.map_err(difference_error)
        })
        .await;
        let outcome = match outcome {
            Ok(change @ Some(_)) => {
                guard.armed = true;
                control(Control::Deliver(1))
                    .map(|()| change)
                    .map_err(authorized)
            }
            other => other,
        };
        match outcome {
            Ok(change) => {
                guard.armed = false;
                if change.is_none() || inner.state() == ScanState::Exhausted {
                    self.state = ScanState::Exhausted;
                    self.guard.take();
                } else {
                    self.state = ScanState::Open;
                    self.inner = Some(inner);
                    self.control = Some(control);
                }
                Ok(change)
            }
            Err(error) => {
                guard.armed = terminal_spool(&error);
                self.guard.take();
                Err(error)
            }
        }
    }
}
