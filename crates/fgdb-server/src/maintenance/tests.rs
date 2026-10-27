//! Fixed scheduling and real engine maintenance through the served owner.

use super::*;
use crate::recovery::tests::{WakeCounter, served, vertex};
use crate::{DatabaseStatus, ServerLimits};
use asupersync::Budget;
use asupersync::lab::{
    AutoAdvanceTermination, LabConfig, LabRunReport, LabRuntime, lab_test_config_from_env,
};
use asupersync::runtime::RuntimeBuilder;
use asupersync::time::{TimerDriverHandle, VirtualClock};
use asupersync::types::CancelKind;
use core::pin::Pin;
use fgdb_types::{CommitSeq, PurposeContexts, VId};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::task::{Context, Waker};

fn admin_token(db: &crate::Served) -> fgdb_warden::CapabilityToken {
    db.authority
        .issue_at(
            &fgdb_warden::Grant {
                branch: crate::TRUNK.into(),
                labels: fgdb_warden::Scope::All,
                relations: fgdb_warden::Scope::All,
                properties: fgdb_warden::Scope::All,
                rights: fgdb_warden::Rights::Admin,
                limits: fgdb_warden::QueryLimits {
                    max_nodes: 0,
                    max_work: 0,
                    max_rows: 0,
                },
                expires_at_ms: u64::MAX / 2,
            },
            crate::unix_millis(),
        )
        .unwrap()
}

fn maintenance_request(
    name: &str,
    verb: &str,
    token: &fgdb_warden::CapabilityToken,
) -> asupersync::http::h1::types::Request {
    asupersync::http::h1::types::RequestBuilder::new(
        asupersync::http::h1::types::Method::Post,
        format!("/v1/databases/{name}/{verb}"),
    )
    .header(
        "Authorization",
        format!("Bearer {}", fgdb_protocol::json::hex(&token.encode())),
    )
    .build()
}

