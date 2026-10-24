//! Operator-selected fixed maintenance intervals for a running server.
//!
//! One caller-owned task drives one database. It never spawns another task,
//! learns a policy, catches up missed ticks, or overlaps its own operations.
//! The engine retains the writer lease, snapshot pins, and recovery decisions.

use crate::{MaintenanceError, Server};
use asupersync::Cx;
use asupersync::types::Time;
use core::future::{Future, poll_fn};
use core::task::Poll;
use std::time::Duration;

/// Fixed delays between completed operations on one served database.
///
/// Both operations are disabled by default. The first operation waits its
/// full configured interval; each subsequent interval starts at completion,
/// including a failed attempt. This is explicit operator configuration, not
/// the plan's adaptive foreground-latency pacing policy.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MaintenanceSchedule {
    intervals: [Option<u64>; 2],
}

/// Invalid maintenance configuration or an unavailable runtime timer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MaintenanceScheduleError(&'static str);

impl core::fmt::Display for MaintenanceScheduleError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.0)
    }
}

impl core::error::Error for MaintenanceScheduleError {}

impl MaintenanceSchedule {
    /// Configure compaction and scrub independently. Intervals must be
    /// positive and fit the runtime's nanosecond clock without truncation.
    pub fn new(
        compact_interval: Option<Duration>,
        scrub_interval: Option<Duration>,
    ) -> Result<Self, MaintenanceScheduleError> {
        let mut intervals = [None; 2];
        for (slot, interval) in intervals.iter_mut().zip([compact_interval, scrub_interval]) {
            if let Some(interval) = interval {
                if interval.is_zero() {
                    return Err(MaintenanceScheduleError(
                        "maintenance intervals must be positive",
                    ));
                }
                *slot = Some(u64::try_from(interval.as_nanos()).map_err(|_| {
                    MaintenanceScheduleError("maintenance interval exceeds the runtime clock")
                })?);
            }
        }
        Ok(Self { intervals })
    }

    #[must_use]
    pub fn is_enabled(self) -> bool {
        self.intervals.iter().any(Option::is_some)
    }

    /// Validate the caller's timer and initial deadlines before spawning a
    /// maintenance child. A disabled schedule needs no timer.
    pub fn validate(self, cx: &Cx) -> Result<(), MaintenanceScheduleError> {
        if self.is_enabled() {
            let timer = cx.timer_driver().ok_or(MaintenanceScheduleError(
                "maintenance needs a runtime timer driver",
            ))?;
            ScheduleState::new(self, timer.now())?;
        }
        Ok(())
    }
}

