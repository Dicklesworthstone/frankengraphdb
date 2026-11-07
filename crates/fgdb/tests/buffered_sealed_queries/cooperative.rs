//! External CALL/Apply suspends in cold projection construction and retained
//! kernels, with unchanged logical admission and no partial publication.

use super::*;
use asupersync::{Budget, runtime::RuntimeBuilder};
use fgdb_types::context::SimulationCheckpointProbe;
use std::future::Future;
use std::num::NonZeroUsize;

async fn counted<F: Future>(future: F) -> (F::Output, usize) {
    let mut future = std::pin::pin!(future);
    let mut pending = 0;
    let result = poll_fn(|task| match future.as_mut().poll(task) {
        Poll::Pending => {
            pending += 1;
            Poll::Pending
        }
        Poll::Ready(result) => Poll::Ready(result),
    })
    .await;
    (result, pending)
}

#[test]
fn cold_projection_quantum_preserves_answers_receipts_and_typed_work_refusals() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let (commit, cx) = (contexts.commit(), contexts.query());
        let (vfs, _writer) = fixture(&commit).await;
        let source_pool = MemoryPool::new(16 * 1024 * 1024, 0).unwrap();
        let spill_pool = MemoryPool::new(1024 * 1024, 0).unwrap();
        let params = parameters();
        for direction in ["directed", "reversed", "undirected"] {
            let text = format!(
                "{} YIELD node,distance RETURN node,distance ORDER BY node",
                BFS.replace("'directed'", &format!("'{direction}'")),
            );
            let prepared = prepare(&text, &params);
            let mut expected = None;
            let mut immediate_polls = 0;
            for steps in [usize::MAX, 1, 2, 7, 64] {
                // Every quantum starts with a newly admitted owner. A cached
                // receipt cannot hide differences in cold construction work.
                let mut cold = view(&commit, &vfs, &source_pool).await;
                let retained = cold.sealed_read_cache_pool().clone();
                assert_eq!(retained.used(), 0);
                let configured = prepared
                    .clone()
                    .with_analytics_quantum(NonZeroUsize::new(steps).unwrap());
                let (answer, polls) = counted(execute(
                    &configured,
                    &mut cold,
                    &cx,
                    policy(),
                    &spill_pool,
                    2,
                    WORK,
                ))
                .await;
                let answer = answer.unwrap();
                let literal = if direction == "reversed" {
                    vec![vec![vertex(1), integer(0)]]
                } else {
                    bfs_rows()
                };
                assert_eq!(answer.rows, literal);
                if let Some(expected) = &expected {
                    assert_eq!(&answer, expected, "{direction}, quantum={steps}");
                } else {
                    expected = Some(answer);
                    immediate_polls = polls;
                }
                if steps == 1 {
                    assert!(polls > immediate_polls, "cold construction must cooperate");
                }
                assert!(retained.used() > 0);
                // Reuse recharges the same complete logical receipt even with
                // a different scheduling quantum and no physical rebuild.
                assert_eq!(
                    execute(&prepared, &mut cold, &cx, policy(), &spill_pool, 2, WORK)
                        .await
                        .unwrap(),
                    *expected.as_ref().unwrap(),
                );
                drop(cold);
                assert_eq!(source_pool.used(), 0);
                assert_eq!(retained.used(), 0);
                assert_eq!(spill_pool.used(), 0);
            }
            if direction != "directed" {
                continue;
            }
            let expected = expected.unwrap();
            for allowance in [
                1,
                7,
                expected.evaluator_work / 2,
                expected.evaluator_work - 1,
            ] {
                let mut refusal = None;
                for steps in [usize::MAX, 1, 7] {
                    let mut cold = view(&commit, &vfs, &source_pool).await;
                    let configured = prepared
                        .clone()
                        .with_analytics_quantum(NonZeroUsize::new(steps).unwrap());
                    let error = execute(
                        &configured,
                        &mut cold,
                        &cx,
                        GqlQueryPolicy::new(10_000, 100, allowance, WORK),
                        &spill_pool,
                        2,
                        WORK,
                    )
                    .await
                    .unwrap_err();
                    if allowance == expected.evaluator_work - 1 {
                        let evaluator = match &error {
                            NativeSpoolError::Procedure(inner) => {
                                matches!(inner.as_ref(), GqlQueryError::Evaluator(_))
                            }
                            NativeSpoolError::BufferedExecute(inner) => {
                                matches!(inner.as_ref(), GqlQueryError::Evaluator(_))
                            }
                            _ => false,
                        };
                        assert!(evaluator, "expected a typed evaluator refusal: {error:?}");
                    }
                    // Preserve the concrete error variant, dimension, limit
                    // and observed count, not just the fact that it failed.
                    let exact = format!("{error:?}");
                    if let Some(refusal) = &refusal {
                        assert_eq!(&exact, refusal, "allowance={allowance}, quantum={steps}");
                    } else {
                        refusal = Some(exact);
                    }
                    drop(cold);
                    assert_eq!(source_pool.used(), 0);
                    assert_eq!(spill_pool.used(), 0);
                }
            }
        }
        assert_eq!(contexts.outstanding_obligations(), 0);
    });
}

