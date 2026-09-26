//! SUBSCRIBE TO is a surface over the existing native GLA preparation and
//! maintained circuit registry, not an AST interpreter or a polling evaluator.

use super::subscription::{NativeSubscription, SubscriptionError};
use super::*;

#[derive(Debug)]
pub enum SubscribeError {
    ExpectedKeyword(&'static str),
    EmptyQuery,
    UnterminatedComment,
    Subscription(SubscriptionError),
}
impl core::fmt::Display for SubscribeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::ExpectedKeyword(keyword) => {
                write!(f, "expected {keyword} in subscription header")
            }
            Self::EmptyQuery => f.write_str("SUBSCRIBE TO requires a native query"),
            Self::UnterminatedComment => f.write_str("unterminated subscription header comment"),
            Self::Subscription(error) => error.fmt(f),
        }
    }
}
impl core::error::Error for SubscribeError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Subscription(error) => Some(error),
            _ => None,
        }
    }
}
impl From<SubscriptionError> for SubscribeError {
    fn from(error: SubscriptionError) -> Self {
        Self::Subscription(error)
    }
}
impl From<StandingQueryError> for SubscribeError {
    fn from(error: StandingQueryError) -> Self {
        Self::Subscription(error.into())
    }
}

impl<V: Vfs + Clone> Database<V> {
    /// Register `SUBSCRIBE TO <native query>` and open an acknowledged consumer.
    /// Case-insensitive header keywords, whitespace, line comments and nested
    /// block comments are accepted. The query body is passed unchanged to the
    /// ordinary native preparation/binding path; all maintained-operator,
    /// parameter, null, grouping and selected-page semantics remain there.
    ///
    /// The header has an interruptible logical-work allowance; registration
    /// keeps the existing per-node maintenance allowances. Failure publishes no
    /// registration, including cancellation while opening the consumer after a
    /// successfully prepared circuit. No resolver runs before health admission.
    ///
    /// This lane starts with a current BAG baseline at the first successful
    /// poll. It does not implement historical SINCE, durable resume, an event
    /// backlog, ordered sequence edits or registration persistence on reopen.
    pub fn subscribe_native(
        &mut self,
        cx: &QueryCx,
        statement: &str,
        params: &GqlParameters,
        resolver: impl GraphSymbolResolver,
        policy: GqlQueryPolicy,
    ) -> Result<NativeSubscription, SubscribeError> {
        cx.checkpoint().map_err(StandingQueryError::Interrupted)?;
        self.ensure_readable().map_err(StandingQueryError::Read)?;
        let mut remaining = policy.evaluator.max_work_units;
        let query = subscription_query(statement, &mut || {
            cx.checkpoint().map_err(StandingQueryError::Interrupted)?;
            remaining = remaining
                .checked_sub(1)
                .ok_or_else(|| StandingQueryError::Delivery(StandingQueryFailure::WorkBudget))?;
            Ok(())
        })?;
        let first = self.standing_queries.len();
        let registered = self.register_standing_native(cx, query, params, resolver, policy);
        finish_registration(self, cx, first, registered).map_err(SubscribeError::from)
    }
}

impl PreparedNativeRead {
    /// Bind a reusable native template into a maintained circuit and independent
    /// acknowledged consumer. Later polls never re-run preparation or the query.
    /// The parameters are owned by the accepted definition, as for
    /// register_standing. Failed opening removes only this call's private suffix.
    pub fn subscribe<V: Vfs + Clone>(
        &self,
        database: &mut Database<V>,
        cx: &QueryCx,
        params: &GqlParameters,
        policy: GqlQueryPolicy,
    ) -> Result<NativeSubscription, SubscriptionError> {
        let first = database.standing_queries.len();
        let registered = self.register_standing(database, cx, params, policy);
        finish_registration(database, cx, first, registered)
    }
}

