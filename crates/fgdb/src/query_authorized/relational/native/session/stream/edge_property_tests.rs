//! Selected edge fields must obey the same scope and live allowance as topology.
use super::super::failure_tests::{BRANCH, grant, issuer, policy};
use super::*;
use asupersync::lab::run_async_under_lab;
use fgdb_delta_types::LabelId;
use fgdb_types::PurposeContexts;
use fgdb_warden::{LimitDimension, Scope};
use std::cell::Cell;

struct Raw {
    fields: Vec<(PropertyKeyId, CanonicalScalar)>,
    relation: RelationId,
    hidden_target: bool,
    self_loop: bool,
    history_work: usize,
}
impl Raw {
    fn new(fields: Vec<(PropertyKeyId, CanonicalScalar)>) -> Self {
        Self {
            fields,
            relation: RelationId(1),
            hidden_target: false,
            self_loop: false,
            history_work: 1,
        }
    }
}
impl VertexScanSource for Raw {
    type Error = ReadError;
    fn snapshot_seq(&self) -> CommitSeq {
        CommitSeq(7)
    }
    fn next_vertex<C>(
        &mut self,
        _: &mut impl FnMut(VertexScanEvent) -> Result<(), C>,
    ) -> Result<Option<VId>, VertexScanSourceError<ReadError, C>> {
        panic!("a field lookup restarted the root scan")
    }
    fn vertex<'a, C>(
        &'a self,
        id: VId,
        control: &mut impl FnMut(VertexScanEvent) -> Result<(), C>,
    ) -> Result<Option<VertexScanRow<'a>>, VertexScanSourceError<ReadError, C>> {
        control(VertexScanEvent::Work).map_err(VertexScanSourceError::Control)?;
        Ok([VId(7), VId(8)].contains(&id).then_some(VertexScanRow {
            labels: if id == VId(8) && self.hidden_target {
                &[LabelId(99)]
            } else {
                &[LabelId(1)]
            },
            properties: &[],
        }))
    }
    fn probe_edge<'a, C>(
        &'a self,
        id: EId,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<EdgeScanRow<'a>>, EdgeExpansionSourceError<ReadError, C>> {
        for _ in 0..self.history_work {
            control(GlaExecutionEvent::Work)
                .map_err(|e| EdgeExpansionSourceError::Read(VertexScanSourceError::Control(e)))?;
        }
        Ok((id == EId(7)).then_some(EdgeScanRow {
            source: VId(7),
            target: if self.self_loop { VId(7) } else { VId(8) },
            relation: self.relation,
            properties: &self.fields,
        }))
    }
}
#[derive(Debug, PartialEq)]
struct Observation {
    result: Result<Option<Option<CanonicalScalar>>, AuthorizationError>,
    native_work: u64,
    signed_work: u64,
    nodes: u64,
    clock_samples: usize,
}
fn auth_error(error: EdgeExpansionSourceError<QueryError, QueryError>) -> AuthorizationError {
    match error {
        EdgeExpansionSourceError::Read(VertexScanSourceError::Source(
            QueryError::Authorization(e),
        ))
        | EdgeExpansionSourceError::Read(VertexScanSourceError::Control(
            QueryError::Authorization(e),
        )) => e,
        other => panic!("unexpected source refusal: {other:?}"),
    }
}
fn observe(cx: &QueryCx, raw: Raw, id: EId, key: PropertyKeyId, limit: u64) -> Observation {
    let issuer = issuer(8801);
    let mut grant = grant();
    grant.properties = Scope::only([0, 17, 25, 33, 200].map(PropertyKeyId));
    grant.limits.max_work = limit;
    let token = issuer.issue_at(&grant, 100).unwrap();
    let verified = issuer.verify_at(&token, BRANCH, 100).unwrap();
    let permit = verified.begin_read_at(BRANCH, 100).unwrap();
    let samples = Cell::new(0);
    let clock = || {
        samples.set(samples.get() + 1);
        100
    };
    let execution: Shared<'_> = Rc::new(RefCell::new(Execution::new(cx, permit, Box::new(clock))));
    let source = ScopedSource {
        inner: raw,
        execution: Rc::clone(&execution),
    };
    let mut work = 0;
    let result = source
        .probe_edge_property(id, key, &mut |event| {
            assert_eq!(
                event,
                GlaExecutionEvent::Work,
                "a borrowed scalar allocated scratch"
            );
            work += 1;
            execution.borrow_mut().checkpoint()
        })
        .map(|value| value.map(|value| value.cloned()))
        .map_err(auth_error);
    let used = execution.borrow().permit.usage();
    Observation {
        result,
        native_work: work,
        signed_work: used.work,
        nodes: used.nodes,
        clock_samples: samples.get(),
    }
}
fn with_query(test: impl FnOnce(&QueryCx)) {
    let runtime = asupersync::runtime::RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(asupersync::Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    test(&contexts.query());
}

#[test]
fn hidden_edge_fields_and_history_do_not_change_logical_usage_or_refusal_thresholds() {
    with_query(|cx| {
        let fields = vec![
            (PropertyKeyId(17), CanonicalScalar::Int(7)),
            (PropertyKeyId(33), CanonicalScalar::Null),
        ];
        let mut mixed = fields.clone();
        for key in 1u8..128 {
            if ![17, 25, 33].contains(&key) {
                mixed.push((
                    PropertyKeyId(u64::from(key)),
                    CanonicalScalar::Int(i64::from(key)),
                ));
            }
        }
        mixed.push((
            PropertyKeyId(150),
            CanonicalScalar::ucs_basic_text(&"hidden".repeat(10_000)).unwrap(),
        ));
        mixed.sort_by_key(|entry| entry.0);
        for key in [0, 7, 17, 25, 33, 200].map(PropertyKeyId) {
            let baseline = observe(cx, Raw::new(fields.clone()), EId(7), key, u64::MAX);
            assert_eq!(baseline.nodes, 2);
            for limit in 0..=baseline.signed_work {
                let plain = observe(cx, Raw::new(fields.clone()), EId(7), key, limit);
                let mut raw = Raw::new(mixed.clone());
                raw.history_work = 257;
                assert_eq!(
                    observe(cx, raw, EId(7), key, limit),
                    plain,
                    "key {} limit {limit}",
                    key.0
                );
                if limit < baseline.signed_work {
                    assert_eq!(
                        plain.result,
                        Err(AuthorizationError::LimitExceeded(LimitDimension::Work))
                    );
                } else {
                    assert_eq!(plain, baseline);
                }
            }
        }
    });
}

#[test]
fn relation_and_both_endpoints_are_admitted_before_charging_or_lending_a_field() {
    with_query(|cx| {
        let fields = vec![(PropertyKeyId(17), CanonicalScalar::Int(7))];
        for case in 0..3 {
            let mut raw = Raw::new(fields.clone());
            raw.relation = if case == 0 {
                RelationId(99)
            } else {
                RelationId(1)
            };
            raw.hidden_target = case == 1;
            let id = if case == 2 { EId(99) } else { EId(7) };
            assert_eq!(
                observe(cx, raw, id, PropertyKeyId(17), 0),
                Observation {
                    result: Ok(None),
                    native_work: 0,
                    signed_work: 0,
                    nodes: 0,
                    clock_samples: 0,
                }
            );
        }
        let mut raw = Raw::new(fields.clone());
        raw.self_loop = true;
        let value = observe(cx, raw, EId(7), PropertyKeyId(17), u64::MAX);
        assert_eq!(value.result, Ok(Some(Some(CanonicalScalar::Int(7)))));
        assert_eq!(value.nodes, 1, "a self-loop has one unique endpoint");
        assert_eq!(
            observe(cx, Raw::new(fields), EId(7), PropertyKeyId(7), u64::MAX).result,
            Ok(Some(None))
        );
    });
}

#[test]
fn repeated_edge_fields_share_a_single_terminal_node_allowance() {
    with_query(|cx| {
        let issuer = issuer(8802);
        let mut grant = grant();
        grant.properties = Scope::only([PropertyKeyId(17)]);
        grant.limits.max_nodes = 3;
        let token = issuer.issue_at(&grant, 100).unwrap();
        let verified = issuer.verify_at(&token, BRANCH, 100).unwrap();
        let permit = verified.begin_read_at(BRANCH, 100).unwrap();
        let execution: Shared<'_> =
            Rc::new(RefCell::new(Execution::new(cx, permit, Box::new(|| 100))));
        let source = ScopedSource {
            inner: Raw::new(vec![(PropertyKeyId(17), CanonicalScalar::Int(7))]),
            execution: Rc::clone(&execution),
        };
        let mut control = |_| execution.borrow_mut().checkpoint();
        let value = source
            .probe_edge_property(EId(7), PropertyKeyId(17), &mut control)
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(std::ptr::eq(value, &source.inner.fields[0].1));
        let error = source
            .probe_edge_property(EId(7), PropertyKeyId(17), &mut control)
            .unwrap_err();
        assert_eq!(
            auth_error(error),
            AuthorizationError::LimitExceeded(LimitDimension::Nodes)
        );
        let used = execution.borrow().permit.usage();
        let again = source
            .probe_edge_property(EId(7), PropertyKeyId(17), &mut control)
            .unwrap_err();
        assert_eq!(
            auth_error(again),
            AuthorizationError::LimitExceeded(LimitDimension::Nodes)
        );
        assert_eq!(execution.borrow().permit.usage(), used);
    });
}

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "L") => Some(GraphSymbol::Label(LabelId(1))),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(RelationId(1))),
        (GraphSymbolKind::Relation, "S") => Some(GraphSymbol::Relation(RelationId(99))),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
        (GraphSymbolKind::Property, "secret") => Some(GraphSymbol::Property(PropertyKeyId(2))),
        _ => None,
    }
}
fn ids(rows: &[GraphValueRow]) -> Vec<VId> {
    rows.iter()
        .map(|row| row.get(0).unwrap().as_vertex().unwrap())
        .collect()
}
fn pull<R: GraphSymbolResolver, C: FnMut() -> u64>(
    session: &mut AuthorizedReadSession<'_, R, C>,
    cx: &QueryCx,
    text: &str,
) -> Result<Vec<GraphValueRow>, QueryError> {
    let params = GqlParameters::new();
    let prepared = session.prepare(cx, text, &params)?;
    session.stream(cx, &prepared, &params)?.collect()
}
const EXISTS: &str =
    "MATCH (a) WHERE EXISTS { MATCH (a)-[edge:R]->(b) WHERE edge.p > a.p } RETURN a";

