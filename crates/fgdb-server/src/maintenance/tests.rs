//! Fixed scheduling and real engine maintenance through the served owner.

use super::*;
use crate::recovery::tests::{WakeCounter, served, vertex};
use crate::{DatabaseStatus, ServerLimits};
use asupersync::Budget;
use asupersync::lab::{LabConfig, LabRunReport, LabRuntime, lab_test_config_from_env};
use asupersync::runtime::RuntimeBuilder;
use asupersync::time::{TimerDriverHandle, VirtualClock};
use asupersync::types::CancelKind;
use fgdb_types::{CommitSeq, PurposeContexts, VId};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Waker};

/// The plain async lab helper does not drive virtual deadlines. Use the
/// foundation's auto-advance runner, retaining root completion, all oracles,
/// replay overrides, the original step budget and automatic failure artifacts.
#[cfg(test)]
fn run_with_elapsed_time<F, Fut>(seed: u64, task: F) -> ((), LabRunReport)
where
    F: FnOnce(Cx) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let config = lab_test_config_from_env(LabConfig::new(seed).with_auto_advance());
    let seed = config.seed;
    let mut lab = LabRuntime::new(config);
    let region = lab.state.create_root_region(Budget::INFINITE);
    let (task_id, mut handle) = lab
        .state
        .create_task(region, Budget::INFINITE, async move {
            let root = Cx::current().expect("the lab installs the task context");
            task(root).await;
        })
        .unwrap();
    lab.scheduler
        .lock()
        .schedule(task_id, Budget::INFINITE.priority);
    let elapsed = lab.run_with_auto_advance();
    let report = lab.report();
    let completion = handle.try_join();
    if !matches!(&completion, Ok(Some(())))
        || !report.lab_test_passed()
        || elapsed.auto_advances == 0
    {
        let name = std::thread::current()
            .name()
            .unwrap_or("timed-maintenance")
            .to_owned();
        let failure = format!(
            "seed {seed}: completion={completion:?}, elapsed={elapsed:?}, report={report:?}"
        );
        let artifact = lab.write_auto_crashpack_for_panic(&name, &report, &failure);
        panic!("{failure}; crashpack={artifact:?}");
    }
    ((), report)
}

#[test]
fn timed_lab_runner_rejects_panicked_tasks_and_zero_elapsed_time() {
    for panic_root in [false, true] {
        let reached = Arc::new(AtomicBool::new(false));
        let observed = Arc::clone(&reached);
        let refused = std::panic::catch_unwind(move || {
            run_with_elapsed_time(
                0x79a0_0022 + u64::from(panic_root),
                move |root| async move {
                    if panic_root {
                        asupersync::time::sleep(root.now(), Duration::from_secs(1)).await;
                    }
                    reached.store(true, Ordering::Relaxed);
                    assert!(!panic_root, "intentional timed lab root failure");
                },
            )
        });
        assert!(
            observed.load(Ordering::Relaxed),
            "the control must reach its intended root outcome"
        );
        assert!(refused.is_err(), "the invalid timed run must be refused");
    }
}

#[test]
fn intervals_and_absolute_deadlines_refuse_zero_and_overflow() {
    assert!(!MaintenanceSchedule::default().is_enabled());
    assert_eq!(
        MaintenanceSchedule::new(None, None).unwrap(),
        MaintenanceSchedule::default()
    );
    assert!(MaintenanceSchedule::new(Some(Duration::ZERO), None).is_err());
    assert!(MaintenanceSchedule::new(None, Some(Duration::ZERO)).is_err());
    assert!(MaintenanceSchedule::new(Some(Duration::from_secs(u64::MAX)), None).is_err());
    let schedule = MaintenanceSchedule::new(Some(Duration::from_nanos(u64::MAX)), None).unwrap();
    assert!(ScheduleState::new(schedule, Time::from_nanos(1)).is_err());
}

#[test]
fn fixed_delay_skips_missed_ticks_and_orders_equal_deadlines() {
    let schedule =
        MaintenanceSchedule::new(Some(Duration::from_secs(10)), Some(Duration::from_secs(10)))
            .unwrap();
    let mut state = ScheduleState::new(schedule, Time::from_secs(7)).unwrap();
    assert_eq!(
        state.next(),
        Some((Operation::Compact, Time::from_secs(17)))
    );
    // A compaction taking twenty seconds does not queue two missed compactions.
    state
        .completed(Operation::Compact, Time::from_secs(37))
        .unwrap();
    assert_eq!(state.next(), Some((Operation::Scrub, Time::from_secs(17))));
    state
        .completed(Operation::Scrub, Time::from_secs(39))
        .unwrap();
    assert_eq!(
        state.next(),
        Some((Operation::Compact, Time::from_secs(47)))
    );
    // A long stall retains at most the one due attempt of each kind.
    state
        .completed(Operation::Compact, Time::from_secs(120))
        .unwrap();
    assert_eq!(state.next(), Some((Operation::Scrub, Time::from_secs(49))));
    state
        .completed(Operation::Scrub, Time::from_secs(120))
        .unwrap();
    assert_eq!(
        state.next(),
        Some((Operation::Compact, Time::from_secs(130)))
    );
}

