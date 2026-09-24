//! Real committed graph reads through the signed Warden boundary.
//! No claim that these resident-source tests prove physical side-channel isolation.
use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb::{Database, DatabaseKeys, MemVfs, QueryError, ReadError, WriteBatch};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId, SchemaEpoch};
use fgdb_gql::algebra::{GraphValue, GraphValueRow, PreparedGraphPattern};
use fgdb_gql::row_join::{RowJoinKind, RowJoinSpec};
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GraphSymbol, GraphSymbolKind, PreparedGraphText,
};
use fgdb_gql::{
    GraphAggregate, GraphAggregateError, GraphAggregateValue, GraphSetExecutionError,
    GraphSetOperation, GraphSetQuantifier, GraphSetValue, PreparedGraphSet,
    PreparedGraphSetAggregate,
};
use fgdb_types::{
    CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts,
    QueryCx, VId,
};
use fgdb_warden::{
    Authority, CapabilityToken, Error, Grant, LimitDimension, QueryLimits, Restriction, Rights,
    Scope,
};

const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([7; 32]);
const BRANCH: &str = "host-selected-branch";
fn authority(seed: u64, namespace: DatabaseSecurityNamespaceId) -> Authority {
    Authority::new(
        AuthKey::from_seed(seed),
        namespace,
        "host-graph",
        SchemaEpoch(0),
        1,
    )
    .unwrap()
}
fn grant() -> Grant {
    let mut grant = Grant::read_only(
        BRANCH,
        1000,
        QueryLimits {
            max_nodes: 1000,
            max_work: 1_000_000,
            max_rows: 1000,
        },
    );
    grant.labels = Scope::only([LabelId(1)]);
    grant.relations = Scope::only([RelationId(1)]);
    grant.properties = Scope::only([PropertyKeyId(1)]);
    grant
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(1000, 1000, 1_000_000, 1_000_000)
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "L") => Some(GraphSymbol::Label(LabelId(1))),
        (GraphSymbolKind::Label, "H") => Some(GraphSymbol::Label(LabelId(99))),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(RelationId(1))),
        (GraphSymbolKind::Relation, "S") => Some(GraphSymbol::Relation(RelationId(2))),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
        (GraphSymbolKind::Property, "hidden") => Some(GraphSymbol::Property(PropertyKeyId(2))),
        _ => None,
    }
}
fn pattern(text: &str) -> PreparedGraphPattern<GraphValueRow> {
    PreparedGraphText::prepare(text, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
        .with_duplicates()
}
async fn database(cx: &CommitCx) -> Database<MemVfs> {
    let mut db = Database::open_memory(cx, DatabaseKeys::new([0x45; 32], NS, [0x73; 32]))
        .await
        .unwrap();
    let mut initial = WriteBatch::new(RelationId(1));
    for (vid, labels, p) in [
        (1, vec![LabelId(1), LabelId(99)], 10),
        (2, vec![LabelId(99)], 20),
        (3, vec![LabelId(1)], 30),
    ] {
        initial.create_vertex(
            VId(vid),
            labels,
            vec![
                (PropertyKeyId(1), CanonicalScalar::Int(p)),
                (PropertyKeyId(2), CanonicalScalar::Int(777)),
            ],
        );
    }
    for (eid, src, dst) in [(10, 1, 2), (11, 2, 3), (12, 1, 3), (13, 1, 3), (14, 3, 1)] {
        initial.add_edge(
            EId(eid),
            VId(src),
            VId(dst),
            vec![
                (PropertyKeyId(1), CanonicalScalar::Int(5)),
                (PropertyKeyId(2), CanonicalScalar::Int(999)),
            ],
        );
    }
    db.write(cx, initial).await.unwrap();
    let mut other = WriteBatch::new(RelationId(2));
    other.add_edge(EId(20), VId(1), VId(3), vec![]);
    db.write(cx, other).await.unwrap();
    db
}
fn row(values: Vec<GraphValue>) -> GraphValueRow {
    GraphValueRow::from_owned_values(values)
}
fn vertex(id: u128) -> GraphValue {
    GraphValue::Vertex(VId(id))
}
fn scalar(value: i64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(value))
}
fn null() -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Null)
}
fn read(
    db: &Database<MemVfs>,
    cx: &QueryCx,
    issuer: &Authority,
    token: &CapabilityToken,
    text: &str,
) -> Vec<GraphValueRow> {
    db.execute_graph_pattern_authorized(cx, issuer, token, BRANCH, &pattern(text), policy(), || 100)
        .unwrap()
}