fn finish_registration<V: Vfs + Clone>(
    database: &mut Database<V>,
    cx: &QueryCx,
    first: usize,
    registered: Result<StandingQueryHandle, StandingQueryError>,
) -> Result<NativeSubscription, SubscriptionError> {
    let result = registered
        .map_err(SubscriptionError::from)
        .and_then(|handle| database.open_standing_subscription(cx, &handle));
    if result.is_err() {
        // The exclusive database borrow covers both registration and opening.
        // Only newly appended nodes belong to this call; no old handle can
        // refer to them and no successful new handle escaped this function.
        database.standing_queries.truncate(first);
    }
    result
}

fn subscription_query<'a>(
    text: &'a str,
    control: &mut impl FnMut() -> Result<(), SubscribeError>,
) -> Result<&'a str, SubscribeError> {
    let mut rest = text;
    for keyword in ["SUBSCRIBE", "TO"] {
        rest = skip_trivia(rest, control)?;
        control()?;
        let head = rest
            .get(..keyword.len())
            .filter(|head| head.eq_ignore_ascii_case(keyword))
            .ok_or(SubscribeError::ExpectedKeyword(keyword))?;
        rest = &rest[head.len()..];
        if rest
            .chars()
            .next()
            .is_some_and(|ch| ch.is_alphanumeric() || ch == '_')
        {
            return Err(SubscribeError::ExpectedKeyword(keyword));
        }
    }
    let query = skip_trivia(rest, control)?;
    if query.is_empty() {
        return Err(SubscribeError::EmptyQuery);
    }
    Ok(query)
}

