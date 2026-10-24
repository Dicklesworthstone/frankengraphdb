//! One projection constructor, driven either immediately or by bounded yields.
//! Scheduling owns only private build progress and never changes graph policy.

use super::*;
use std::future::Future;
use std::num::NonZeroUsize;
use std::task::{Context, Poll, Waker};

pub(super) trait Schedule {
    fn fuel(&mut self) -> &mut SealedScanBudget;
    async fn check(&mut self, cx: &Control<'_>) -> Result<(), SealedProjectionError>;
    async fn refill(&mut self, cx: &Control<'_>) -> Result<(), SealedProjectionError>;
    async fn incoming(
        &mut self,
        cx: &Control<'_>,
        partition: &SealedPartition,
        limits: IncomingIndexLimits,
    ) -> Result<SealedIncomingIndex, SealedProjectionError>;
}

pub(super) struct Immediate {
    fuel: SealedScanBudget,
}

impl Immediate {
    pub(super) fn new() -> Self {
        Self {
            fuel: SealedScanBudget::new(usize::MAX),
        }
    }
}

impl Schedule for Immediate {
    fn fuel(&mut self) -> &mut SealedScanBudget {
        &mut self.fuel
    }

    async fn check(&mut self, cx: &Control<'_>) -> Result<(), SealedProjectionError> {
        checkpoint(cx)
    }

    async fn refill(&mut self, cx: &Control<'_>) -> Result<(), SealedProjectionError> {
        checkpoint(cx)?;
        self.fuel = SealedScanBudget::new(usize::MAX);
        Ok(())
    }

    async fn incoming(
        &mut self,
        cx: &Control<'_>,
        partition: &SealedPartition,
        limits: IncomingIndexLimits,
    ) -> Result<SealedIncomingIndex, SealedProjectionError> {
        partition.incoming_index_with_checkpoint(cx.query, limits, || cx.guard())
    }
}

pub(super) struct Cooperative<Y> {
    fuel: SealedScanBudget,
    quantum: NonZeroUsize,
    yield_now: Y,
}

impl<Y> Cooperative<Y> {
    pub(super) fn new(quantum: NonZeroUsize, yield_now: Y) -> Self {
        Self {
            fuel: SealedScanBudget::new(quantum.get()),
            quantum,
            yield_now,
        }
    }
}

impl<Y, F> Schedule for Cooperative<Y>
where
    Y: FnMut() -> F,
    F: Future<Output = ()>,
{
    fn fuel(&mut self) -> &mut SealedScanBudget {
        &mut self.fuel
    }

    async fn check(&mut self, cx: &Control<'_>) -> Result<(), SealedProjectionError> {
        checkpoint(cx)?;
        if !self.fuel.spend() {
            self.refill(cx).await?;
            self.fuel.spend();
        }
        Ok(())
    }

    async fn refill(&mut self, cx: &Control<'_>) -> Result<(), SealedProjectionError> {
        checkpoint(cx)?;
        (self.yield_now)().await;
        // Both the original query context and the live caller guard are checked
        // before touching the source or unpublished output after suspension.
        checkpoint(cx)?;
        self.fuel = SealedScanBudget::new(self.quantum.get());
        Ok(())
    }

    async fn incoming(
        &mut self,
        cx: &Control<'_>,
        partition: &SealedPartition,
        limits: IncomingIndexLimits,
    ) -> Result<SealedIncomingIndex, SealedProjectionError> {
        // The incoming builder has its own quantum. Suspend at both boundaries
        // so directory work cannot accumulate on top of a complete index quantum.
        self.refill(cx).await?;
        let index = partition
            .incoming_index_cooperative_with_checkpoint(
                cx.query,
                limits,
                self.quantum,
                &mut self.yield_now,
                || cx.guard(),
            )
            .await?;
        self.refill(cx).await?;
        Ok(index)
    }
}

// This driver accepts only the constructor instantiated with Immediate. There
// is no executor, blocking wait or poll loop. A mistakenly introduced suspend
// fails closed and drops every unpublished allocation.
pub(super) fn immediately(
    future: impl Future<Output = Result<SealedGraphView, SealedProjectionError>>,
) -> Result<SealedGraphView, SealedProjectionError> {
    let mut future = std::pin::pin!(future);
    match future
        .as_mut()
        .poll(&mut Context::from_waker(Waker::noop()))
    {
        Poll::Ready(result) => result,
        Poll::Pending => Err(SealedError::NonCanonical.into()),
    }
}