async fn seed(db: &mut Database<crate::MemVfs>, cx: &fgdb_types::CommitCx) -> CommitSeq {
    let p = PropertyKeyId(1);
    let mut batch = crate::WriteBatch::new(RelationId(1));
    for id in 1..=4 {
        batch.create_vertex(
            VId(id),
            vec![LabelId(if id == 2 { 99 } else { 1 })],
            vec![(p, CanonicalScalar::Int(10))],
        );
    }
    for (id, a, b, value) in [
        (10, 1, 3, 0),
        (11, 1, 3, 20),
        (12, 3, 4, 0),
        (13, 3, 2, 100),
    ] {
        batch.add_edge(
            EId(id),
            VId(a),
            VId(b),
            vec![
                (p, CanonicalScalar::Int(value)),
                (PropertyKeyId(2), CanonicalScalar::Int(999)),
            ],
        );
    }
    db.write(cx, batch).await.unwrap()
}

#[test]
fn authorized_private_relationship_predicates_match_scoped_eager_results() {
    let ((), report) = run_async_under_lab(0x5ec0_8803, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let issuer = issuer(8803);
        let keys = crate::DatabaseKeys::new([0x37; 32], issuer.namespace(), [0x95; 32]);
        let mut db = Database::open_memory(&contexts.commit(), keys)
            .await
            .unwrap();
        seed(&mut db, &contexts.commit()).await;
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let mut session = db
            .authorized_read_session(&cx, &issuer, &token, BRANCH, symbols, policy(), || 100)
            .unwrap();
        for (text, expected) in [
            (EXISTS, vec![VId(1)]),
            (
                "MATCH (a) WHERE NOT EXISTS { MATCH (a)-[edge:R]->(b) WHERE edge.p > a.p } RETURN a",
                vec![VId(3), VId(4)],
            ),
            (
                "MATCH (a) WHERE EXISTS { MATCH (a)-[edge:R]->(b) WHERE edge.secret = 999 } RETURN a",
                vec![],
            ),
            (
                "MATCH (a) WHERE EXISTS { MATCH (a)-[edge:R]->(b) WHERE edge.secret IS NULL } RETURN a",
                vec![VId(1), VId(3)],
            ),
            (
                "MATCH (a) WHERE EXISTS { MATCH (a)<-[edge:R]-(b) WHERE edge.p > 0 } RETURN a",
                vec![VId(3)],
            ),
            (
                "MATCH (a) WHERE EXISTS { MATCH (a)-[edge:R]-(b) WHERE edge.p > 0 } RETURN a",
                vec![VId(1), VId(3)],
            ),
            (
                "MATCH (a) WHERE NOT EXISTS { MATCH (a)-[edge:S]->(b) WHERE edge.p > 0 } RETURN a",
                vec![VId(1), VId(3), VId(4)],
            ),
        ] {
            let rows = pull(&mut session, &cx, text).unwrap();
            assert_eq!(ids(&rows), expected, "{text}");
            let crate::QueryResult::Rows { rows: eager, .. } =
                session.query(&cx, text, &GqlParameters::new()).unwrap()
            else {
                panic!("read returned a write")
            };
            let values = rows
                .iter()
                .map(|row| {
                    row.values()
                        .iter()
                        .cloned()
                        .map(fgdb_gql::GraphAggregateValue::Value)
                        .collect::<Vec<_>>()
                })
                .collect::<Vec<_>>();
            assert_eq!(values, eager, "{text}");
        }
        let aggregate = "MATCH (a) WHERE EXISTS { MATCH (a)-[edge:R]->(b) WHERE edge.p > a.p } RETURN count(*) AS total";
        let params = GqlParameters::new();
        let prepared = session.prepare(&cx, aggregate, &params).unwrap();
        {
            let mut cursor = session.stream_aggregate(&cx, &prepared, &params).unwrap();
            let rows = cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].values(), &[fgdb_gql::GraphAggregateValue::Count(1)]);
            assert_eq!(cursor.state(), VertexScanState::Exhausted);
        }
        assert!(!session.is_closed());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn every_issuer_expiry_boundary_closes_the_private_edge_cursor_and_session() {
    let ((), report) = run_async_under_lab(0x5ec0_8805, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let issuer = issuer(8805);
        let keys = crate::DatabaseKeys::new([0x37; 32], issuer.namespace(), [0x95; 32]);
        let mut db = Database::open_memory(&contexts.commit(), keys)
            .await
            .unwrap();
        seed(&mut db, &contexts.commit()).await;
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let calls = Cell::new(0);
        let cutoff = Cell::new(usize::MAX);
        let clock = || {
            let next = calls.get() + 1;
            calls.set(next);
            if next >= cutoff.get() { 1000 } else { 100 }
        };
        let params = GqlParameters::new();
        let baseline_refs = Arc::strong_count(&db.snapshot);
        let checkpoints = {
            let mut session = db
                .authorized_read_session(&cx, &issuer, &token, BRANCH, symbols, policy(), clock)
                .unwrap();
            let prepared = session.prepare(&cx, EXISTS, &params).unwrap();
            calls.set(0);
            let rows = session
                .stream(&cx, &prepared, &params)
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
            assert_eq!(ids(&rows), vec![VId(1)]);
            calls.get()
        };
        assert_eq!(Arc::strong_count(&db.snapshot), baseline_refs);
        for stop in 1..=checkpoints {
            cutoff.set(usize::MAX);
            calls.set(0);
            let mut session = db
                .authorized_read_session(&cx, &issuer, &token, BRANCH, symbols, policy(), clock)
                .unwrap();
            let prepared = session.prepare(&cx, EXISTS, &params).unwrap();
            calls.set(0);
            cutoff.set(stop);
            let mut delivered = Vec::new();
            let failure = match session.stream(&cx, &prepared, &params) {
                Err(error) => error,
                Ok(mut cursor) => {
                    let error = loop {
                        match cursor.next() {
                            Some(Ok(row)) => delivered.push(row),
                            Some(Err(error)) => break error,
                            None => panic!("expiry became EOF at {stop}"),
                        }
                    };
                    assert_eq!(cursor.state(), VertexScanState::Failed);
                    assert!(cursor.next().is_none());
                    error
                }
            };
            assert!(matches!(
                failure,
                QueryError::Authorization(AuthorizationError::Expired)
            ));
            assert!([VId(1)].starts_with(&ids(&delivered)));
            assert!(
                session.is_closed(),
                "expiry at callback {stop} retained the session"
            );
            assert_eq!(calls.get(), stop, "failed execution called its clock again");
            assert_eq!(Arc::strong_count(&db.snapshot), baseline_refs);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn signed_row_refusal_is_not_empty_success_and_does_not_widen_the_next_query() {
    let ((), report) = run_async_under_lab(0x5ec0_8806, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let issuer = issuer(8806);
        let keys = crate::DatabaseKeys::new([0x37; 32], issuer.namespace(), [0x95; 32]);
        let mut db = Database::open_memory(&contexts.commit(), keys)
            .await
            .unwrap();
        seed(&mut db, &contexts.commit()).await;
        let mut grant = grant();
        grant.limits.max_rows = 0;
        let token = issuer.issue_at(&grant, 100).unwrap();
        let mut session = db
            .authorized_read_session(&cx, &issuer, &token, BRANCH, symbols, policy(), || 100)
            .unwrap();
        for _ in 0..2 {
            let result = pull(&mut session, &cx, EXISTS);
            assert!(matches!(
                result,
                Err(QueryError::Authorization(
                    AuthorizationError::LimitExceeded(LimitDimension::Rows)
                ))
            ));
            assert!(!session.is_closed());
            assert!(
                pull(&mut session, &cx, &format!("{EXISTS} LIMIT 0"))
                    .unwrap()
                    .is_empty()
            );
        }
        issuer.retire();
        assert!(matches!(
            pull(&mut session, &cx, &format!("{EXISTS} LIMIT 0")),
            Err(QueryError::Authorization(
                AuthorizationError::AuthorityRetired
            ))
        ));
        assert!(session.is_closed());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn scoped_edge_fields_follow_pinned_history_after_writes_and_reopen() {
    let ((), report) = run_async_under_lab(0x5ec0_8804, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let issuer = issuer(8804);
        let keys = crate::DatabaseKeys::new([0x37; 32], issuer.namespace(), [0x95; 32]);
        let vfs = crate::MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys.clone())
            .await
            .unwrap();
        let basis = seed(&mut db, &commit).await;
        let token = issuer.issue_at(&grant(), 100).unwrap();
        // A plain function clock keeps this session Send across native writes;
        // no !Send pull cursor or Rc-backed source survives an await.
        let mut pinned = db
            .authorized_read_session(&cx, &issuer, &token, BRANCH, symbols, policy(), || 100)
            .unwrap();
        let mut change = crate::WriteBatch::new(RelationId(1));
        change.set_edge_property(EId(11), PropertyKeyId(1), Some(CanonicalScalar::Int(-1)));
        change.delete_vertex(VId(4));
        change.add_edge(
            EId(14),
            VId(3),
            VId(1),
            vec![(PropertyKeyId(1), CanonicalScalar::Int(40))],
        );
        let after = db.write(&commit, change).await.unwrap();
        {
            let params = GqlParameters::new();
            let prepared = pinned.prepare(&cx, EXISTS, &params).unwrap();
            let mut cursor = pinned.stream(&cx, &prepared, &params).unwrap();
            assert_eq!(cursor.snapshot_seq(), basis);
            assert_eq!(
                ids(&cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap()),
                vec![VId(1)]
            );
        }
        drop(pinned);
        drop(db);
        let reopened = Database::open_with_vfs(&commit, vfs, &path, keys)
            .await
            .unwrap();
        let mut session = reopened
            .authorized_read_session(&cx, &issuer, &token, BRANCH, symbols, policy(), || 100)
            .unwrap();
        let params = GqlParameters::new();
        let prepared = session.prepare(&cx, EXISTS, &params).unwrap();
        let mut cursor = session.stream(&cx, &prepared, &params).unwrap();
        assert_eq!(cursor.snapshot_seq(), after);
        assert_eq!(
            ids(&cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap()),
            vec![VId(3)]
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