#[test]
fn suspended_cold_projection_is_unpublished_and_drop_or_cancel_leaves_reader_usable() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let (commit, cx) = (contexts.commit(), contexts.query());
        let (vfs, _writer) = fixture(&commit).await;
        let source_pool = MemoryPool::new(16 * 1024 * 1024, 0).unwrap();
        let spill_pool = MemoryPool::new(1024 * 1024, 0).unwrap();
        let prepared = prepare(
            &format!("{BFS} YIELD node,distance RETURN node,distance ORDER BY node"),
            &parameters(),
        )
        .with_analytics_quantum(NonZeroUsize::new(1).unwrap());
        for cancel in [false, true] {
            let mut cold = view(&commit, &vfs, &source_pool).await;
            let retained = cold.sealed_read_cache_pool().clone();
            assert_eq!(retained.used(), 0);
            // Physical preparation checkpoints can differ between fresh views.
            // Cancel this request after its own observed suspension instead of
            // guessing an ordinal from the previous invocation.
            let request = runtime.request_cx_with_budget(Budget::INFINITE);
            let request_contexts = PurposeContexts::narrow_runtime_root(&request);
            let probe = Arc::new(SimulationCheckpointProbe::new(None));
            let controlled = request_contexts
                .query()
                .with_checkpoint_probe(Arc::clone(&probe));
            let mut future = Box::pin(execute(
                &prepared,
                &mut cold,
                &controlled,
                policy(),
                &spill_pool,
                2,
                WORK,
            ));
            poll_fn(|task| {
                let first_poll = future.as_mut().poll(task);
                assert!(
                    first_poll.is_pending(),
                    "cancel={cancel}, checkpoints={}, poll={first_poll:?}",
                    probe.calls(),
                );
                Poll::Ready(())
            })
            .await;
            let suspended_after = probe.calls();
            assert!(suspended_after > 0);
            // These files have immediate I/O. Unlike the warm-kernel law,
            // suspension occurs before a completed projection is cached.
            assert_eq!(retained.used(), 0);
            if cancel {
                request.cancel_with(
                    asupersync::types::CancelKind::User,
                    Some("cancel suspended cold projection"),
                );
                assert!(controlled.checkpoint().is_err());
                assert!(cx.checkpoint().is_ok());
                let error = future.await.unwrap_err();
                assert!(
                    matches!(error, NativeSpoolError::Procedure(ref inner)
                    if matches!(inner.as_ref(), GqlQueryError::Interrupted(_))),
                    "{error:?}"
                );
                // Runtime cancellation is observed before another successful
                // checkpoint, rather than after additional projection work.
                assert_eq!(probe.calls(), suspended_after);
            } else {
                drop(future);
            }
            assert_eq!(retained.used(), 0);
            assert_eq!(spill_pool.used(), 0);
            assert_eq!(request_contexts.outstanding_obligations(), 0);
            let answer = execute(&prepared, &mut cold, &cx, policy(), &spill_pool, 2, WORK)
                .await
                .unwrap();
            assert_eq!(answer.rows, bfs_rows());
            assert!(retained.used() > 0);
            drop(cold);
            assert_eq!(source_pool.used(), 0);
            assert_eq!(retained.used(), 0);
            assert_eq!(spill_pool.used(), 0);
        }
        assert_eq!(contexts.outstanding_obligations(), 0);
    });
}