#[test]
fn idle_wait_is_woken_by_drain_or_direct_context_cancellation() {
    for cancel in [false, true] {
        let clock = Arc::new(VirtualClock::new());
        let timer = TimerDriverHandle::with_virtual_clock(clock);
        let runtime = RuntimeBuilder::current_thread()
            .with_timer_driver(timer.clone())
            .build()
            .unwrap();
        let cx = runtime.request_cx_with_budget(Budget::INFINITE);
        let server = Server::new(&cx, ServerLimits::default()).unwrap();
        let wakes = Arc::new(WakeCounter::default());
        let waker = Waker::from(Arc::clone(&wakes));
        let mut task = Context::from_waker(&waker);
        let mut wait = Box::pin(server.wait_for_maintenance(&cx, Time::from_secs(3_600)));
        assert!(wait.as_mut().poll(&mut task).is_pending());
        assert_eq!(timer.pending_count(), 1);
        let before = wakes.0.load(Ordering::Relaxed);
        if cancel {
            // This Cx is polled directly; no scheduler task or listener can
            // supply the wake on our behalf.
            cx.cancel_fast(CancelKind::User);
        } else {
            server.shutdown().trigger();
        }
        assert!(wakes.0.load(Ordering::Relaxed) > before);
        assert!(matches!(wait.as_mut().poll(&mut task), Poll::Ready(false)));
        drop(wait);
        assert_eq!(
            timer.pending_count(),
            0,
            "retire the idle timer registration"
        );
        assert_eq!(
            cx.now(),
            Time::ZERO,
            "shutdown never waits for its interval"
        );
    }
}

