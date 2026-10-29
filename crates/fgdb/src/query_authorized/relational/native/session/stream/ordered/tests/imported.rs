//! Trusted cached bindings compose with fresh pins and private external sort.
use super::*;

fn retained(
    text: &str,
    params: &GqlParameters,
) -> (Arc<PreparedGraphBranchText>, Arc<PreparedNativeRead>) {
    let selector = Arc::new(PreparedGraphBranchText::prepare(text).unwrap());
    let native = {
        let selected = selector.bind_parameters(params).unwrap();
        PreparedNativeRead::prepare(selected.statement(), selected.parameters(), symbols).unwrap()
    };
    (selector, Arc::new(native))
}

#[test]
fn imported_ordered_templates_rebind_at_a_fresh_pin_without_catalog_callbacks() {
    let ((), report) = run_async_under_lab(0x8600_0010, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let mut db = database(&contexts.commit()).await;
        let issuer = authority();
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let pool = MemoryPool::new(32_768, 0).unwrap();
        let args = |cut, skip| {
            GqlParameters::new()
                .with_text("route", "main")
                .unwrap()
                .with_uint64("cut", cut)
                .unwrap()
                .with_uint64("skip", skip)
                .unwrap()
        };
        let params = args(1, 0);
        let text = concat!(
            "AT BRANCH $route MATCH (n:L) FOR SYSTEM_TIME AS OF SEQ $cut ",
            "RETURN n.p AS p,n.hidden AS hidden ORDER BY p DESC SKIP $skip LIMIT 2;\n"
        );
        let (selector, native) = retained(text, &params);
        let refs = Arc::strong_count(&db.snapshot);
        let mut change = WriteBatch::new(RelationId(1));
        change.set_vertex_property(VId(1), PropertyKeyId(1), Some(CanonicalScalar::Int(30)));
        db.write(&contexts.commit(), change).await.unwrap();
        assert_eq!(Arc::strong_count(&db.snapshot), refs);
        let (scratch, a) = file(&cx, &pool).await;
        let (destination, b) = file(&cx, &pool).await;
        let callbacks = Arc::new(AtomicU64::new(0));
        let observed = Arc::clone(&callbacks);
        let resolver = move |kind, name: &str| {
            observed.fetch_add(1, Ordering::SeqCst);
            symbols(kind, name)
        };
        let (mut session, prepared) = db
            .authorized_prepared_ordered_read_session(
                &cx,
                &issuer,
                &token,
                "main",
                resolver,
                policy(),
                || 100,
                Arc::clone(&selector),
                Arc::clone(&native),
                scratch,
                destination,
                file(&cx, &pool).await.0,
                limits(),
            )
            .unwrap();
        assert!(Arc::ptr_eq(&selector, &prepared.selector));
        assert!(Arc::ptr_eq(&native, &prepared.native));
        assert_eq!(callbacks.load(Ordering::SeqCst), 0);
        assert!(a.0.lock().unwrap().bytes.get_ref().is_empty());
        assert!(b.0.lock().unwrap().bytes.get_ref().is_empty());
        for (cut, skip, expected) in [
            (1, 0, vec![vec![scalar(3), null()], vec![scalar(3), null()]]),
            (
                2,
                0,
                vec![vec![scalar(30), null()], vec![scalar(3), null()]],
            ),
            (2, 1, vec![vec![scalar(3), null()], vec![scalar(2), null()]]),
        ] {
            let future = session
                .execute_ordered(&cx, &prepared, &args(cut, skip))
                .unwrap();
            assert_eq!(future.columns(), &["p", "hidden"]);
            assert_eq!(future.snapshot_seq(), CommitSeq(cut));
            fn require_send<T: Send>(_: &T) {}
            require_send(&future);
            let mut cursor = future.await.unwrap();
            assert_eq!(cursor.snapshot_seq(), CommitSeq(cut));
            assert_eq!(values(&mut cursor).await, expected);
            drop(cursor);
            assert_eq!(pool.used(), 0);
        }
        assert_eq!(callbacks.load(Ordering::SeqCst), 0);
        session.close();
        assert_eq!(Arc::strong_count(&db.snapshot), refs);
        assert_eq!(prepared.parameter_schema().len(), 2);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn ordered_import_admits_authority_before_templates_limits_and_scratch() {
    let ((), report) = run_async_under_lab(0x8600_0011, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let db = database(&contexts.commit()).await;
        let issuer = authority();
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let pool = MemoryPool::new(32_768, 0).unwrap();
        let params = GqlParameters::new();
        let (selector, native) = retained("MATCH (n:L) RETURN n.p AS p ORDER BY p", &params);
        let mismatch = Arc::new(PreparedGraphBranchText::prepare("MATCH (n:L) RETURN n").unwrap());
        let pins = Arc::strong_count(&db.snapshot);
        for (now, selected, expected) in [
            (
                1001,
                Arc::clone(&mismatch),
                Some(AuthorizationError::Expired),
            ),
            (
                100,
                Arc::clone(&mismatch),
                Some(AuthorizationError::WrongAuthority),
            ),
            (100, Arc::clone(&selector), None),
        ] {
            let (scratch, a) = file(&cx, &pool).await;
            let (destination, b) = file(&cx, &pool).await;
            let mut invalid = limits();
            invalid.page_bytes = 0;
            let result = db.authorized_prepared_ordered_read_session(
                &cx,
                &issuer,
                &token,
                "main",
                symbols,
                policy(),
                || now,
                selected,
                Arc::clone(&native),
                scratch,
                destination,
                file(&cx, &pool).await.0,
                invalid,
            );
            if let Some(expected) = expected {
                refusal(result, expected);
            } else {
                assert!(matches!(
                    result,
                    Err(NativeSpoolError::Spill(SpillError::InvalidLimits))
                ));
            }
            assert_eq!(Arc::strong_count(&db.snapshot), pins);
            assert!(a.0.lock().unwrap().bytes.get_ref().is_empty());
            assert!(b.0.lock().unwrap().bytes.get_ref().is_empty());
            assert_eq!(pool.used(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn imported_handles_keep_exact_session_ownership_and_branch_rebinding() {
    let ((), report) = run_async_under_lab(0x8600_0012, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let db = database(&contexts.commit()).await;
        let issuer = authority();
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let pool = MemoryPool::new(32_768, 0).unwrap();
        let params = GqlParameters::new().with_text("route", "main").unwrap();
        let (selector, native) = retained(
            "AT BRANCH $route MATCH (n:L) RETURN n.p AS p ORDER BY p DESC LIMIT 1",
            &params,
        );
        let mut imported = Vec::new();
        for _ in 0..2 {
            let (scratch, _) = file(&cx, &pool).await;
            let (destination, _) = file(&cx, &pool).await;
            imported.push(
                db.authorized_prepared_ordered_read_session(
                    &cx,
                    &issuer,
                    &token,
                    "main",
                    symbols,
                    policy(),
                    || 100,
                    Arc::clone(&selector),
                    Arc::clone(&native),
                    scratch,
                    destination,
                    file(&cx, &pool).await.0,
                    limits(),
                )
                .unwrap(),
            );
        }
        let (mut second, second_handle) = imported.pop().unwrap();
        let (mut first, first_handle) = imported.pop().unwrap();
        refusal(
            second.execute_ordered(&cx, &first_handle, &params),
            AuthorizationError::WrongAuthority,
        );
        let wrong_branch = GqlParameters::new().with_text("route", "other").unwrap();
        refusal(
            second.execute_ordered(&cx, &second_handle, &wrong_branch),
            AuthorizationError::ScopeDenied,
        );
        let mut cursor = second
            .execute_ordered(&cx, &second_handle, &params)
            .unwrap()
            .await
            .unwrap();
        assert_eq!(values(&mut cursor).await, vec![vec![scalar(3)]]);
        drop(cursor);
        first.close();
        refusal(
            first.execute_ordered(&cx, &first_handle, &params),
            AuthorizationError::ExecutionStopped,
        );
        let mut cursor = second
            .execute_ordered(&cx, &second_handle, &params)
            .unwrap()
            .await
            .unwrap();
        assert_eq!(values(&mut cursor).await, vec![vec![scalar(3)]]);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

// Both factories must keep the same native projection meter after importing a
// cached template. Drive real scoped sources and the existing spill pipeline.
mod projected {
    use super::*;
    use fgdb_gql::{GlaLimitDimension, GraphIntegerErrorKind};

    fn clock() -> u64 {
        100
    }

    #[allow(clippy::too_many_arguments)]
    async fn open<'a>(
        db: &Database<MemVfs>,
        cx: &QueryCx,
        issuer: &'a Authority,
        token: &CapabilityToken,
        pool: &MemoryPool,
        text: &str,
        params: &GqlParameters,
        policy: GqlQueryPolicy,
        imported: bool,
    ) -> (Session<'a, fn() -> u64>, AuthorizedPreparedRead) {
        let (scratch, _) = file(cx, pool).await;
        let (destination, _) = file(cx, pool).await;
        let mut physical = limits();
        physical.max_row_bytes = 16_384;
        physical.max_input_rows = 5;
        if imported {
            let (selector, native) = retained(text, params);
            db.authorized_prepared_ordered_read_session(
                cx,
                issuer,
                token,
                "main",
                symbols as Resolver,
                policy,
                clock as fn() -> u64,
                selector,
                native,
                scratch,
                destination,
                file(cx, pool).await.0,
                physical,
            )
            .unwrap()
        } else {
            let mut session = db
                .authorized_ordered_read_session(
                    cx,
                    issuer,
                    token,
                    "main",
                    symbols as Resolver,
                    policy,
                    clock as fn() -> u64,
                    scratch,
                    destination,
                    file(cx, pool).await.0,
                    physical,
                )
                .unwrap();
            let prepared = session.prepare(cx, text, params).unwrap();
            (session, prepared)
        }
    }

    fn usage(
        session: &mut Session<'_, fn() -> u64>,
        cx: &QueryCx,
        prepared: &AuthorizedPreparedRead,
        params: &GqlParameters,
    ) -> GlaExecutionStats {
        let limits = session.limits;
        let mut opened = session
            .session
            .open_ordered(cx, prepared, params, limits)
            .unwrap();
        let OpenedOrderSource::Linear { input, .. } = &mut opened.source else {
            panic!("one-source row pull retains its shared native meter");
        };
        let mut count = 0;
        while let Some(row) = input.0.pull() {
            drop(row.expect("the wide input allowance admits the whole source"));
            count += 1;
        }
        assert_eq!(count, 5);
        let (at, kind, rows, evaluator) = input.0.stats();
        assert_eq!((at, kind), (CommitSeq(1), ScanKind::Vertex));
        assert_eq!((rows.snapshot_records, rows.result_rows), (5, 5));
        assert_eq!(input.0.state(), ScanState::Exhausted);
        assert!(input.0.pull().is_none());
        assert_eq!(input.0.stats().3, evaluator);
        evaluator
    }

    fn native_limit(error: NativeSpoolError, dimension: GlaLimitDimension) {
        assert!(matches!(
            error,
            NativeSpoolError::Authorized(error)
                if matches!(error.as_ref(), QueryError::Stream(GqlQueryError::Evaluator(limit))
                    if limit.dimension == dimension
                        && limit.observed == u128::from(limit.limit) + 1)
        ));
    }

    fn division(error: NativeSpoolError) {
        assert!(matches!(
            error,
            NativeSpoolError::Authorized(error)
                if matches!(error.as_ref(),
                    QueryError::Set(GqlQueryError::Source(GraphSetExecutionError::Projection {
                        row: 1,
                        column: 1,
                        error,
                    })) if error.kind == GraphIntegerErrorKind::DivisionByZero)
        ));
    }

    #[test]
    fn computed_set_quantifiers_sort_before_paging_with_or_without_order_by() {
        let ((), report) = run_async_under_lab(0x8600_0016, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.query();
            let db = database(&contexts.commit()).await;
            let issuer = authority();
            let token = issuer.issue_at(&grant(), 100).unwrap();
            let pool = MemoryPool::new(2_000_000, 0).unwrap();
            let declarations = GqlParameters::new().with_int64("value", 0).unwrap();
            for imported in [false, true] {
                for distinct in [false, true] {
                    for order in ["", "ORDER BY value DESC"] {
                        let pages = [(0, None), (0, Some(0)), (0, Some(2)), (1, Some(3))];
                        for (skip, count) in pages {
                            let quantifier = if distinct { "DISTINCT" } else { "ALL" };
                            let page = count.map_or_else(String::new, |count| {
                                format!("SKIP {skip} LIMIT {count}")
                            });
                            let text = format!(
                                "MATCH (n:L) RETURN {quantifier} $value AS value {order} {page}"
                            );
                            let (mut session, prepared) = open(
                                &db,
                                &cx,
                                &issuer,
                                &token,
                                &pool,
                                &text,
                                &declarations,
                                policy(),
                                imported,
                            )
                            .await;
                            session.limits.run_rows = 1;
                            assert!(matches!(
                                prepared.native.as_ref(),
                                PreparedNativeRead::Set(_)
                            ));
                            for value in [4, -7] {
                                let args = GqlParameters::new().with_int64("value", value).unwrap();
                                // DISTINCT and LIMIT belong to the final result:
                                // all five private occurrences still reach projection.
                                usage(&mut session, &cx, &prepared, &args);
                                let available = if distinct { 1_usize } else { 5_usize };
                                let selected = available.saturating_sub(skip);
                                let selected = count.map_or(selected, |count| selected.min(count));
                                let future =
                                    session.execute_ordered(&cx, &prepared, &args).unwrap();
                                assert_eq!(future.columns(), &["value"]);
                                assert_eq!(future.snapshot_seq(), CommitSeq(1));
                                fn require_send<T: Send>(_: &T) {}
                                require_send(&future);
                                let mut cursor = future.await.unwrap();
                                require_send(&cursor);
                                assert_eq!(
                                    values(&mut cursor).await,
                                    vec![vec![scalar(value)]; selected]
                                );
                                drop(cursor);
                                assert!(!session.is_closed());
                                assert_eq!(pool.used(), 0);
                            }
                        }
                    }
                }
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn distinct_null_projection_obeys_one_final_row_quota_not_input_multiplicity() {
        let ((), report) = run_async_under_lab(0x8600_0017, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.query();
            let db = database(&contexts.commit()).await;
            let issuer = authority();
            let mut capability = grant();
            capability.limits.max_rows = 1;
            let token = issuer.issue_at(&capability, 100).unwrap();
            let pool = MemoryPool::new(2_000_000, 0).unwrap();
            let args = GqlParameters::new();
            for imported in [false, true] {
                let (mut session, prepared) = open(
                    &db,
                    &cx,
                    &issuer,
                    &token,
                    &pool,
                    "MATCH (n:L) RETURN DISTINCT NULL AS value",
                    &args,
                    GqlQueryPolicy::new(5, 1, 1_000_000, 1_000_000),
                    imported,
                )
                .await;
                session.limits.run_rows = 1;
                usage(&mut session, &cx, &prepared, &args);
                let mut cursor = session
                    .execute_ordered(&cx, &prepared, &args)
                    .unwrap()
                    .await
                    .unwrap();
                assert_eq!(values(&mut cursor).await, vec![vec![null()]]);
                drop(cursor);
                assert!(!session.is_closed());
                assert_eq!(pool.used(), 0);
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn distinct_limit_zero_keeps_native_admission_and_late_projection_errors() {
        let ((), report) = run_async_under_lab(0x8600_0018, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.query();
            let db = database(&contexts.commit()).await;
            let issuer = authority();
            let token = issuer.issue_at(&grant(), 100).unwrap();
            let pool = MemoryPool::new(2_000_000, 0).unwrap();
            let args = GqlParameters::new().with_int64("divisor", 1).unwrap();
            for imported in [false, true] {
                let text = concat!(
                    "MATCH (n) RETURN DISTINCT n AS id,",
                    "12 / (n.p - $divisor) AS value LIMIT 0"
                );
                let (mut session, prepared) = open(
                    &db,
                    &cx,
                    &issuer,
                    &token,
                    &pool,
                    text,
                    &args,
                    policy(),
                    imported,
                )
                .await;
                let error = session
                    .execute_ordered(&cx, &prepared, &args)
                    .unwrap()
                    .await
                    .expect_err("DISTINCT and LIMIT 0 must not conceal the second row's error");
                division(error);
                assert!(!session.is_closed());
                assert_eq!(pool.used(), 0);
                let safe = GqlParameters::new().with_int64("divisor", 4).unwrap();
                // A source error abandoned the destination's paged writer.
                // It refunds resident buffers, not the file's poison fence.
                let retry = session
                    .execute_ordered(&cx, &prepared, &safe)
                    .unwrap()
                    .await
                    .expect_err("an abandoned spill writer cannot be reused");
                assert!(matches!(
                    retry,
                    NativeSpoolError::Spill(SpillError::PoisonedFile)
                ));
                assert!(!session.is_closed());
                assert_eq!(pool.used(), 0);
                drop(session);
                let (mut session, prepared) = open(
                    &db,
                    &cx,
                    &issuer,
                    &token,
                    &pool,
                    text,
                    &args,
                    policy(),
                    imported,
                )
                .await;
                let mut cursor = session
                    .execute_ordered(&cx, &prepared, &safe)
                    .unwrap()
                    .await
                    .unwrap();
                assert!(values(&mut cursor).await.is_empty());
                drop(cursor);
                assert_eq!(pool.used(), 0);

                let text = "MATCH (n:L) RETURN DISTINCT n AS id,7 AS value LIMIT 0";
                let empty = GqlParameters::new();
                for dimension in [
                    GlaLimitDimension::WorkUnits,
                    GlaLimitDimension::ScratchEntries,
                ] {
                    let allowance = match dimension {
                        GlaLimitDimension::WorkUnits => GqlQueryPolicy::new(5, 0, 0, 1_000_000),
                        GlaLimitDimension::ScratchEntries => {
                            GqlQueryPolicy::new(5, 0, 1_000_000, 0)
                        }
                    };
                    let (mut denied, prepared) = open(
                        &db, &cx, &issuer, &token, &pool, text, &empty, allowance, imported,
                    )
                    .await;
                    let error = denied
                        .execute_ordered(&cx, &prepared, &empty)
                        .unwrap()
                        .await
                        .expect_err("a zero-result page does not bypass native limits");
                    native_limit(error, dimension);
                    assert!(!denied.is_closed());
                    assert_eq!(pool.used(), 0);
                }
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn ordered_projection_refuses_native_limits_even_beneath_limit_zero() {
        let ((), report) = run_async_under_lab(0x8600_0013, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.query();
            let db = database(&contexts.commit()).await;
            let issuer = authority();
            let token = issuer.issue_at(&grant(), 100).unwrap();
            let pool = MemoryPool::new(2_000_000, 0).unwrap();
            let empty = GqlParameters::new();
            let (mut raw, prepared) = open(
                &db,
                &cx,
                &issuer,
                &token,
                &pool,
                "MATCH (n:L) RETURN n AS id ORDER BY id",
                &empty,
                policy(),
                false,
            )
            .await;
            let source = usage(&mut raw, &cx, &prepared, &empty);
            drop(raw);
            let args = GqlParameters::new()
                .with_text("value", &"ab".repeat(2048))
                .unwrap();
            for imported in [false, true] {
                for count in [0, 1] {
                    let text = format!(
                        "MATCH (n:L) RETURN n AS id,toUpper($value) AS value \
                         ORDER BY value LIMIT {count}"
                    );
                    for (dimension, allowance) in [
                        (
                            GlaLimitDimension::WorkUnits,
                            GqlQueryPolicy::new(100_000, 100, source.work_units, 1_000_000),
                        ),
                        (
                            GlaLimitDimension::ScratchEntries,
                            GqlQueryPolicy::new(100_000, 100, 1_000_000, source.scratch_entries),
                        ),
                    ] {
                        let (mut session, prepared) = open(
                            &db, &cx, &issuer, &token, &pool, &text, &args, allowance, imported,
                        )
                        .await;
                        assert!(matches!(
                            prepared.native.as_ref(),
                            PreparedNativeRead::Set(_)
                        ));
                        // The ENTIRE raw source fits. The first large computed
                        // row must not escape via the signed-only callback.
                        {
                            let limits = session.limits;
                            let mut opened = session
                                .session
                                .open_ordered(&cx, &prepared, &args, limits)
                                .unwrap();
                            let OpenedOrderSource::Linear { input, .. } = &mut opened.source else {
                                panic!("one-source row pull retains its shared native meter");
                            };
                            let Err(error) = input.0.pull().unwrap() else {
                                panic!("projection must spend the native allowance");
                            };
                            native_limit(error, dimension);
                            assert_eq!(input.0.state(), ScanState::Failed);
                            assert_eq!(input.0.stats().2.result_rows, 0);
                            let stats = input.0.stats();
                            assert!(input.0.pull().is_none());
                            assert_eq!(input.0.stats(), stats);
                        }
                        // Opening is still lazy; the same error crosses the
                        // actual spill future before any final row is exposed.
                        let future = session.execute_ordered(&cx, &prepared, &args).unwrap();
                        native_limit(future.await.expect_err("native refusal"), dimension);
                        assert!(!session.is_closed());
                        assert_eq!(pool.used(), 0);
                    }
                }
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn imported_ordered_projection_uses_inclusive_cumulative_input_allowances() {
        let ((), report) = run_async_under_lab(0x8600_0014, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.query();
            let db = database(&contexts.commit()).await;
            let issuer = authority();
            let mut capability = grant();
            capability.limits.max_rows = 2;
            let token = issuer.issue_at(&capability, 100).unwrap();
            let pool = MemoryPool::new(2_000_000, 0).unwrap();
            let text = "MATCH (n:L) RETURN n AS id,n.p * $factor AS value \
                        ORDER BY value DESC SKIP 1 LIMIT 2";
            for imported in [false, true] {
                for (factor, expected) in [
                    (
                        2,
                        vec![
                            vec![GraphValue::Vertex(VId(3)), scalar(6)],
                            vec![GraphValue::Vertex(VId(5)), scalar(4)],
                        ],
                    ),
                    (
                        -2,
                        vec![
                            vec![GraphValue::Vertex(VId(2)), scalar(-2)],
                            vec![GraphValue::Vertex(VId(5)), scalar(-4)],
                        ],
                    ),
                ] {
                    let args = GqlParameters::new().with_int64("factor", factor).unwrap();
                    let (mut wide, prepared) = open(
                        &db,
                        &cx,
                        &issuer,
                        &token,
                        &pool,
                        text,
                        &args,
                        policy(),
                        imported,
                    )
                    .await;
                    let needed = usage(&mut wide, &cx, &prepared, &args);
                    drop(wide);
                    for (work, scratch, denied) in [
                        (needed.work_units, needed.scratch_entries, None),
                        (
                            needed.work_units - 1,
                            needed.scratch_entries,
                            Some(GlaLimitDimension::WorkUnits),
                        ),
                        (
                            needed.work_units,
                            needed.scratch_entries - 1,
                            Some(GlaLimitDimension::ScratchEntries),
                        ),
                    ] {
                        let allowance = GqlQueryPolicy::new(5, 2, work, scratch);
                        let (mut session, prepared) = open(
                            &db, &cx, &issuer, &token, &pool, text, &args, allowance, imported,
                        )
                        .await;
                        let future = session.execute_ordered(&cx, &prepared, &args).unwrap();
                        assert_eq!(future.columns(), &["id", "value"]);
                        assert_eq!(future.snapshot_seq(), CommitSeq(1));
                        fn require_send<T: Send>(_: &T) {}
                        require_send(&future);
                        if let Some(dimension) = denied {
                            native_limit(future.await.expect_err("one unit short"), dimension);
                        } else {
                            let mut cursor = future.await.unwrap();
                            require_send(&cursor);
                            assert_eq!(values(&mut cursor).await, expected);
                            drop(cursor);
                        }
                        assert!(!session.is_closed());
                        assert_eq!(pool.used(), 0);
                    }
                }
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn ordered_projection_errors_fuse_input_without_crediting_the_failed_row() {
        let ((), report) = run_async_under_lab(0x8600_0015, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.query();
            let mut db = database(&contexts.commit()).await;
            let mut hidden = WriteBatch::new(RelationId(1));
            hidden.create_vertex(
                VId(0),
                vec![LabelId(99)],
                vec![(PropertyKeyId(1), CanonicalScalar::Int(1))],
            );
            db.write(&contexts.commit(), hidden).await.unwrap();
            let issuer = authority();
            let token = issuer.issue_at(&grant(), 100).unwrap();
            let pool = MemoryPool::new(2_000_000, 0).unwrap();
            let args = GqlParameters::new().with_int64("divisor", 1).unwrap();
            for imported in [false, true] {
                for count in [0, 1] {
                    let text = format!(
                        "MATCH (n) RETURN n AS id,12 / (n.p - $divisor) AS value \
                         ORDER BY value LIMIT {count}"
                    );
                    let (mut session, prepared) = open(
                        &db,
                        &cx,
                        &issuer,
                        &token,
                        &pool,
                        &text,
                        &args,
                        policy(),
                        imported,
                    )
                    .await;
                    let pins = Arc::strong_count(&db.snapshot);
                    {
                        let limits = session.limits;
                        let mut opened = session
                            .session
                            .open_ordered(&cx, &prepared, &args, limits)
                            .unwrap();
                        let OpenedOrderSource::Linear { input, .. } = &mut opened.source else {
                            panic!("one-source row pull retains its shared native meter");
                        };
                        drop(input.0.pull().unwrap().unwrap());
                        let Err(error) = input.0.pull().unwrap() else {
                            panic!("second row fails");
                        };
                        division(error);
                        assert_eq!(input.0.state(), ScanState::Failed);
                        let stats = input.0.stats();
                        assert_eq!((stats.2.snapshot_records, stats.2.result_rows), (2, 1));
                        assert_eq!(Arc::strong_count(&db.snapshot), pins);
                        for _ in 0..3 {
                            assert!(input.0.pull().is_none());
                            assert_eq!(input.0.stats(), stats);
                        }
                    }
                    let future = session.execute_ordered(&cx, &prepared, &args).unwrap();
                    division(
                        future
                            .await
                            .expect_err("LIMIT cannot conceal an input failure"),
                    );
                    assert!(!session.is_closed());
                    assert_eq!(pool.used(), 0);
                    assert_eq!(Arc::strong_count(&db.snapshot), pins);
                }
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }
}