fn skip_trivia<'a>(
    text: &'a str,
    control: &mut impl FnMut() -> Result<(), SubscribeError>,
) -> Result<&'a str, SubscribeError> {
    let bytes = text.as_bytes();
    let mut at = 0;
    while at < bytes.len() {
        control()?;
        if bytes[at].is_ascii_whitespace() {
            at += 1;
        } else if bytes[at..].starts_with(b"--") {
            at += 2;
            while at < bytes.len() && !matches!(bytes[at], b'\r' | b'\n') {
                control()?;
                at += 1;
            }
        } else if bytes[at..].starts_with(b"/*") {
            at += 2;
            let mut depth = 1_usize;
            while depth != 0 {
                control()?;
                if at == bytes.len() {
                    return Err(SubscribeError::UnterminatedComment);
                }
                if bytes[at..].starts_with(b"/*") {
                    // Each nesting opener occupies two bytes of this slice,
                    // so depth cannot overflow its usize-addressable length.
                    depth += 1;
                    at += 2;
                } else if bytes[at..].starts_with(b"*/") {
                    depth -= 1;
                    at += 2;
                } else {
                    at += 1;
                }
            }
        } else {
            break;
        }
    }
    // Byte walks over comment payloads exit only at EOF/newline or the ASCII
    // closing delimiter. Outside comments we advance over ASCII whitespace
    // only, so this return is always a UTF-8 boundary, including Unicode text.
    Ok(&text[at..])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DatabaseKeys, WriteBatch};
    use asupersync::lab::run_async_under_lab;
    use fgdb_delta_types::{PropertyKeyId, RelationId};
    use fgdb_gql::{GraphSymbol, GraphSymbolKind};
    use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, PurposeContexts};

    fn policy() -> GqlQueryPolicy {
        GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000)
    }
    fn keys() -> DatabaseKeys {
        DatabaseKeys::new([1; 32], DatabaseSecurityNamespaceId([2; 32]), [3; 32])
    }
    fn resolve(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        match (kind, name) {
            (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
            _ => None,
        }
    }
    fn seed() -> WriteBatch {
        let mut batch = WriteBatch::new(RelationId(1));
        for (id, value) in [(1, 3), (2, 3), (3, 7)] {
            batch.create_vertex(
                VId(id),
                vec![],
                vec![(PropertyKeyId(1), CanonicalScalar::Int(value))],
            );
        }
        batch
    }
    fn result_bag(result: QueryResult) -> ZSet<Vec<QueryValue>> {
        let QueryResult::Rows { rows, .. } = result else {
            panic!("read result required");
        };
        ZSet::from_updates(
            rows.into_iter().map(|row| (row, ZWeight::ONE)),
            LIMBS,
            &mut |_| Ok::<_, ()>(()),
        )
        .unwrap()
    }

    #[test]
    fn header_boundaries_comments_and_unicode_preserve_the_query_body() {
        for (statement, expected) in [
            ("SUBSCRIBE TO MATCH (n) RETURN n", "MATCH (n) RETURN n"),
            (
                " /* λ /* nested */ */ SuBsCrIbE -- header\n TO/* c */RETURN 'SINCE TO' AS s",
                "RETURN 'SINCE TO' AS s",
            ),
            ("SUBSCRIBE/**/TO RETURN $value AS v", "RETURN $value AS v"),
            (
                "SUBSCRIBE TO RETURN '/* literal */' AS s; -- tail",
                "RETURN '/* literal */' AS s; -- tail",
            ),
        ] {
            assert_eq!(
                subscription_query(statement, &mut || Ok(())).unwrap(),
                expected
            );
        }
        for statement in [
            "",
            "SUBSCRIBETO RETURN 1",
            "SUBSCRIBE2 TO RETURN 1",
            "SUBSCRIBE_TO RETURN 1",
            "SUBSCRIBE TOGETHER RETURN 1",
            "SUBSCRIBE",
            "SUBSCRIBE TO",
            "SUBSCRIBE TO -- only comment",
            "SUBSCRIBE /* unclosed",
            "/* λ",
            "ΣUBSCRIBE TO RETURN 1",
        ] {
            assert!(
                subscription_query(statement, &mut || Ok(())).is_err(),
                "{statement}"
            );
        }
    }

    #[test]
    fn every_header_checkpoint_refuses_without_changing_the_input() {
        let input = "/* α /* nested */ */SUBSCRIBE-- line\n TO RETURN 1 AS x";
        let mut count = 0;
        assert_eq!(
            subscription_query(input, &mut || {
                count += 1;
                Ok(())
            })
            .unwrap(),
            "RETURN 1 AS x"
        );
        for stop in 1..=count {
            let mut seen = 0;
            let result = subscription_query(input, &mut || {
                seen += 1;
                if seen == stop {
                    Err(SubscribeError::EmptyQuery)
                } else {
                    Ok(())
                }
            });
            assert!(matches!(result, Err(SubscribeError::EmptyQuery)));
            assert_eq!(seen, stop);
        }
    }

    #[test]
    fn native_subscriptions_integrate_deltas_across_final_output_shapes() {
        let ((), report) = run_async_under_lab(0x006d_de51, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.query();
            let commit = contexts.commit();
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let first = db.write(&commit, seed()).await.unwrap();
            let params = GqlParameters::new();
            let texts = [
                "MATCH (n) RETURN n.p AS p",
                "MATCH (n) RETURN DISTINCT n.p AS p",
                "MATCH (n) RETURN n.p AS p ORDER BY p DESC LIMIT 2",
                "MATCH (n) RETURN COUNT(*) AS c,SUM(n.p) AS s,AVG(n.p) AS a",
                "MATCH (n) RETURN n.p AS p UNION ALL MATCH (m) RETURN m.p AS p",
                "RETURN 7 AS fixed",
            ];
            let mut consumers = Vec::new();
            let mut bags = Vec::new();
            for text in texts {
                let mut sub = db
                    .subscribe_native(
                        &cx,
                        &format!("SUBSCRIBE TO {text}"),
                        &params,
                        resolve,
                        policy(),
                    )
                    .unwrap();
                let frame = sub.poll(&db, &cx, policy()).unwrap().unwrap();
                assert!(frame.is_snapshot());
                assert_eq!(frame.frontier(), first);
                let expected = result_bag(db.query(&cx, text, &params, resolve, policy()).unwrap());
                assert_eq!(frame.rows(), &expected);
                bags.push(expected);
                sub.acknowledge(frame.receipt()).unwrap();
                assert!(sub.poll(&db, &cx, policy()).unwrap().is_none());
                consumers.push(sub);
            }
            for tick in 0..2 {
                let mut batch = WriteBatch::new(RelationId(1));
                if tick == 0 {
                    batch.delete_vertex(VId(1));
                    batch.set_vertex_property(
                        VId(3),
                        PropertyKeyId(1),
                        Some(CanonicalScalar::Int(2)),
                    );
                } else {
                    batch.set_vertex_property(
                        VId(2),
                        PropertyKeyId(99),
                        Some(CanonicalScalar::Int(1)),
                    );
                }
                let at = db.write(&commit, batch).await.unwrap();
                for (index, sub) in consumers.iter_mut().enumerate() {
                    let from = sub.acknowledged_frontier().unwrap();
                    let frame = sub.poll(&db, &cx, policy()).unwrap().unwrap();
                    assert_eq!(frame.from(), Some(from));
                    assert_eq!(frame.frontier(), at);
                    let replay = sub
                        .poll(&db, &cx, GqlQueryPolicy::new(0, 0, 0, 0))
                        .unwrap()
                        .unwrap();
                    assert!(Arc::ptr_eq(&frame, &replay));
                    assert_eq!(sub.acknowledged_frontier(), Some(from));
                    if tick == 1 {
                        assert!(frame.rows().is_empty());
                    }
                    bags[index]
                        .integrate(frame.rows(), LIMBS, &mut |_| Ok::<_, ()>(()))
                        .unwrap();
                    assert_eq!(
                        bags[index],
                        result_bag(
                            db.query(&cx, texts[index], &params, resolve, policy())
                                .unwrap()
                        )
                    );
                    sub.acknowledge(frame.receipt()).unwrap();
                    assert!(sub.poll(&db, &cx, policy()).unwrap().is_none());
                }
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn slow_consumer_replays_pending_frame_then_requires_explicit_rebaseline() {
        let ((), report) = run_async_under_lab(0x006d_de52, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.query();
            let commit = contexts.commit();
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let first = db.write(&commit, seed()).await.unwrap();
            let params = GqlParameters::new();
            let mut sub = db
                .subscribe_native(
                    &cx,
                    "SUBSCRIBE TO MATCH (n) RETURN n.p AS p",
                    &params,
                    resolve,
                    policy(),
                )
                .unwrap();
            assert!(matches!(
                sub.poll(&db, &cx, GqlQueryPolicy::new(0, 0, 100_000, 100_000)),
                Err(SubscriptionError::Query(StandingQueryError::Delivery(
                    StandingQueryFailure::ResultBudget
                )))
            ));
            assert_eq!(sub.acknowledged_frontier(), None);
            let initial = sub.poll(&db, &cx, policy()).unwrap().unwrap();
            for id in [2, 3] {
                let mut batch = WriteBatch::new(RelationId(1));
                batch.set_vertex_property(VId(id), PropertyKeyId(1), Some(CanonicalScalar::Int(9)));
                db.write(&commit, batch).await.unwrap();
            }
            let replay = sub.poll(&db, &cx, policy()).unwrap().unwrap();
            assert!(Arc::ptr_eq(&initial, &replay));
            sub.acknowledge(initial.receipt()).unwrap();
            assert!(matches!(
                sub.poll(&db, &cx, policy()),
                Err(SubscriptionError::Query(
                    StandingQueryError::DeltaUnavailable { .. }
                ))
            ));
            assert_eq!(sub.acknowledged_frontier(), Some(first));
            sub.restart_from_current().unwrap();
            let replacement = sub.poll(&db, &cx, policy()).unwrap().unwrap();
            assert!(replacement.is_snapshot());
            assert_eq!(replacement.frontier(), db.frontier().unwrap());
            assert_eq!(
                replacement.rows(),
                &db.standing_native_bag(&cx, sub.handle(), policy())
                    .unwrap()
                    .1
            );
            assert!(matches!(
                sub.acknowledge(initial.receipt()),
                Err(SubscriptionError::InvalidReceipt)
            ));
            let foreign = Database::open_memory(&commit, keys()).await.unwrap();
            assert!(matches!(
                sub.poll(&foreign, &cx, policy()),
                Err(SubscriptionError::Query(StandingQueryError::ForeignHandle))
            ));
            assert!(Arc::ptr_eq(
                &replacement,
                &sub.poll(&db, &cx, policy()).unwrap().unwrap()
            ));
            sub.acknowledge(replacement.receipt()).unwrap();
            sub.close();
            assert!(matches!(
                sub.poll(&db, &cx, policy()),
                Err(SubscriptionError::Closed)
            ));
            assert!(db.standing_native_bag(&cx, sub.handle(), policy()).is_ok());
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn statement_failures_preserve_registry_and_prepared_templates_subscribe() {
        let ((), report) = run_async_under_lab(0x006d_de53, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.query();
            let commit = contexts.commit();
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            db.write(&commit, seed()).await.unwrap();
            let params = GqlParameters::new();
            let template =
                PreparedNativeRead::prepare("MATCH (n) RETURN n.p AS p", &params, resolve).unwrap();
            let mut existing = template.subscribe(&mut db, &cx, &params, policy()).unwrap();
            let count = db.standing_queries.len();
            for text in [
                "",
                "SUBSCRIBE TO",
                "SUBSCRIBE /* unclosed",
                "SUBSCRIBE TO NOT_A_QUERY",
            ] {
                assert!(
                    db.subscribe_native(&cx, text, &params, resolve, policy())
                        .is_err()
                );
                assert_eq!(db.standing_queries.len(), count);
            }
            assert!(matches!(
                db.subscribe_native(
                    &cx,
                    "SUBSCRIBE TO RETURN 1 AS v",
                    &params,
                    resolve,
                    GqlQueryPolicy::new(0, 0, 0, 0),
                ),
                Err(SubscribeError::Subscription(SubscriptionError::Query(
                    StandingQueryError::Delivery(StandingQueryFailure::WorkBudget)
                )))
            ));
            assert_eq!(db.standing_queries.len(), count);
            let frame = existing.poll(&db, &cx, policy()).unwrap().unwrap();
            assert_eq!(frame.rows().len(), 2);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn failed_maintenance_is_not_an_empty_tick_and_rebuild_requires_a_baseline() {
        let ((), report) = run_async_under_lab(0x006d_de54, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.query();
            let commit = contexts.commit();
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let first = db.write(&commit, seed()).await.unwrap();
            let params = GqlParameters::new();
            let mut sub = db
                .subscribe_native(
                    &cx,
                    "SUBSCRIBE TO MATCH (n) RETURN SUM(n.p) AS s",
                    &params,
                    resolve,
                    policy(),
                )
                .unwrap();
            let baseline = sub.poll(&db, &cx, policy()).unwrap().unwrap();
            sub.acknowledge(baseline.receipt()).unwrap();
            let mut invalid = WriteBatch::new(RelationId(1));
            invalid.set_vertex_property(
                VId(1),
                PropertyKeyId(1),
                Some(CanonicalScalar::Bool(true)),
            );
            let at = db.write(&commit, invalid).await.unwrap();
            assert_eq!(db.frontier().unwrap(), at);
            assert!(matches!(
                sub.poll(&db, &cx, policy()),
                Err(SubscriptionError::Query(
                    StandingQueryError::Unavailable { .. }
                ))
            ));
            assert_eq!(sub.acknowledged_frontier(), Some(first));
            let mut repair = WriteBatch::new(RelationId(1));
            repair.set_vertex_property(VId(1), PropertyKeyId(1), Some(CanonicalScalar::Int(3)));
            db.write(&commit, repair).await.unwrap();
            db.rebuild_standing_query(&cx, sub.handle(), policy())
                .unwrap();
            assert!(matches!(
                sub.poll(&db, &cx, policy()),
                Err(SubscriptionError::Query(
                    StandingQueryError::DeltaUnavailable { .. }
                ))
            ));
            sub.restart_from_current().unwrap();
            assert!(sub.poll(&db, &cx, policy()).unwrap().unwrap().is_snapshot());
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }
}