#[test]
fn configured_driver_compacts_and_scrubs_without_advancing_the_frontier() {
    // The intervals need modeled elapsed time, with the original step budget.
    let ((), report) = run_with_elapsed_time(0x79a0_0020, |root| async move {
        let (server, token, path) = served(&root, "periodic-maintenance").await;
        let database = &server.databases["test"];
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        database
            .db
            .write(&root)
            .await
            .unwrap()
            .write(&contexts.commit(), vertex(1))
            .await
            .unwrap();
        let (mut pinned, at) = crate::execute::read_session(&root, database, &token)
            .await
            .unwrap_or_else(|error| panic!("{}", error.message));
        assert_eq!(at, CommitSeq(1));
        let before = std::fs::read(path.join("manifest.root")).unwrap();
        server
            .run_maintenance(&root, "test", MaintenanceSchedule::default(), |_| {
                panic!("disabled maintenance must not run")
            })
            .await
            .unwrap();
        assert!(
            server
                .run_maintenance(&root, "missing", MaintenanceSchedule::default(), |_| {
                    panic!("unknown databases must not run maintenance")
                })
                .await
                .is_err()
        );

        let schedule =
            MaintenanceSchedule::new(Some(Duration::from_secs(2)), Some(Duration::from_secs(2)))
                .unwrap();
        schedule.validate(&root).unwrap();
        let events = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&events);
        let shutdown = server.shutdown();
        let clock = root.clone();
        let started = root.now();
        let mut driver = Box::pin(
            server.run_maintenance(&root, "test", schedule, move |event| {
                let mut events = recorded.lock().unwrap();
                events.push((clock.now(), event));
                if events.len() == 2 {
                    shutdown.trigger();
                }
            }),
        );
        assert!(
            driver
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        assert!(events.lock().unwrap().is_empty());
        assert_eq!(std::fs::read(path.join("manifest.root")).unwrap(), before);
        driver.await.unwrap();
        {
            let events = events.lock().unwrap();
            assert_eq!(events.len(), 2);
            assert!(events[0].0 >= started.saturating_add_nanos(2_000_000_000));
            assert!(matches!(&events[0].1, MaintenanceEvent::Compaction(Ok(()))));
            let MaintenanceEvent::Scrub(Ok(summary)) = &events[1].1 else {
                panic!("expected a successful scrub after compaction: {events:?}");
            };
            assert_eq!(summary.objects, 1);
            assert_eq!(summary.clean.len() + summary.repaired.len(), 1);
            assert!(summary.metadata_objects >= 2);
            assert!(summary.lost.is_empty());
            assert!(summary.block_lost.is_empty());
            assert!(summary.metadata_lost.is_empty());
        }
        assert_ne!(std::fs::read(path.join("manifest.root")).unwrap(), before);
        let (view, generation) = database.db.read_view(&root).await.unwrap();
        assert_eq!(view.frontier(), CommitSeq(1));
        assert!(view.vertex(VId(1)).unwrap().is_some());
        generation.check().unwrap();
        let (answer, snapshot) = pinned
            .query_with_snapshot(
                &contexts.query(),
                "MATCH (n) RETURN n",
                &fgdb_gql::GqlParameters::new(),
            )
            .ok()
            .unwrap();
        assert_eq!(snapshot, CommitSeq(1));
        assert!(matches!(answer, fgdb::QueryResult::Rows { rows, .. } if rows.len() == 1));
        assert_eq!(
            server.database_status(&root, "test"),
            Some(DatabaseStatus::Ready { generation: 1 })
        );
        server.finish_shutdown(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn drain_finishes_an_admitted_operation_and_starts_no_second_operation() {
    let ((), report) = run_with_elapsed_time(0x79a0_0021, |root| async move {
        let (server, _, _) = served(&root, "periodic-maintenance-drain").await;
        let database = &server.databases["test"];
        let held = database.db.write(&root).await.unwrap();
        let schedule =
            MaintenanceSchedule::new(Some(Duration::from_secs(1)), Some(Duration::from_secs(1)))
                .unwrap();
        let events = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&events);
        let mut driver = Box::pin(
            server.run_maintenance(&root, "test", schedule, move |event| {
                recorded.lock().unwrap().push(event);
            }),
        );
        let waker = Waker::noop();
        assert!(
            driver
                .as_mut()
                .poll(&mut Context::from_waker(waker))
                .is_pending()
        );
        asupersync::time::sleep(root.now(), Duration::from_secs(1)).await;
        // The compaction has passed admission and now waits for the writer.
        assert!(
            driver
                .as_mut()
                .poll(&mut Context::from_waker(waker))
                .is_pending()
        );
        server.shutdown().trigger();
        assert!(
            driver
                .as_mut()
                .poll(&mut Context::from_waker(waker))
                .is_pending()
        );
        drop(held);
        driver.await.unwrap();
        {
            let events = events.lock().unwrap();
            assert_eq!(
                events.len(),
                1,
                "the already-due scrub must not start during drain"
            );
            assert!(matches!(&events[0], MaintenanceEvent::Compaction(Ok(()))));
        }
        server.finish_shutdown(&root).await;
        assert_eq!(
            server.database_status(&root, "test"),
            Some(DatabaseStatus::Stopped)
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn caller_budget_wakes_the_wait_without_starting_an_early_masked_tick() {
    for masked in [false, true] {
        let clock = Arc::new(VirtualClock::new());
        let timer = TimerDriverHandle::with_virtual_clock(Arc::clone(&clock));
        let runtime = RuntimeBuilder::current_thread()
            .with_timer_driver(timer.clone())
            .build()
            .unwrap();
        let cx = runtime.request_cx_with_budget(Budget::INFINITE.with_deadline(Time::from_secs(5)));
        let server = Server::new(&cx, ServerLimits::default()).unwrap();
        let wakes = Arc::new(WakeCounter::default());
        let waker = Waker::from(Arc::clone(&wakes));
        let mut task = Context::from_waker(&waker);
        let mut wait = Box::pin(server.wait_for_maintenance(&cx, Time::from_secs(3_600)));
        assert!(wait.as_mut().poll(&mut task).is_pending());
        let before = wakes.0.load(Ordering::Relaxed);
        if masked {
            cx.masked(|| {
                clock.advance_to(Time::from_secs(5));
                assert_eq!(timer.process_timers(), 1);
                assert!(wakes.0.load(Ordering::Relaxed) > before);
                let from_timer = wakes.0.load(Ordering::Relaxed);
                assert!(
                    wait.as_mut().poll(&mut task).is_pending(),
                    "a masked budget alarm must not admit an early maintenance tick"
                );
                assert_eq!(timer.pending_count(), 1);
                assert!(
                    wakes.0.load(Ordering::Relaxed) > from_timer,
                    "queue a follow-up poll after the synchronous mask ends"
                );
            });
        } else {
            clock.advance_to(Time::from_secs(5));
            assert_eq!(timer.process_timers(), 1);
            assert!(wakes.0.load(Ordering::Relaxed) > before);
        }
        assert!(matches!(wait.as_mut().poll(&mut task), Poll::Ready(false)));
        drop(wait);
        assert_eq!(timer.pending_count(), 0);
        assert_eq!(cx.now(), Time::from_secs(5));
        assert!(cx.cancelled_by(CancelKind::Deadline));
    }
}