#[test]
fn topology_and_metadata_are_masked_before_matching_not_after_projection() {
    let ((), report) = run_async_under_lab(0x5ec0_1001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let db = database(&contexts.commit()).await;
        let issuer = authority(91, NS);
        let token = issuer.issue_at(&grant(), 100).unwrap();
        assert_eq!(
            read(
                &db,
                &cx,
                &issuer,
                &token,
                "MATCH (n) RETURN n, n.p AS p, n.hidden AS hidden"
            ),
            vec![
                row(vec![vertex(1), scalar(10), null()]),
                row(vec![vertex(3), scalar(30), null()])
            ]
        );
        assert_eq!(
            read(
                &db,
                &cx,
                &issuer,
                &token,
                "MATCH (n) WHERE n.hidden = 777 OR n.p = 10 RETURN n"
            ),
            vec![row(vec![vertex(1)])]
        );
        assert_eq!(
            read(
                &db,
                &cx,
                &issuer,
                &token,
                "MATCH (n) WHERE n.hidden IS NULL RETURN n"
            ),
            vec![row(vec![vertex(1)]), row(vec![vertex(3)])]
        );
        assert!(read(&db, &cx, &issuer, &token, "MATCH (n:H) RETURN n").is_empty());
        let pairs = read(&db, &cx, &issuer, &token, "MATCH (a)-[:R]->(b) RETURN a, b");
        assert_eq!(
            pairs,
            vec![
                row(vec![vertex(1), vertex(3)]),
                row(vec![vertex(1), vertex(3)]),
                row(vec![vertex(3), vertex(1)])
            ]
        );
        assert!(read(&db, &cx, &issuer, &token, "MATCH (a)-[:S]->(b) RETURN a, b").is_empty());
        // A hidden transit vertex cannot manufacture an a=1,b=3 two-hop answer.
        let two = read(
            &db,
            &cx,
            &issuer,
            &token,
            "MATCH (a)-[:R]->(x)-[:R]->(b) RETURN a, b",
        );
        assert_eq!(
            two,
            vec![
                row(vec![vertex(1), vertex(1)]),
                row(vec![vertex(1), vertex(1)]),
                row(vec![vertex(3), vertex(3)]),
                row(vec![vertex(3), vertex(3)])
            ]
        );
        assert!(
            db.execute_graph_pattern_governed(
                &cx,
                &pattern("MATCH (a)-[:R]->(x)-[:R]->(b) RETURN a, b"),
                policy()
            )
            .unwrap()
            .value
            .contains(&row(vec![vertex(1), vertex(3)])),
            "raw control must expose the hidden transit path"
        );
        let edge = read(
            &db,
            &cx,
            &issuer,
            &token,
            "MATCH (a)-[e:R]->(b) RETURN e.p AS p, e.hidden AS hidden",
        );
        assert_eq!(edge, vec![row(vec![scalar(5), null()]); 3]);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn optional_and_negative_existence_observe_the_visible_graph() {
    let ((), report) = run_async_under_lab(0x5ec0_1002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let db = database(&contexts.commit()).await;
        let issuer = authority(92, NS);
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let none = token
            .attenuate(Restriction::Relations(Scope::only([])))
            .unwrap();
        assert_eq!(
            read(
                &db,
                &cx,
                &issuer,
                &none,
                "MATCH (a) OPTIONAL MATCH (a)-[:R]->(b) RETURN a, b"
            ),
            vec![row(vec![vertex(1), null()]), row(vec![vertex(3), null()])]
        );
        assert_eq!(
            read(
                &db,
                &cx,
                &issuer,
                &none,
                "MATCH (a) WHERE NOT EXISTS { MATCH (a)-[:R]->(b) } RETURN a"
            ),
            vec![row(vec![vertex(1)]), row(vec![vertex(3)])]
        );
        assert!(
            read(
                &db,
                &cx,
                &issuer,
                &token,
                "MATCH (a) WHERE NOT EXISTS { MATCH (a)-[:R]->(b) } RETURN a"
            )
            .is_empty()
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn history_is_resolved_before_scope_and_compaction_preserves_authorized_answers() {
    let ((), report) = run_async_under_lab(0x5ec0_1003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = database(&commit).await;
        let before = db.frontier().unwrap();
        let issuer = authority(93, NS);
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let p = pattern("MATCH (n) RETURN n, n.p AS p");
        let expected = vec![
            row(vec![vertex(1), scalar(10)]),
            row(vec![vertex(3), scalar(30)]),
        ];
        let mut change = WriteBatch::new(RelationId(1));
        change.set_vertex_label(VId(1), LabelId(1), false);
        change.set_vertex_label(VId(2), LabelId(1), true);
        change.set_vertex_property(VId(1), PropertyKeyId(1), Some(CanonicalScalar::Int(500)));
        db.write(&commit, change).await.unwrap();
        let current = vec![
            row(vec![vertex(2), scalar(20)]),
            row(vec![vertex(3), scalar(30)]),
        ];
        for compacted in [false, true] {
            if compacted {
                db.compact(&commit).await.unwrap();
            }
            assert_eq!(
                db.execute_graph_pattern_authorized_at(
                    &cx,
                    &issuer,
                    &token,
                    BRANCH,
                    &p,
                    before,
                    policy(),
                    || 100
                )
                .unwrap(),
                expected
            );
            assert_eq!(
                db.execute_graph_pattern_authorized(
                    &cx,
                    &issuer,
                    &token,
                    BRANCH,
                    &p,
                    policy(),
                    || 100
                )
                .unwrap(),
                current
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn namespace_signature_branch_rights_and_expiry_precede_frontier_admission() {
    let ((), report) = run_async_under_lab(0x5ec0_1004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let db = database(&contexts.commit()).await;
        let p = pattern("MATCH (n) RETURN n LIMIT 0");
        let issuer = authority(94, NS);
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let foreign = authority(94, DatabaseSecurityNamespaceId([8; 32]));
        assert!(matches!(
            db.execute_graph_pattern_authorized_at(
                &cx,
                &foreign,
                &token,
                BRANCH,
                &p,
                CommitSeq(u64::MAX),
                policy(),
                || panic!("namespace check must not call the clock")
            ),
            Err(QueryError::Authorization(Error::WrongAuthority))
        ));
        let forged = authority(95, NS).issue_at(&grant(), 100).unwrap();
        assert!(matches!(
            db.execute_graph_pattern_authorized(
                &cx,
                &issuer,
                &forged,
                BRANCH,
                &p,
                policy(),
                || 100
            ),
            Err(QueryError::Authorization(Error::Unauthenticated))
        ));
        assert!(matches!(
            db.execute_graph_pattern_authorized(
                &cx,
                &issuer,
                &token,
                "other",
                &p,
                policy(),
                || 100
            ),
            Err(QueryError::Authorization(Error::ScopeDenied))
        ));
        assert!(matches!(
            db.execute_graph_pattern_authorized_at(
                &cx,
                &issuer,
                &token,
                BRANCH,
                &p,
                CommitSeq(u64::MAX),
                policy(),
                || 1000
            ),
            Err(QueryError::Authorization(Error::Expired))
        ));
        assert!(matches!(
            db.execute_graph_pattern_authorized_at(
                &cx,
                &issuer,
                &token,
                BRANCH,
                &p,
                CommitSeq(u64::MAX),
                policy(),
                || 100
            ),
            Err(QueryError::Read(ReadError::BeyondFrontier { .. }))
        ));
        let write_only = token.attenuate(Restriction::Rights(Rights::Write)).unwrap();
        assert!(matches!(
            db.execute_graph_pattern_authorized(
                &cx,
                &issuer,
                &write_only,
                BRANCH,
                &p,
                policy(),
                || 100
            ),
            Err(QueryError::Authorization(Error::PermissionDenied))
        ));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn signed_nodes_work_and_rows_and_native_limits_are_independent_fail_closed_bounds() {
    let ((), report) = run_async_under_lab(0x5ec0_1005, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let db = database(&contexts.commit()).await;
        let p = pattern("MATCH (n) RETURN n");
        let issuer = authority(96, NS);
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let exact = token
            .attenuate(Restriction::MaxNodes(2))
            .unwrap()
            .attenuate(Restriction::MaxRows(2))
            .unwrap();
        assert_eq!(
            db.execute_graph_pattern_authorized(&cx, &issuer, &exact, BRANCH, &p, policy(), || 100)
                .unwrap()
                .len(),
            2
        );
        for (restriction, dimension) in [
            (Restriction::MaxNodes(1), LimitDimension::Nodes),
            (Restriction::MaxRows(1), LimitDimension::Rows),
            (Restriction::MaxWork(0), LimitDimension::Work),
        ] {
            let denied = token.attenuate(restriction).unwrap();
            assert!(
                matches!(db.execute_graph_pattern_authorized(&cx, &issuer, &denied, BRANCH, &p, policy(), || 100),
                Err(QueryError::Authorization(Error::LimitExceeded(actual))) if actual == dimension)
            );
        }
        assert!(matches!(
            db.execute_graph_pattern_authorized(
                &cx,
                &issuer,
                &token,
                BRANCH,
                &p,
                GqlQueryPolicy::new(1000, 1000, 0, 1000),
                || 100
            ),
            Err(QueryError::Pattern(GqlQueryError::Evaluator(_)))
        ));
        assert!(matches!(
            db.execute_graph_pattern_authorized(
                &cx,
                &issuer,
                &token,
                BRANCH,
                &p,
                GqlQueryPolicy::new(1, 1000, 1_000_000, 1_000_000),
                || 100
            ),
            Err(QueryError::Pattern(GqlQueryError::Rows(_)))
        ));
        assert!(
            db.execute_graph_pattern_authorized(
                &cx,
                &issuer,
                &token.attenuate(Restriction::MaxRows(0)).unwrap(),
                BRANCH,
                &pattern("MATCH (n) RETURN n LIMIT 0"),
                policy(),
                || 100
            )
            .unwrap()
            .is_empty()
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn original_label_conjunctions_do_not_leak_hidden_label_names() {
    let ((), report) = run_async_under_lab(0x5ec0_1006, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let db = database(&contexts.commit()).await;
        let issuer = authority(97, NS);
        let token = issuer.issue_at(&grant(), 100).unwrap();
        // Label 99 has no catalog binding in this definition and must not fail.
        let visible = pattern("MATCH (n:L) RETURN labels(n) AS labels");
        let labels = GraphValue::List(
            vec![GraphValue::Scalar(
                CanonicalScalar::ucs_basic_text("L").unwrap(),
            )]
            .into_boxed_slice(),
        );
        assert_eq!(
            db.execute_graph_pattern_authorized(
                &cx,
                &issuer,
                &token,
                BRANCH,
                &visible,
                policy(),
                || 100
            )
            .unwrap(),
            vec![row(vec![labels.clone()]), row(vec![labels])]
        );
        let both = token
            .attenuate(Restriction::Labels(Scope::only([LabelId(99)])))
            .unwrap();
        // Node 1 satisfies both original-label clauses, though no label is
        // individually visible in the intersection. An output-only filter or
        // authorization over already-masked labels would get this wrong.
        assert_eq!(
            read(
                &db,
                &cx,
                &issuer,
                &both,
                "MATCH (n) RETURN n, labels(n) AS labels"
            ),
            vec![row(vec![
                vertex(1),
                GraphValue::List(Vec::new().into_boxed_slice())
            ])]
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn every_clock_boundary_observes_expiry_and_final_delivery_observes_retirement() {
    let ((), report) = run_async_under_lab(0x5ec0_1007, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let db = database(&contexts.commit()).await;
        let p = pattern("MATCH (a)-[:R]->(b) RETURN a, b");
        let issuer = authority(98, NS);
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let mut calls = 0;
        let expected = db
            .execute_graph_pattern_authorized(&cx, &issuer, &token, BRANCH, &p, policy(), || {
                calls += 1;
                100
            })
            .unwrap();
        assert!(!expected.is_empty());
        assert!(calls > 10);
        for stop in 1..=calls {
            let mut seen = 0;
            assert!(
                matches!(
                    db.execute_graph_pattern_authorized(
                        &cx,
                        &issuer,
                        &token,
                        BRANCH,
                        &p,
                        policy(),
                        || {
                            seen += 1;
                            if seen == stop { 1000 } else { 100 }
                        }
                    ),
                    Err(QueryError::Authorization(Error::Expired))
                ),
                "stop={stop}"
            );
            assert_eq!(seen, stop);
        }
        let mut seen = 0;
        assert!(matches!(
            db.execute_graph_pattern_authorized(&cx, &issuer, &token, BRANCH, &p, policy(), || {
                seen += 1;
                if seen == 2 { 99 } else { 100 }
            }),
            Err(QueryError::Authorization(Error::ClockWentBackwards))
        ));
        let mut seen = 0;
        assert!(matches!(
            db.execute_graph_pattern_authorized(&cx, &issuer, &token, BRANCH, &p, policy(), || {
                seen += 1;
                if seen == calls {
                    issuer.retire();
                }
                100
            }),
            Err(QueryError::Authorization(Error::AuthorityRetired))
        ));
        assert!(matches!(
            db.execute_graph_pattern_authorized(&cx, &issuer, &token, BRANCH, &p, policy(), || 100),
            Err(QueryError::Authorization(Error::AuthorityRetired))
        ));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn all_six_set_and_join_kinds_consume_only_the_visible_complete_inputs() {
    let ((), report) = run_async_under_lab(0x5ec0_2001, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let db = database(&c.commit()).await;
        let issuer = authority(101, NS);
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let left =
            PreparedGraphSet::from(pattern("MATCH (n) RETURN n, n.p AS p, n.hidden AS hidden"));
        let right = PreparedGraphSet::from(pattern(
            "MATCH (n) WHERE n.p >= 20 RETURN n, n.p AS p, n.hidden AS hidden",
        ));
        let a = row(vec![vertex(1), scalar(10), null()]);
        let b = row(vec![vertex(3), scalar(30), null()]);
        for (operation, quantifier, expected) in [
            (
                GraphSetOperation::Union,
                GraphSetQuantifier::All,
                vec![a.clone(), b.clone(), b.clone()],
            ),
            (
                GraphSetOperation::Union,
                GraphSetQuantifier::Distinct,
                vec![a.clone(), b.clone()],
            ),
            (
                GraphSetOperation::Intersect,
                GraphSetQuantifier::All,
                vec![b.clone()],
            ),
            (
                GraphSetOperation::Intersect,
                GraphSetQuantifier::Distinct,
                vec![b.clone()],
            ),
            (
                GraphSetOperation::Except,
                GraphSetQuantifier::All,
                vec![a.clone()],
            ),
            (
                GraphSetOperation::Except,
                GraphSetQuantifier::Distinct,
                vec![a.clone()],
            ),
        ] {
            let query = left
                .clone()
                .combine(operation, quantifier, right.clone())
                .unwrap();
            assert_eq!(
                db.execute_graph_set_authorized(
                    &cx,
                    &issuer,
                    &token,
                    BRANCH,
                    &query,
                    policy(),
                    || 100
                )
                .unwrap(),
                expected
            );
        }
        let matched = row(vec![
            vertex(3),
            scalar(30),
            null(),
            vertex(3),
            scalar(30),
            null(),
        ]);
        let unmatched = row(vec![vertex(1), scalar(10), null(), null(), null(), null()]);
        for (kind, expected) in [
            (RowJoinKind::Inner, vec![matched.clone()]),
            (RowJoinKind::Left, vec![unmatched.clone(), matched.clone()]),
            (RowJoinKind::Right, vec![matched.clone()]),
            (RowJoinKind::Full, vec![unmatched, matched]),
            (RowJoinKind::Semi, vec![b]),
            (RowJoinKind::Anti, vec![a]),
        ] {
            let spec = RowJoinSpec::new(left.column_types(), right.column_types(), &[(0, 0)])
                .unwrap()
                .with_kind(kind);
            let query = left.clone().join(right.clone(), spec).unwrap();
            assert_eq!(
                db.execute_graph_set_authorized(
                    &cx,
                    &issuer,
                    &token,
                    BRANCH,
                    &query,
                    policy(),
                    || 100
                )
                .unwrap(),
                expected
            );
        }
        // An equality on a forbidden key must not match two raw equal values.
        let spec = RowJoinSpec::new(left.column_types(), right.column_types(), &[(2, 2)]).unwrap();
        let query = left.join(right, spec).unwrap();
        assert!(
            !db.execute_graph_set_governed(&cx, &query, policy())
                .unwrap()
                .value
                .is_empty()
        );
        assert!(
            db.execute_graph_set_authorized(&cx, &issuer, &token, BRANCH, &query, policy(), || 100)
                .unwrap()
                .is_empty()
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

fn double_visible() -> PreparedGraphSet {
    let leaf = PreparedGraphSet::from(pattern("MATCH (n) RETURN n.p AS p, n.hidden AS hidden"));
    leaf.clone()
        .combine(GraphSetOperation::Union, GraphSetQuantifier::All, leaf)
        .unwrap()
}
fn totals(input: PreparedGraphSet) -> PreparedGraphSetAggregate {
    PreparedGraphSetAggregate::prepare(
        input,
        &[],
        &[
            GraphAggregate::count_rows("rows"),
            GraphAggregate::count("hidden_count", 1),
            GraphAggregate::sum_int("sum", 0),
            GraphAggregate::collect("hidden_values", 1),
        ],
        0,
        None,
    )
    .unwrap()
}

#[test]
fn signed_source_limits_span_leaves_while_delivery_counts_only_the_final_page_or_groups() {
    let ((), report) = run_async_under_lab(0x5ec0_2002, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let db = database(&c.commit()).await;
        let issuer = authority(102, NS);
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let all = double_visible();
        let page = all.clone().with_page(1, Some(1));
        let exact = token
            .attenuate(Restriction::MaxNodes(4))
            .unwrap()
            .attenuate(Restriction::MaxRows(1))
            .unwrap();
        assert_eq!(
            db.execute_graph_set_authorized(&cx, &issuer, &exact, BRANCH, &page, policy(), || 100)
                .unwrap(),
            vec![row(vec![scalar(10), null()])]
        );
        let denied = exact.attenuate(Restriction::MaxNodes(3)).unwrap();
        assert!(matches!(
            db.execute_graph_set_authorized(&cx, &issuer, &denied, BRANCH, &page, policy(), || 100),
            Err(QueryError::Authorization(Error::LimitExceeded(
                LimitDimension::Nodes
            )))
        ));
        assert!(matches!(
            db.execute_graph_set_authorized(&cx, &issuer, &exact, BRANCH, &all, policy(), || 100),
            Err(QueryError::Authorization(Error::LimitExceeded(
                LimitDimension::Rows
            )))
        ));
        let summary = totals(all.clone());
        let result = db
            .execute_graph_set_aggregate_authorized(
                &cx,
                &issuer,
                &exact,
                BRANCH,
                &summary,
                GqlQueryPolicy::new(4, 1, 1_000_000, 1_000_000),
                || 100,
            )
            .unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(
            result[0].values(),
            &[
                GraphAggregateValue::Count(4),
                GraphAggregateValue::Count(0),
                GraphAggregateValue::Integer(80),
                GraphAggregateValue::Value(GraphValue::List(Box::new([]))),
            ]
        );
        assert!(matches!(
            db.execute_graph_set_aggregate_authorized(
                &cx,
                &issuer,
                &denied,
                BRANCH,
                &summary,
                policy(),
                || 100
            ),
            Err(QueryError::Authorization(Error::LimitExceeded(
                LimitDimension::Nodes
            )))
        ));
        // The same native source limit is cumulative too; the second leaf
        // reports the original 3-record allowance, not a reset or remainder.
        for aggregate in [false, true] {
            let limit = GqlQueryPolicy::new(3, 1000, 1_000_000, 1_000_000);
            let error = if aggregate {
                db.execute_graph_set_aggregate_authorized(
                    &cx,
                    &issuer,
                    &token,
                    BRANCH,
                    &summary,
                    limit,
                    || 100,
                )
                .unwrap_err()
            } else {
                db.execute_graph_set_authorized(&cx, &issuer, &token, BRANCH, &all, limit, || 100)
                    .unwrap_err()
            };
            match error {
                QueryError::Set(GqlQueryError::Rows(error))
                | QueryError::Aggregate(GqlQueryError::Rows(error)) => {
                    assert_eq!((error.limit, error.observed), (3, 4));
                }
                _ => panic!("native compound source budget must retain its error class"),
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn masked_values_define_groups_and_counts_instead_of_filtering_finished_aggregates() {
    let ((), report) = run_async_under_lab(0x5ec0_2003, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let db = database(&c.commit()).await;
        let issuer = authority(103, NS);
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let groups = PreparedGraphSetAggregate::prepare(
            double_visible(),
            &[1],
            &[
                GraphAggregate::count_rows("rows"),
                GraphAggregate::sum_int("sum", 0),
            ],
            0,
            None,
        )
        .unwrap();
        let actual = db
            .execute_graph_set_aggregate_authorized(
                &cx,
                &issuer,
                &token,
                BRANCH,
                &groups,
                policy(),
                || 100,
            )
            .unwrap();
        assert_eq!(actual.len(), 1);
        assert_eq!(actual[0].keys(), &[null()]);
        assert_eq!(
            actual[0].values(),
            &[
                GraphAggregateValue::Count(4),
                GraphAggregateValue::Integer(80)
            ]
        );
        let raw = db
            .execute_graph_set_aggregate_governed(&cx, &groups, policy())
            .unwrap()
            .value;
        assert_eq!(raw[0].keys(), &[scalar(777)]);
        assert_eq!(
            raw[0].values(),
            &[
                GraphAggregateValue::Count(6),
                GraphAggregateValue::Integer(120)
            ]
        );
        // A denied relation contributes no inputs, but empty global COUNT
        // still returns one authorized result row and spends a row allowance.
        let hidden = PreparedGraphSet::from(pattern("MATCH (a)-[:S]->(b) RETURN a"));
        let empty = PreparedGraphSetAggregate::prepare(
            hidden,
            &[],
            &[GraphAggregate::count_rows("rows")],
            0,
            None,
        )
        .unwrap();
        let actual = db
            .execute_graph_set_aggregate_authorized(
                &cx,
                &issuer,
                &token,
                BRANCH,
                &empty,
                policy(),
                || 100,
            )
            .unwrap();
        assert_eq!(actual[0].get(0).unwrap().as_count(), Some(0));
        let zero = token.attenuate(Restriction::MaxRows(0)).unwrap();
        assert!(matches!(
            db.execute_graph_set_aggregate_authorized(
                &cx,
                &issuer,
                &zero,
                BRANCH,
                &empty,
                policy(),
                || 100
            ),
            Err(QueryError::Authorization(Error::LimitExceeded(
                LimitDimension::Rows
            )))
        ));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn every_leaf_and_group_uses_the_same_historical_cut_after_scope_changes() {
    let ((), report) = run_async_under_lab(0x5ec0_2004, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = database(&commit).await;
        let at = db.frontier().unwrap();
        let issuer = authority(104, NS);
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let relation = double_visible();
        let summary = totals(relation.clone());
        let mut change = WriteBatch::new(RelationId(1));
        change.set_vertex_label(VId(1), LabelId(1), false);
        change.set_vertex_label(VId(2), LabelId(1), true);
        change.set_vertex_property(VId(1), PropertyKeyId(1), Some(CanonicalScalar::Int(500)));
        db.write(&commit, change).await.unwrap();
        db.compact(&commit).await.unwrap();
        let historical = db
            .execute_graph_set_authorized_at(
                &cx,
                &issuer,
                &token,
                BRANCH,
                &relation,
                at,
                policy(),
                || 100,
            )
            .unwrap();
        assert_eq!(
            historical,
            vec![
                row(vec![scalar(10), null()]),
                row(vec![scalar(10), null()]),
                row(vec![scalar(30), null()]),
                row(vec![scalar(30), null()])
            ]
        );
        let old = db
            .execute_graph_set_aggregate_authorized_at(
                &cx,
                &issuer,
                &token,
                BRANCH,
                &summary,
                at,
                policy(),
                || 100,
            )
            .unwrap();
        let new = db
            .execute_graph_set_aggregate_authorized(
                &cx,
                &issuer,
                &token,
                BRANCH,
                &summary,
                policy(),
                || 100,
            )
            .unwrap();
        assert_eq!(old[0].get(2).unwrap().as_integer(), Some(80));
        assert_eq!(new[0].get(2).unwrap().as_integer(), Some(100));
        issuer.retire();
        assert!(matches!(
            db.execute_graph_set_aggregate_authorized_at(
                &cx,
                &issuer,
                &token,
                BRANCH,
                &summary,
                at,
                policy(),
                || 100
            ),
            Err(QueryError::Authorization(Error::AuthorityRetired))
        ));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn source_free_queries_are_authenticated_and_late_errors_survive_empty_pages() {
    let ((), report) = run_async_under_lab(0x5ec0_2005, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let db = database(&c.commit()).await;
        let issuer = authority(105, NS);
        let token = issuer
            .issue_at(&grant(), 100)
            .unwrap()
            .attenuate(Restriction::MaxNodes(0))
            .unwrap();
        let values = PreparedGraphSet::singleton()
            .unwind(
                "value".into(),
                GraphSetValue::List(vec![
                    GraphSetValue::Value(scalar(3)),
                    GraphSetValue::Value(scalar(1)),
                ]),
            )
            .unwrap();
        assert_eq!(
            db.execute_graph_set_authorized(
                &cx,
                &issuer,
                &token,
                BRANCH,
                &values,
                policy(),
                || 100
            )
            .unwrap(),
            vec![row(vec![scalar(3)]), row(vec![scalar(1)])]
        );
        let total = PreparedGraphSetAggregate::prepare(
            values.clone(),
            &[],
            &[GraphAggregate::count_rows("rows")],
            0,
            None,
        )
        .unwrap();
        assert_eq!(
            db.execute_graph_set_aggregate_authorized(
                &cx,
                &issuer,
                &token,
                BRANCH,
                &total,
                policy(),
                || 100
            )
            .unwrap()[0]
                .get(0)
                .unwrap()
                .as_count(),
            Some(2)
        );
        // UNWIND a non-list is a late value error, not permission to omit
        // the second child because the first child or final page is empty.
        let bad = PreparedGraphSet::singleton()
            .unwind("value".into(), GraphSetValue::Value(scalar(9)))
            .unwrap();
        let late = values
            .with_page(0, Some(0))
            .combine(GraphSetOperation::Union, GraphSetQuantifier::All, bad)
            .unwrap()
            .with_page(0, Some(0));
        assert!(matches!(
            db.execute_graph_set_authorized(&cx, &issuer, &token, BRANCH, &late, policy(), || 100),
            Err(QueryError::Set(GqlQueryError::Source(
                GraphSetExecutionError::Projection { .. }
            )))
        ));
        let aggregate = PreparedGraphSetAggregate::prepare(
            late,
            &[],
            &[GraphAggregate::count_rows("rows")],
            0,
            Some(0),
        )
        .unwrap();
        assert!(matches!(
            db.execute_graph_set_aggregate_authorized(
                &cx,
                &issuer,
                &token,
                BRANCH,
                &aggregate,
                policy(),
                || 100
            ),
            Err(QueryError::Aggregate(GqlQueryError::Source(
                GraphAggregateError::InputRelation(GraphSetExecutionError::Projection { .. })
            )))
        ));
        assert!(matches!(
            db.execute_graph_set_aggregate_authorized(
                &cx,
                &issuer,
                &token,
                "wrong",
                &aggregate,
                policy(),
                || 100
            ),
            Err(QueryError::Authorization(Error::ScopeDenied))
        ));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn compound_work_and_expiry_and_final_group_delivery_use_one_live_permit() {
    let ((), report) = run_async_under_lab(0x5ec0_2006, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let db = database(&c.commit()).await;
        let query = totals(double_visible());
        let issuer = authority(106, NS);
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let mut calls = 0;
        let expected = db
            .execute_graph_set_aggregate_authorized(
                &cx,
                &issuer,
                &token,
                BRANCH,
                &query,
                policy(),
                || {
                    calls += 1;
                    100
                },
            )
            .unwrap();
        assert_eq!(expected.len(), 1);
        // Four admitted vertices, one row-delivery charge and one verification
        // sample are the six clock calls that are not work checkpoints.
        let work = calls - 6;
        let exact = token.attenuate(Restriction::MaxWork(work)).unwrap();
        assert_eq!(
            db.execute_graph_set_aggregate_authorized(
                &cx,
                &issuer,
                &exact,
                BRANCH,
                &query,
                policy(),
                || 100
            )
            .unwrap(),
            expected
        );
        let short = token.attenuate(Restriction::MaxWork(work - 1)).unwrap();
        assert!(matches!(
            db.execute_graph_set_aggregate_authorized(
                &cx,
                &issuer,
                &short,
                BRANCH,
                &query,
                policy(),
                || 100
            ),
            Err(QueryError::Authorization(Error::LimitExceeded(
                LimitDimension::Work
            )))
        ));
        for stop in 1..=calls {
            let mut seen = 0;
            assert!(
                matches!(
                    db.execute_graph_set_aggregate_authorized(
                        &cx,
                        &issuer,
                        &token,
                        BRANCH,
                        &query,
                        policy(),
                        || {
                            seen += 1;
                            if seen == stop { 1000 } else { 100 }
                        }
                    ),
                    Err(QueryError::Authorization(Error::Expired))
                ),
                "stop={stop}"
            );
            assert_eq!(seen, stop);
        }
        let mut seen = 0;
        assert!(matches!(
            db.execute_graph_set_aggregate_authorized(
                &cx,
                &issuer,
                &token,
                BRANCH,
                &query,
                policy(),
                || {
                    seen += 1;
                    if seen == calls {
                        issuer.retire();
                    }
                    100
                }
            ),
            Err(QueryError::Authorization(Error::AuthorityRetired))
        ));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