/// One completed scheduled attempt. A successful scrub can still report lost
/// objects; those losses trigger the same fencing and recovery as host scrub.
#[derive(Debug)]
pub enum MaintenanceEvent {
    Compaction(Result<(), MaintenanceError>),
    Scrub(Result<fgdb::ScrubSummary, MaintenanceError>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Operation {
    Compact,
    Scrub,
}

impl Operation {
    fn index(self) -> usize {
        match self {
            Self::Compact => 0,
            Self::Scrub => 1,
        }
    }
}

struct ScheduleState {
    intervals: [Option<u64>; 2],
    deadlines: [Option<Time>; 2],
}

impl ScheduleState {
    fn new(schedule: MaintenanceSchedule, now: Time) -> Result<Self, MaintenanceScheduleError> {
        let mut state = Self {
            intervals: schedule.intervals,
            deadlines: [None; 2],
        };
        for operation in [Operation::Compact, Operation::Scrub] {
            state.completed(operation, now)?;
        }
        Ok(state)
    }

    fn completed(
        &mut self,
        operation: Operation,
        now: Time,
    ) -> Result<(), MaintenanceScheduleError> {
        let at = operation.index();
        self.deadlines[at] = self.intervals[at]
            .map(|interval| {
                now.as_nanos()
                    .checked_add(interval)
                    .map(Time::from_nanos)
                    .ok_or(MaintenanceScheduleError(
                        "maintenance deadline exceeds the runtime clock",
                    ))
            })
            .transpose()?;
        Ok(())
    }

    fn next(&self) -> Option<(Operation, Time)> {
        [Operation::Compact, Operation::Scrub]
            .into_iter()
            .filter_map(|operation| {
                self.deadlines[operation.index()].map(|deadline| (operation, deadline))
            })
            // Compaction wins a simultaneous deadline. Its next tick is
            // moved forward before selecting the already-due scrub.
            .min_by_key(|(operation, deadline)| (*deadline, operation.index()))
    }
}

impl Server {
    /// Drive fixed maintenance for one database until server drain or caller
    /// cancellation. This future belongs in the host's long-lived region;
    /// the host must retain and join its task, just like a listener task.
    ///
    /// No work runs before the first interval. At most one operation is
    /// admitted at a time, and the next tick is measured from completion.
    /// Drain wakes an idle timer immediately. An already admitted operation
    /// finishes before this future returns; its existing operation and writer
    /// guards keep database recovery/drain ordered throughout the await.
    ///
    /// Each engine or lifecycle outcome is passed to `report`, then the
    /// configured interval is observed before another attempt of that kind.
    /// Failed authoritative recovery is never reset or retried by this driver.
    /// The observer should return promptly and does not run under a writer
    /// lock. A missing database or invalid timer/deadline is a driver error.
    pub async fn run_maintenance(
        &self,
        cx: &Cx,
        name: &str,
        schedule: MaintenanceSchedule,
        mut report: impl FnMut(MaintenanceEvent) + Send,
    ) -> Result<(), MaintenanceScheduleError> {
        if !self.databases.contains_key(name) {
            return Err(MaintenanceScheduleError(
                "maintenance database is not served",
            ));
        }
        schedule.validate(cx)?;
        let mut state = ScheduleState::new(schedule, cx.now())?;
        while let Some((operation, deadline)) = state.next() {
            if !self.wait_for_maintenance(cx, deadline).await {
                break;
            }
            let event = match operation {
                Operation::Compact => {
                    MaintenanceEvent::Compaction(self.compact_database(cx, name).await)
                }
                Operation::Scrub => MaintenanceEvent::Scrub(self.scrub_database(cx, name).await),
            };
            let completed_at = cx.now();
            report(event);
            if self.shutdown.is_triggered() || cx.checkpoint().is_err() {
                break;
            }
            state.completed(operation, completed_at)?;
        }
        Ok(())
    }

    async fn wait_for_maintenance(&self, cx: &Cx, deadline: Time) -> bool {
        let waiter = self.shutdown.waiter();
        // The pinned foundation's cancellable receive owns a cancellation
        // waker even when this future is polled directly by an embedding host.
        // Retain the unsent sender: only context cancellation can complete it.
        let (_sender, mut receiver) = asupersync::channel::oneshot::channel::<()>();
        let mut cancelled = core::pin::pin!(receiver.recv(cx));
        let wake_at = cx
            .budget()
            .deadline
            .map_or(deadline, |limit| limit.min(deadline));
        let mut timer = core::pin::pin!(asupersync::time::sleep_until(wake_at));
        poll_fn(|task| {
            if waiter.poll_triggered(task)
                || cx.checkpoint().is_err()
                || cancelled.as_mut().poll(task).is_ready()
            {
                return Poll::Ready(false);
            }
            // Sleep consults the ambient context when it registers a timer.
            // Bind exactly this caller's driver for each poll, including
            // hosts that poll the future outside a spawned runtime task.
            let _current = Cx::set_current(Some(cx.clone()));
            loop {
                if timer.as_mut().poll(task).is_pending() {
                    return Poll::Pending;
                }
                if self.shutdown.is_triggered() || cx.checkpoint().is_err() {
                    return Poll::Ready(false);
                }
                if cx.now() >= deadline {
                    return Poll::Ready(true);
                }
                // The earlier caller-budget alarm fired while cancellation
                // was masked. It must not start maintenance before its own
                // deadline. Re-arm once at that deadline.
                timer.as_mut().get_mut().reset(deadline);
                // Queue one follow-up poll after this synchronous mask ends.
                // The rearmed deadline prevents repeated wakes if the host
                // keeps masking cancellation.
                task.waker().wake_by_ref();
            }
        })
        .await
    }
}

#[cfg(test)]
mod tests;