#[test]
fn http_admin_maintenance_preserves_pinned_readers_and_the_committed_frontier() {
    let ((), report) = asupersync::lab::run_async_under_lab(0x79a0_0024, |root| async move {
        let (server, reader, path) = served(&root, "http-admin-maintenance-pins").await;
        let db = &server.databases["test"];
        let admin = admin_token(db);
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        db.db
            .write(&root)
            .await
            .unwrap()
            .write(&contexts.commit(), vertex(1))
            .await
            .unwrap();
        let (mut pinned, at) = crate::execute::read_session(&root, db, &reader)
            .await
            .unwrap_or_else(|error| panic!("{}", error.message));
        assert_eq!(at, CommitSeq(1));
        db.db
            .write(&root)
            .await
            .unwrap()
            .write(&contexts.commit(), vertex(2))
            .await
            .unwrap();
        let before = std::fs::read(path.join("manifest.root")).unwrap();
        let compact = crate::http::respond(
            &root,
            &server,
            maintenance_request("test", "compact", &admin),
            &crate::http::OutputAuthority::new(),
        )
        .await;
        assert_eq!(compact.status, 200, "{compact:?}");
        assert_ne!(std::fs::read(path.join("manifest.root")).unwrap(), before);
        let output = Arc::new(crate::http::OutputAuthority::new());
        let scrub = crate::http::respond(
            &root,
            &server,
            maintenance_request("test", "scrub", &admin),
            &output,
        )
        .await;
        assert_eq!(scrub.status, 200, "{scrub:?}");
        let text = std::str::from_utf8(&scrub.body).unwrap();
        assert!(text.contains("\"objects\":2"), "{text}");
        assert!(text.contains("\"operation\":\"scrub\""), "{text}");
        let (answer, at) = pinned
            .query_with_snapshot(
                &contexts.query(),
                "MATCH (n) RETURN n",
                &fgdb_gql::GqlParameters::new(),
            )
            .ok()
            .unwrap();
        assert_eq!(at, CommitSeq(1));
        assert!(matches!(answer, fgdb::QueryResult::Rows { rows, .. } if rows.len() == 1));
        let (fresh, generation) = db.db.read_view(&root).await.unwrap();
        assert_eq!(fresh.frontier(), CommitSeq(2));
        assert!(fresh.vertex(VId(2)).unwrap().is_some());
        generation.check().unwrap();
        assert_eq!(
            db.db
                .write(&root)
                .await
                .unwrap()
                .write(&contexts.commit(), vertex(3))
                .await
                .unwrap(),
            CommitSeq(3)
        );
        // The new route installs the ordinary physical-output fence too:
        // retirement after execution must prevent even the first header byte.
        assert!(db.authority.retire());
        let (io, wire) = crate::recovery::tests::WireIo::new(vec![], false);
        let mut io = crate::http::GuardedIo::new(io, root.clone(), output);
        let sent = asupersync::io::AsyncWrite::poll_write(
            Pin::new(&mut io),
            &mut Context::from_waker(Waker::noop()),
            &scrub.body,
        );
        assert!(
            matches!(sent, Poll::Ready(Err(error)) if error.kind() == std::io::ErrorKind::PermissionDenied)
        );
        assert_eq!(wire.writes.load(Ordering::Relaxed), 0);
        server.join_database_workers(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn http_maintenance_denies_query_or_masked_authority_before_waiting_for_storage() {
    use fgdb_warden::{Restriction, Rights, Scope};
    let ((), report) = asupersync::lab::run_async_under_lab(0x79a0_0025, |root| async move {
        let (server, reader, path) = served(&root, "http-admin-denial").await;
        let db = &server.databases["test"];
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        db.db
            .write(&root)
            .await
            .unwrap()
            .write(&contexts.commit(), vertex(1))
            .await
            .unwrap();
        let admin = admin_token(db);
        let before = std::fs::read(path.join("manifest.root")).unwrap();
        let held = db.db.write(&root).await.unwrap();
        let foreign = fgdb_warden::Authority::new(
            asupersync::security::key::AuthKey::from_seed(799),
            db.authority.namespace(),
            crate::GRAPH_NAME,
            fgdb_delta_types::SchemaEpoch(0),
            99,
        )
        .unwrap();
        let foreign = foreign
            .issue_at(
                &fgdb_warden::Grant::read_only(
                    crate::TRUNK,
                    u64::MAX / 2,
                    fgdb_warden::QueryLimits {
                        max_nodes: 0,
                        max_work: 0,
                        max_rows: 0,
                    },
                ),
                crate::unix_millis(),
            )
            .unwrap();
        let mut denied = vec![reader.clone()];
        for rights in [Rights::None, Rights::Read, Rights::Write, Rights::Admin] {
            denied.push(reader.attenuate(Restriction::Rights(rights)).unwrap());
        }
        for restriction in [
            Restriction::Labels(Scope::only([fgdb_delta_types::LabelId(1)])),
            Restriction::Relations(Scope::only([fgdb_delta_types::RelationId(1)])),
            Restriction::Properties(Scope::only([fgdb_delta_types::PropertyKeyId(1)])),
            Restriction::DenyProperties(std::collections::BTreeSet::from([
                fgdb_delta_types::PropertyKeyId(1),
            ])),
            Restriction::Rights(Rights::ReadWrite),
        ] {
            denied.push(admin.attenuate(restriction).unwrap());
        }
        for verb in ["compact", "scrub"] {
            for token in &denied {
                let output = crate::http::OutputAuthority::new();
                let mut request = Box::pin(crate::http::respond(
                    &root,
                    &server,
                    maintenance_request("test", verb, token),
                    &output,
                ));
                let Poll::Ready(response) = request
                    .as_mut()
                    .poll(&mut Context::from_waker(Waker::noop()))
                else {
                    panic!("unauthorized maintenance must not wait for the held writer");
                };
                assert_eq!(response.status, 403, "{response:?}");
            }
            let unknown = crate::http::respond(
                &root,
                &server,
                maintenance_request("unknown", verb, &admin),
                &crate::http::OutputAuthority::new(),
            )
            .await;
            assert_eq!(unknown.status, 404);
            let foreign = crate::http::respond(
                &root,
                &server,
                maintenance_request("test", verb, &foreign),
                &crate::http::OutputAuthority::new(),
            )
            .await;
            assert_eq!(foreign.status, 404);
            assert_eq!(foreign.body, unknown.body);
            let expired = admin
                .attenuate(Restriction::ExpiresBefore(crate::unix_millis()))
                .unwrap();
            let rejected = crate::http::respond(
                &root,
                &server,
                maintenance_request("test", verb, &expired),
                &crate::http::OutputAuthority::new(),
            )
            .await;
            assert_eq!(rejected.status, 404);
            assert_eq!(rejected.body, unknown.body);
            let mut wrong_method = maintenance_request("test", verb, &admin);
            wrong_method.method = asupersync::http::h1::types::Method::Get;
            assert_eq!(
                crate::http::respond(
                    &root,
                    &server,
                    wrong_method,
                    &crate::http::OutputAuthority::new()
                )
                .await
                .status,
                405
            );
            let mut missing = maintenance_request("test", verb, &admin);
            missing.headers.clear();
            assert_eq!(
                crate::http::respond(
                    &root,
                    &server,
                    missing,
                    &crate::http::OutputAuthority::new()
                )
                .await
                .status,
                401
            );
            let mut malformed = maintenance_request("test", verb, &admin);
            malformed.body = b"{}".to_vec();
            assert_eq!(
                crate::http::respond(
                    &root,
                    &server,
                    malformed,
                    &crate::http::OutputAuthority::new()
                )
                .await
                .status,
                400
            );
        }
        drop(held);
        assert_eq!(std::fs::read(path.join("manifest.root")).unwrap(), before);
        assert_eq!(db.db.status(), DatabaseStatus::Ready { generation: 1 });
        server.join_database_workers(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn queued_http_maintenance_rechecks_issuer_retirement_before_engine_work() {
    let ((), report) = asupersync::lab::run_async_under_lab(0x79a0_0026, |root| async move {
        let (server, _, path) = served(&root, "http-admin-queued-retirement").await;
        let db = &server.databases["test"];
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        db.db
            .write(&root)
            .await
            .unwrap()
            .write(&contexts.commit(), vertex(1))
            .await
            .unwrap();
        let admin = admin_token(db);
        let before = std::fs::read(path.join("manifest.root")).unwrap();
        let held = db.db.write(&root).await.unwrap();
        let output = crate::http::OutputAuthority::new();
        let mut request = Box::pin(crate::http::respond(
            &root,
            &server,
            maintenance_request("test", "compact", &admin),
            &output,
        ));
        assert!(
            request
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        assert!(db.authority.retire());
        drop(held);
        assert_eq!(request.await.status, 403);
        assert_eq!(std::fs::read(path.join("manifest.root")).unwrap(), before);
        assert_eq!(db.db.status(), DatabaseStatus::Ready { generation: 1 });
        server.join_database_workers(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

// The pinned run_async_under_lab helper drives run_until_quiescent, which
// never advances virtual time. Timed cases use the foundation's actual
// auto-advance driver, with the same replay overrides and complete verdict.
fn run_timed_under_lab<F, Fut>(seed: u64, task: F) -> LabRunReport
where
    F: FnOnce(asupersync::Cx) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let config = lab_test_config_from_env(LabConfig::new(seed).with_auto_advance());
    let mut runtime = LabRuntime::new(config);
    let root = runtime.state.create_root_region(Budget::INFINITE);
    let (id, mut handle) = runtime
        .state
        .create_task(root, Budget::INFINITE, async move {
            task(asupersync::Cx::current().expect("lab task has its current Cx")).await;
        })
        .expect("timed lab task spawns");
    runtime
        .scheduler
        .lock()
        .schedule(id, Budget::INFINITE.priority);
    let timed = runtime.run_with_auto_advance();
    let report = runtime.report();
    let output = handle.try_join();
    if !matches!(timed.termination, AutoAdvanceTermination::Quiescent)
        || !report.lab_test_passed()
        || !matches!(output, Ok(Some(())))
    {
        let detail = format!("timed lab failed: {timed:?}; root={output:?}; {report:?}");
        let name = std::thread::current()
            .name()
            .unwrap_or("timed maintenance")
            .to_owned();
        let artifact = runtime.write_auto_crashpack_for_panic(&name, &report, &detail);
        panic!("{detail}; crashpack={artifact:?}");
    }
    report
}

#[test]
fn timed_lab_driver_refuses_a_root_that_never_finishes() {
    assert!(
        std::panic::catch_unwind(|| {
            run_timed_under_lab(0x79a0_0022, |_| core::future::pending());
        })
        .is_err()
    );
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
    let report = run_timed_under_lab(0x79a0_0020, |root| async move {
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
    let report = run_timed_under_lab(0x79a0_0021, |root| async move {
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
