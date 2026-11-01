//! One projection constructor, driven either immediately or by bounded yields.
//! Scheduling owns only private build progress and never changes graph policy.

use super::*;
use std::cell::RefCell;
use std::future::{Future, Ready, ready};
use std::num::NonZeroUsize;
use std::task::{Context, Poll, Waker};

/// Own the mutable observer across awaits. A borrowed Control exists only
/// during a synchronous raw-cursor call; no shared RefCell reference crosses
/// a suspension. Construction is therefore Send whenever its callbacks are,
/// without locks, atomics, unsafe code or a second projection algorithm.
pub(super) struct BuildControl<'a, G> {
    query: &'a QueryCx,
    observer: Observer<G>,
}

pub(super) struct Observer<G>(RefCell<G>);

impl<G: FnMut(SealedBuildEvent) -> Result<(), SealedProjectionError>> Guard for Observer<G> {
    fn check(&self) -> Result<(), SealedProjectionError> {
        self.check_event(SealedBuildEvent::Work)
    }

    fn check_event(&self, event: SealedBuildEvent) -> Result<(), SealedProjectionError> {
        (self.0.borrow_mut())(event)
    }
}

impl<'a, G: FnMut(SealedBuildEvent) -> Result<(), SealedProjectionError>> BuildControl<'a, G> {
    pub(super) fn new(query: &'a QueryCx, observer: G) -> Self {
        Self {
            query,
            observer: Observer(RefCell::new(observer)),
        }
    }

    pub(super) fn source(&self) -> Control<'_, Observer<G>> {
        Control::new(self.query, &self.observer)
    }

    fn event(&mut self, event: SealedBuildEvent) -> Result<(), SealedProjectionError> {
        (self.observer.0.get_mut())(event)
    }

    fn checkpoint(&mut self, event: SealedBuildEvent) -> Result<(), SealedProjectionError> {
        self.query.checkpoint().map_err(SealedError::Interrupted)?;
        self.event(event)
    }
}

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
        cx: &mut BuildControl<
            '_,
            impl FnMut(SealedBuildEvent) -> Result<(), SealedProjectionError>,
        >,
    ) -> Result<(), SealedProjectionError> {
        cx.checkpoint(SealedBuildEvent::Work)?;
        if self.quantum.is_some() && !self.fuel.spend() {
            self.refill(cx).await?;
            self.fuel.spend();
        }
        Ok(())
    }

    pub(super) async fn refill(
        &mut self,
        cx: &mut BuildControl<
            '_,
            impl FnMut(SealedBuildEvent) -> Result<(), SealedProjectionError>,
        >,
    ) -> Result<(), SealedProjectionError> {
        let Some(quantum) = self.quantum else {
            cx.checkpoint(SealedBuildEvent::Work)?;
            self.fuel = SealedScanBudget::new(usize::MAX);
            return Ok(());
        };
        cx.checkpoint(SealedBuildEvent::Scheduling)?;
        (self.yield_now)().await;
        // Both the original query context and the live caller guard are checked
        // before touching the source or unpublished output after suspension.
        cx.checkpoint(SealedBuildEvent::Scheduling)?;
        self.fuel = SealedScanBudget::new(quantum.get());
        Ok(())
    }

    pub(super) async fn incoming(
        &mut self,
        cx: &mut BuildControl<
            '_,
            impl FnMut(SealedBuildEvent) -> Result<(), SealedProjectionError>,
        >,
        partition: &SealedPartition,
        limits: IncomingIndexLimits,
    ) -> Result<SealedIncomingIndex, SealedProjectionError> {
        let Some(quantum) = self.quantum else {
            return partition.incoming_index_with_checkpoint(cx.query, limits, || {
                cx.event(SealedBuildEvent::Work)
            });
        };
        // The incoming builder has its own quantum. Suspend at both boundaries
        // so directory work cannot accumulate on top of a complete index quantum.
        self.refill(cx).await?;
        let index = partition
            .incoming_index_cooperative_with_events(
                cx.query,
                limits,
                quantum,
                &mut self.yield_now,
                |event| cx.event(event),
            )
            .await?;
        self.refill(cx).await?;
        Ok(index)
    }
}

// This driver is used only with the immediate schedule. There
// is no executor, blocking wait or poll loop. A mistakenly introduced suspend
// fails closed and drops every unpublished allocation.
pub(super) fn immediately<T>(
    future: impl Future<Output = Result<T, SealedProjectionError>>,
) -> Result<T, SealedProjectionError> {
    let mut future = std::pin::pin!(future);
    match future
        .as_mut()
        .poll(&mut Context::from_waker(Waker::noop()))
    {
        Poll::Ready(result) => result,
        Poll::Pending => Err(SealedError::NonCanonical.into()),
    }
}