#[test]
fn external_calls_and_repeated_apply_share_kernel_quantum_without_changing_bags_or_admission() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let (commit, cx) = (contexts.commit(), contexts.query());
        let (vfs, _writer) = fixture(&commit).await;
        let source_pool = MemoryPool::new(16 * 1024 * 1024, 0).unwrap();
        let spill_pool = MemoryPool::new(1024 * 1024, 0).unwrap();
        let mut cold = view(&commit, &vfs, &source_pool).await;
        let params = parameters();
        let repeated = "UNWIND [1,1] AS start CALL (start) { \
            CALL fnx.single_source_shortest_path_length(\
            args => [start],label => 'Person',relation => 'R',direction => 'directed',\
            parallel => 'collapse',self_loops => 'drop') \
            YIELD node,distance RETURN node,distance } \
            RETURN node,distance ORDER BY node,distance";
        // This original query has no $start parameter. Keep its unused
        // binding as a strict preparation refusal; the row argument API is
        // still args => [start] under the actual imported frame.
        let before = cold.buffer_stats();
        let error = PreparedNativeRead::prepare(repeated, &params, resolve)
            .unwrap()
            .prepare_buffered_sealed_order(&params, R, memory(), sealing(), RAW)
            .unwrap_err();
        assert!(matches!(error, NativeSpoolError::Prepare(source)
            if matches!(source.as_ref(), fgdb::QueryError::SetText(error)
                if error.offset == repeated.len()
                    && error.kind == fgdb_gql::GraphSetTextErrorKind::Pattern(
                        fgdb_gql::GraphPatternTextErrorKind::UnexpectedArguments))));
        assert_eq!(cold.buffer_stats(), before);
        let no_parameters = GqlParameters::new();
        for (text, copies, bindings) in [
            (
                format!("{BFS} YIELD node,distance RETURN node,distance ORDER BY node,distance"),
                1,
                &params,
            ),
            (repeated.to_owned(), 2, &no_parameters),
        ] {
            let prepared = prepare(&text, bindings);
            let expected = execute(&prepared, &mut cold, &cx, policy(), &spill_pool, 2, WORK)
                .await
                .unwrap();
            let literal: Vec<_> = bfs_rows()
                .into_iter()
                .flat_map(|row| std::iter::repeat_n(row, copies))
                .collect();
            assert_eq!(expected.rows, literal);
            // The exact query projection is warm before counting polls, and
            // these backing files complete every I/O immediately. The small
            // quantum therefore exposes the kernel's actual scheduling path.
            let mut small_yields = 0;
            for steps in [1, 7, 1024, usize::MAX] {
                let configured = prepared
                    .clone()
                    .with_analytics_quantum(NonZeroUsize::new(steps).unwrap());
                let (answer, yields) = counted(execute(
                    &configured,
                    &mut cold,
                    &cx,
                    policy(),
                    &spill_pool,
                    2,
                    WORK,
                ))
                .await;
                assert_eq!(answer.unwrap(), expected, "quantum={steps}");
                if steps == 1 {
                    assert!(yields > 0, "the kernel must yield before publishing rows");
                    small_yields = yields;
                }
                if steps == usize::MAX {
                    assert!(small_yields > yields, "quantum must control actual polling");
                }
                assert_eq!(spill_pool.used(), 0);
            }
        }
        drop(cold);
        assert_eq!(source_pool.used(), 0);
        assert_eq!(contexts.outstanding_obligations(), 0);
    });
}

#[test]
fn external_kernel_suspension_drops_cleanly_and_rechecks_cancellation_before_resuming() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let (commit, cx) = (contexts.commit(), contexts.query());
        let (vfs, _writer) = fixture(&commit).await;
        let source_pool = MemoryPool::new(16 * 1024 * 1024, 0).unwrap();
        let spill_pool = MemoryPool::new(1024 * 1024, 0).unwrap();
        let mut cold = view(&commit, &vfs, &source_pool).await;
        let prepared = prepare(
            &format!("{BFS} YIELD node,distance RETURN node,distance ORDER BY node"),
            &parameters(),
        )
        .with_analytics_quantum(NonZeroUsize::new(1).unwrap());
        let expected = execute(&prepared, &mut cold, &cx, policy(), &spill_pool, 2, WORK)
            .await
            .unwrap();
        assert_eq!(expected.rows, bfs_rows());
        let source_bytes = source_pool.used();
        let retained = cold.sealed_read_cache_pool().clone();
        let retained_bytes = retained.used();
        let probe = Arc::new(SimulationCheckpointProbe::new(None));
        let controlled = cx.with_checkpoint_probe(Arc::clone(&probe));
        let mut future = Box::pin(execute(
            &prepared,
            &mut cold,
            &controlled,
            policy(),
            &spill_pool,
            2,
            WORK,
        ));
        poll_fn(|task| {
            assert!(future.as_mut().poll(task).is_pending());
            Poll::Ready(())
        })
        .await;
        let suspended_after = probe.calls();
        drop(future);
        assert_eq!(spill_pool.used(), 0);
        assert_eq!(source_pool.used(), source_bytes);
        assert_eq!(retained.used(), retained_bytes);

        // Replaying the same warm invocation fails at its first checkpoint
        // after the observed kernel suspension. No row/result handle escapes.
        let probe = Arc::new(SimulationCheckpointProbe::new(Some(suspended_after + 1)));
        let controlled = cx.with_checkpoint_probe(Arc::clone(&probe));
        let mut future = Box::pin(execute(
            &prepared,
            &mut cold,
            &controlled,
            policy(),
            &spill_pool,
            2,
            WORK,
        ));
        poll_fn(|task| {
            assert!(future.as_mut().poll(task).is_pending());
            Poll::Ready(())
        })
        .await;
        assert_eq!(probe.calls(), suspended_after);
        let error = future.await.unwrap_err();
        assert!(
            matches!(error, NativeSpoolError::Procedure(ref error)
                if matches!(error.as_ref(), GqlQueryError::Interrupted(_))),
            "{error:?}"
        );
        assert_eq!(spill_pool.used(), 0);
        assert_eq!(source_pool.used(), source_bytes);
        assert_eq!(retained.used(), retained_bytes);
        assert_eq!(
            execute(&prepared, &mut cold, &cx, policy(), &spill_pool, 2, WORK)
                .await
                .unwrap(),
            expected,
        );
        drop(cold);
        assert_eq!(source_pool.used(), 0);
        assert_eq!(retained.used(), 0);
        assert_eq!(contexts.outstanding_obligations(), 0);
    });
}
