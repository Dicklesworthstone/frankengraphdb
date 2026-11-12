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

fn retained(
    text: &str,
    params: &GqlParameters,
) -> (
    std::sync::Arc<fgdb_gql::PreparedGraphBranchText>,
    std::sync::Arc<fgdb::PreparedNativeRead>,
    std::sync::Arc<fgdb_gql::GqlMapArgumentText>,
) {
    let (lowered, derived) = fgdb_gql::GqlMapArgumentText::lower_fields(text, params).unwrap();
    let selector =
        std::sync::Arc::new(fgdb_gql::PreparedGraphBranchText::prepare(lowered.text()).unwrap());
    let native = {
        let selected = selector.bind_parameters(&derived).unwrap();
        fgdb::PreparedNativeRead::prepare(selected.statement(), selected.parameters(), symbols)
            .unwrap()
    };
    (
        selector,
        std::sync::Arc::new(native),
        std::sync::Arc::new(lowered),
    )
}

fn clock() -> u64 {
    100
}

#[test]
fn prepared_original_arguments_precede_lowering_and_binding_in_every_read_mode() {
    let ((), report) = run_async_under_lab(0x5ec0_5023, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let db = database(&c.commit()).await;
        let issuer = authority();
        let declaration = arguments("");
        let padding = "雪".repeat(1024);
        let oversized = arguments(&padding);
        let malformed = GqlParameters::new()
            .with_text("route", "main")
            .unwrap()
            .with_text("record", &padding)
            .unwrap();
        let mut allowed = grant();
        allowed.limits.max_work = oversized
            .canonical_byte_len()
            .min(malformed.canonical_byte_len()) as u64;
        let token = issuer.issue_at(&allowed, 100).unwrap();
        let text = "AT BRANCH $route MATCH (n:L {p: $record.key}) RETURN n AS node ORDER BY n";
        for imported in [false, true] {
            let (mut session, prepared) = if imported {
                let (selector, native, lowered) = retained(text, &declaration);
                db.authorized_prepared_read_session(
                    &cx,
                    &issuer,
                    &token,
                    "main",
                    symbols,
                    policy(),
                    clock as fn() -> u64,
                    selector,
                    native,
                    Some(lowered),
                )
                .unwrap()
            } else {
                let mut session = db
                    .authorized_read_session(
                        &cx,
                        &issuer,
                        &token,
                        "main",
                        symbols,
                        policy(),
                        clock as fn() -> u64,
                    )
                    .unwrap();
                let prepared = session.prepare(&cx, text, &declaration).unwrap();
                (session, prepared)
            };
            let schema = prepared.parameter_schema();
            assert_eq!(schema.len(), 1);
            assert_eq!(schema[0].name, "record");
            assert_eq!(schema[0].parameter_type, fgdb_gql::GqlParameterType::Map);
            for args in [&oversized, &malformed] {
                // The map's unused payload disappears during lowering; even a
                // malformed map must spend its original input before binding.
                work_refusal(session.execute(&cx, &prepared, args));
                work_refusal(session.execute_batch(&cx, &[(&prepared, args)]));
                work_refusal(session.stream(&cx, &prepared, args));
                work_refusal(session.stream_send(&cx, &prepared, args));
                assert!(matches!(
                    session.execute_diff(
                        &cx,
                        &prepared,
                        args,
                        fgdb_types::CommitSeq(0),
                        fgdb_types::CommitSeq(1),
                    ),
                    Err(GqlQueryError::Interrupted(QueryError::Authorization(
                        Error::LimitExceeded(LimitDimension::Work)
                    )))
                ));
                assert!(!session.is_closed());
            }
            assert_eq!(
                session.execute(&cx, &prepared, &declaration).unwrap(),
                result(&["node"], vec![vec![vertex(1)]])
            );
        }

        // The trusted one-shot native facade has no map recipe, but extra
        // caller arguments still cannot reach binding without admission.
        let declaration = GqlParameters::new().with_int64("key", 7).unwrap();
        let native = fgdb::PreparedNativeRead::prepare(
            "MATCH (n:L {p: $key}) RETURN n",
            &declaration,
            symbols,
        )
        .unwrap();
        let oversized = declaration.with_text("unused", &padding.repeat(2)).unwrap();
        work_refusal(native.execute_authorized(
            &db,
            &cx,
            &issuer,
            &token,
            "main",
            &oversized,
            policy(),
            clock,
        ));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn prepared_input_is_once_per_invocation_and_cumulative_across_a_batch() {
    let ((), report) = run_async_under_lab(0x5ec0_5024, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let db = database(&c.commit()).await;
        let issuer = authority();
        let declaration = arguments("");
        let args = arguments(&"雪".repeat(1024));
        let mut allowed = grant();
        allowed.limits.max_work = 2 * args.canonical_byte_len() as u64 - 1;
        let token = issuer.issue_at(&allowed, 100).unwrap();
        let text = "AT BRANCH $route MATCH (n:L {p: $record.key}) RETURN n AS node ORDER BY n";
        let (selector, native, lowered) = retained(text, &declaration);
        let (mut session, prepared) = db
            .authorized_prepared_read_session(
                &cx,
                &issuer,
                &token,
                "main",
                symbols,
                policy(),
                clock,
                selector,
                native,
                Some(lowered),
            )
            .unwrap();
        assert_eq!(
            session.execute(&cx, &prepared, &args).unwrap(),
            result(&["node"], vec![vec![vertex(1)]])
        );
        let rows = session
            .stream(&cx, &prepared, &args)
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(
            rows,
            vec![fgdb_gql::algebra::GraphValueRow::from_owned_values(vec![
                GraphValue::Vertex(VId(1))
            ])]
        );
        // Both cuts share one original argument admission. Charging once per
        // endpoint would exhaust this allowance before a delta is returned.
        let changes = session
            .execute_diff(
                &cx,
                &prepared,
                &args,
                fgdb_types::CommitSeq(0),
                fgdb_types::CommitSeq(1),
            )
            .unwrap();
        assert_eq!(changes.changes().len(), 1);
        work_refusal(session.execute_batch(&cx, &[(&prepared, &args), (&prepared, &args)]));
        assert!(!session.is_closed());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn imported_recipe_coherence_owner_and_lifetime_precede_argument_work() {
    let ((), report) = run_async_under_lab(0x5ec0_5025, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let db = database(&c.commit()).await;
        let issuer = authority();
        let declaration = GqlParameters::new()
            .with_map(
                "record",
                vec![("key".into(), GraphValue::Scalar(CanonicalScalar::Int(7)))],
            )
            .unwrap();
        let text = "AT BRANCH 'main' MATCH (n:L {p: $record.key}) RETURN n";
        let (selector, native, lowered) = retained(text, &declaration);
        // Equal-width selectors have identical branch-free native text. The
        // recipe must also match the selector's exact retained source.
        let (_, _, mismatch) = retained(&text.replace("'main'", "'else'"), &declaration);
        let mut allowed = grant();
        allowed.limits.max_work = 32;
        let token = issuer.issue_at(&allowed, 100).unwrap();
        let calls = Cell::new(0);
        for (now, expected) in [(100, Error::WrongAuthority), (1001, Error::Expired)] {
            assert!(matches!(
                db.authorized_prepared_read_session(
                    &cx,
                    &issuer,
                    &token,
                    "main",
                    |kind, name: &str| {
                        calls.set(calls.get() + 1);
                        symbols(kind, name)
                    },
                    policy(),
                    || now,
                    selector.clone(),
                    native.clone(),
                    Some(mismatch.clone()),
                ),
                Err(QueryError::Authorization(error)) if error == expected
            ));
        }
        assert_eq!(calls.get(), 0);
        let now = Cell::new(100);
        let (mut session, prepared) = db
            .authorized_prepared_read_session(
                &cx,
                &issuer,
                &token,
                "main",
                symbols,
                policy(),
                || now.get(),
                selector.clone(),
                native.clone(),
                Some(lowered.clone()),
            )
            .unwrap();
        let (_, foreign) = db
            .authorized_prepared_read_session(
                &cx,
                &issuer,
                &token,
                "main",
                symbols,
                policy(),
                clock,
                selector.clone(),
                native.clone(),
                Some(lowered.clone()),
            )
            .unwrap();
        let oversized = arguments(&"雪".repeat(1024));
        assert!(matches!(
            session.execute(&cx, &foreign, &oversized),
            Err(QueryError::Authorization(Error::WrongAuthority))
        ));
        assert!(!session.is_closed());
        now.set(1001);
        assert!(matches!(
            session.execute(&cx, &prepared, &oversized),
            Err(QueryError::Authorization(Error::Expired))
        ));
        assert!(session.is_closed());

        let (mut session, prepared) = db
            .authorized_prepared_read_session(
                &cx,
                &issuer,
                &token,
                "main",
                symbols,
                policy(),
                clock,
                selector,
                native,
                Some(lowered),
            )
            .unwrap();
        issuer.retire();
        assert!(matches!(
            session.execute(&cx, &prepared, &oversized),
            Err(QueryError::Authorization(Error::AuthorityRetired))
        ));
        assert!(session.is_closed());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
