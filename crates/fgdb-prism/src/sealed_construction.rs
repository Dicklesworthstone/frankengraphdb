//! One projection constructor, driven either immediately or by bounded yields.
//! Scheduling owns only private build progress and never changes graph policy.

use super::*;
use std::future::{Future, Ready, ready};
use std::num::NonZeroUsize;
use std::task::{Context, Poll, Waker};

// A concrete scheduler retains the yield future's conditional Send property.
// An async trait would erase that property at the shared constructor boundary.
pub(super) struct Schedule<Y> {
    fuel: SealedScanBudget,
    quantum: Option<NonZeroUsize>,
    yield_now: Y,
}

impl Schedule<fn() -> Ready<()>> {
    pub(super) fn immediate() -> Self {
        Self {
            fuel: SealedScanBudget::new(usize::MAX),
            quantum: None,
            yield_now: || ready(()),
        }
    }
}

impl<Y> Schedule<Y> {
    pub(super) fn cooperative(quantum: NonZeroUsize, yield_now: Y) -> Self {
        Self {
            fuel: SealedScanBudget::new(quantum.get()),
            quantum: Some(quantum),
            yield_now,
        }
    }
}

impl<Y, F> Schedule<Y>
where
    Y: FnMut() -> F,
    F: Future<Output = ()>,
{
    pub(super) fn fuel(&mut self) -> &mut SealedScanBudget {
        &mut self.fuel
    }

    pub(super) async fn check(
        &mut self,
        cx: &Control<'_, impl Guard>,
    ) -> Result<(), SealedProjectionError> {
        checkpoint(cx)?;
        if self.quantum.is_some() && !self.fuel.spend() {
            self.refill(cx).await?;
            self.fuel.spend();
        }
        Ok(())
    }

    pub(super) async fn refill(
        &mut self,
        cx: &Control<'_, impl Guard>,
    ) -> Result<(), SealedProjectionError> {
        checkpoint(cx)?;
        let Some(quantum) = self.quantum else {
            self.fuel = SealedScanBudget::new(usize::MAX);
            return Ok(());
        };
        (self.yield_now)().await;
        // Both the original query context and the live caller guard are checked
        // before touching the source or unpublished output after suspension.
        checkpoint(cx)?;
        self.fuel = SealedScanBudget::new(quantum.get());
        Ok(())
    }

    pub(super) async fn incoming(
        &mut self,
        cx: &Control<'_, impl Guard>,
        partition: &SealedPartition,
        limits: IncomingIndexLimits,
    ) -> Result<SealedIncomingIndex, SealedProjectionError> {
        let Some(quantum) = self.quantum else {
            return partition.incoming_index_with_checkpoint(cx.query, limits, || cx.guard());
        };
        // The incoming builder has its own quantum. Suspend at both boundaries
        // so directory work cannot accumulate on top of a complete index quantum.
        self.refill(cx).await?;
        let index = partition
            .incoming_index_cooperative_with_checkpoint(
                cx.query,
                limits,
                quantum,
                &mut self.yield_now,
                || cx.guard(),
            )
            .await?;
        self.refill(cx).await?;
        Ok(index)
    }
}

// This driver is used only with the immediate schedule. There
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
