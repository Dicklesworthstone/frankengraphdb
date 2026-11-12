//! Text admission is part of the live read allowance, before compiler work.
use super::*;

const TEXT: &str = "AT BRANCH $route MATCH (n:L {p: $record.key}) RETURN n.p AS p";

fn arguments(padding: &str) -> GqlParameters {
    GqlParameters::new()
        .with_text("route", "main")
        .unwrap()
        .with_map(
            "record",
            vec![
                ("key".into(), GraphValue::Scalar(CanonicalScalar::Int(7))),
                (
                    "unused".into(),
                    GraphValue::Scalar(CanonicalScalar::ucs_basic_text(padding).unwrap()),
                ),
            ],
        )
        .unwrap()
}

fn work_refusal<T>(result: Result<T, QueryError>) {
    assert!(matches!(
        result,
        Err(QueryError::Authorization(Error::LimitExceeded(
            LimitDimension::Work
        )))
    ));
}

#[test]
fn original_text_and_argument_admission_precedes_every_textual_catalog_callback() {
    let ((), report) = run_async_under_lab(0x5ec0_5020, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let db = database(&c.commit()).await;
        let issuer = authority();
        let args = arguments(&"雪".repeat(128));
        let text = format!("/*{}*/ {TEXT}", "雪".repeat(128));
        let mut allowed = grant();
        // The first owner checkpoint has already spent one unit. The complete
        // original input cannot fit, even though field lowering would discard
        // the unused map payload and substitute a tiny scalar argument.
        allowed.limits.max_work = (text.len() + args.canonical_byte_len()) as u64;
        let token = issuer.issue_at(&allowed, 100).unwrap();
        let calls = Cell::new(0);
        let catalog = |kind, name: &str| {
            calls.set(calls.get() + 1);
            symbols(kind, name)
        };
        let mut session = db
            .authorized_read_session(&cx, &issuer, &token, "main", catalog, policy(), || 100)
            .unwrap();
        work_refusal(session.prepare(&cx, &text, &args));
        work_refusal(session.query(&cx, &text, &args));
        work_refusal(session.query_batch(&cx, &[(&text, &args)]));
        assert!(matches!(
            session.query_diff(
                &cx,
                &text,
                &args,
                fgdb_types::CommitSeq(0),
                fgdb_types::CommitSeq(1),
            ),
            Err(GqlQueryError::Interrupted(QueryError::Authorization(
                Error::LimitExceeded(LimitDimension::Work)
            )))
        ));
        assert!(!session.is_closed());
        assert_eq!(calls.get(), 0);

        // The older one-shot facade must reserve the same ingress before its
        // branch parser or trusted resolver, even for an unsupported binding.
        work_refusal(db.query_authorized(
            &cx,
            &issuer,
            &token,
            "main",
            &text,
            &args,
            |kind, name: &str| {
                calls.set(calls.get() + 1);
                symbols(kind, name)
            },
            policy(),
            || 100,
        ));
        assert_eq!(calls.get(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn preparation_reserves_exact_original_utf8_and_argument_bytes_once() {
    let ((), report) = run_async_under_lab(0x5ec0_5021, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let db = database(&c.commit()).await;
        let issuer = authority();
        // The existing owner has six signed checkpoints: entry, before/after
        // compilation, return, and both sides of final handle publication.
        const PREPARATION_BOUNDARIES: u64 = 6;
        for text in [TEXT.to_owned(), format!("/* 雪 */ {TEXT}")] {
            for padding in ["".to_owned(), "雪".repeat(40)] {
                let args = arguments(&padding);
                let ingress = (text.len() + args.canonical_byte_len()) as u64;
                for admitted in [false, true] {
                    let mut allowed = grant();
                    allowed.limits.max_work =
                        ingress + PREPARATION_BOUNDARIES - u64::from(!admitted);
                    let token = issuer.issue_at(&allowed, 100).unwrap();
                    let calls = Cell::new(0);
                    let mut session = db
                        .authorized_read_session(
                            &cx,
                            &issuer,
                            &token,
                            "main",
                            |kind, name: &str| {
                                calls.set(calls.get() + 1);
                                symbols(kind, name)
                            },
                            policy(),
                            || 100,
                        )
                        .unwrap();
                    let result = session.prepare(&cx, &text, &args);
                    if admitted {
                        result.unwrap();
                    } else {
                        // All compiler work may finish; a final allowance
                        // failure must still publish no prepared handle.
                        work_refusal(result);
                    }
                    assert!(calls.get() > 0);
                    assert!(!session.is_closed());
                    session
                        .prepare(&cx, "RETURN 1 AS one", &GqlParameters::new())
                        .unwrap();
                }
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn expired_or_retired_authority_wins_before_text_admission() {
    let ((), report) = run_async_under_lab(0x5ec0_5022, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let db = database(&c.commit()).await;
        let issuer = authority();
        let mut allowed = grant();
        allowed.limits.max_work = 3; // Session admission alone fits exactly.
        let token = issuer.issue_at(&allowed, 100).unwrap();
        let args = arguments(&"雪".repeat(128));
        let now = Cell::new(100);
        let calls = Cell::new(0);
        let mut session = db
            .authorized_read_session(
                &cx,
                &issuer,
                &token,
                "main",
                |kind, name: &str| {
                    calls.set(calls.get() + 1);
                    symbols(kind, name)
                },
                policy(),
                || now.get(),
            )
            .unwrap();
        now.set(1001);
        assert!(matches!(
            session.prepare(&cx, "invalid text", &args),
            Err(QueryError::Authorization(Error::Expired))
        ));
        assert!(session.is_closed());
        assert_eq!(calls.get(), 0);

        now.set(100);
        let mut session = db
            .authorized_read_session(
                &cx,
                &issuer,
                &token,
                "main",
                |_, _: &str| -> Option<GraphSymbol> {
                    panic!("retired authority must precede catalog callbacks")
                },
                policy(),
                || now.get(),
            )
            .unwrap();
        issuer.retire();
        assert!(matches!(
            session.query(&cx, "invalid text", &args),
            Err(QueryError::Authorization(Error::AuthorityRetired))
        ));
        assert!(session.is_closed());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
