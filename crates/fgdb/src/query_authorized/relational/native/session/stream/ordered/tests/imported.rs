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
