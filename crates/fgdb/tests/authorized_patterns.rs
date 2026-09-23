//! Real committed graph reads through the signed Warden boundary.
//! No claim that these resident-source tests prove physical side-channel isolation.
use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb::{Database, DatabaseKeys, MemVfs, QueryError, ReadError, WriteBatch};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId, SchemaEpoch};
use fgdb_gql::algebra::{GraphValue, GraphValueRow, PreparedGraphPattern};
use fgdb_gql::{GqlParameters, GqlQueryError, GqlQueryPolicy, GraphSymbol, GraphSymbolKind, PreparedGraphText};
use fgdb_types::{CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts, QueryCx, VId};
use fgdb_warden::{Authority, CapabilityToken, Error, Grant, LimitDimension, QueryLimits, Restriction, Rights, Scope};

const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([7; 32]);
const BRANCH: &str = "host-selected-branch";
fn authority(seed: u64, namespace: DatabaseSecurityNamespaceId) -> Authority {
    Authority::new(AuthKey::from_seed(seed), namespace, "host-graph", SchemaEpoch(0), 1).unwrap()
}
fn grant() -> Grant {
    let mut grant = Grant::read_only(BRANCH, 1000, QueryLimits {
        max_nodes: 1000, max_work: 1_000_000, max_rows: 1000,
    });
    grant.labels = Scope::only([LabelId(1)]);
    grant.relations = Scope::only([RelationId(1)]);
    grant.properties = Scope::only([PropertyKeyId(1)]);
    grant
}
fn policy() -> GqlQueryPolicy { GqlQueryPolicy::new(1000, 1000, 1_000_000, 1_000_000) }
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
    PreparedGraphText::prepare(text, symbols).unwrap().bind_parameters(&GqlParameters::new()).unwrap().with_duplicates()
}
async fn database(cx: &CommitCx) -> Database<MemVfs> {
    let mut db = Database::open_memory(cx, DatabaseKeys::new([0x45; 32], NS, [0x73; 32])).await.unwrap();
    let mut initial = WriteBatch::new(RelationId(1));
    for (vid, labels, p) in [(1, vec![LabelId(1), LabelId(99)], 10), (2, vec![LabelId(99)], 20), (3, vec![LabelId(1)], 30)] {
        initial.create_vertex(VId(vid), labels, vec![
            (PropertyKeyId(1), CanonicalScalar::Int(p)),
            (PropertyKeyId(2), CanonicalScalar::Int(777)),
        ]);
    }
    for (eid, src, dst) in [(10, 1, 2), (11, 2, 3), (12, 1, 3), (13, 1, 3), (14, 3, 1)] {
        initial.add_edge(EId(eid), VId(src), VId(dst), vec![
            (PropertyKeyId(1), CanonicalScalar::Int(5)),
            (PropertyKeyId(2), CanonicalScalar::Int(999)),
        ]);
    }
    db.write(cx, initial).await.unwrap();
    let mut other = WriteBatch::new(RelationId(2));
    other.add_edge(EId(20), VId(1), VId(3), vec![]);
    db.write(cx, other).await.unwrap();
    db
}
fn row(values: Vec<GraphValue>) -> GraphValueRow { GraphValueRow::from_owned_values(values) }
fn vertex(id: u128) -> GraphValue { GraphValue::Vertex(VId(id)) }
fn scalar(value: i64) -> GraphValue { GraphValue::Scalar(CanonicalScalar::Int(value)) }
fn null() -> GraphValue { GraphValue::Scalar(CanonicalScalar::Null) }
fn read(db: &Database<MemVfs>, cx: &QueryCx, issuer: &Authority, token: &CapabilityToken, text: &str) -> Vec<GraphValueRow> {
    db.execute_graph_pattern_authorized(cx, issuer, token, BRANCH, &pattern(text), policy(), || 100).unwrap()
}

#[test]
fn topology_and_metadata_are_masked_before_matching_not_after_projection() {
    let ((), report) = run_async_under_lab(0x5ec0_1001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let db = database(&contexts.commit()).await;
        let issuer = authority(91, NS); let token = issuer.issue_at(&grant(), 100).unwrap();
        assert_eq!(read(&db, &cx, &issuer, &token, "MATCH (n) RETURN n, n.p AS p, n.hidden AS hidden"),
            vec![row(vec![vertex(1), scalar(10), null()]), row(vec![vertex(3), scalar(30), null()])]);
        assert_eq!(read(&db, &cx, &issuer, &token, "MATCH (n) WHERE n.hidden = 777 OR n.p = 10 RETURN n"),
            vec![row(vec![vertex(1)])]);
        assert_eq!(read(&db, &cx, &issuer, &token, "MATCH (n) WHERE n.hidden IS NULL RETURN n"),
            vec![row(vec![vertex(1)]), row(vec![vertex(3)])]);
        assert!(read(&db, &cx, &issuer, &token, "MATCH (n:H) RETURN n").is_empty());
        let pairs = read(&db, &cx, &issuer, &token, "MATCH (a)-[:R]->(b) RETURN a, b");
        assert_eq!(pairs, vec![row(vec![vertex(1), vertex(3)]), row(vec![vertex(1), vertex(3)]), row(vec![vertex(3), vertex(1)])]);
        assert!(read(&db, &cx, &issuer, &token, "MATCH (a)-[:S]->(b) RETURN a, b").is_empty());
        // A hidden transit vertex cannot manufacture an a=1,b=3 two-hop answer.
        let two = read(&db, &cx, &issuer, &token, "MATCH (a)-[:R]->(x)-[:R]->(b) RETURN a, b");
        assert_eq!(two, vec![row(vec![vertex(1), vertex(1)]), row(vec![vertex(1), vertex(1)]),
            row(vec![vertex(3), vertex(3)]), row(vec![vertex(3), vertex(3)])]);
        assert!(db.execute_graph_pattern_governed(&cx, &pattern("MATCH (a)-[:R]->(x)-[:R]->(b) RETURN a, b"), policy()).unwrap().value
            .contains(&row(vec![vertex(1), vertex(3)])), "raw control must expose the hidden transit path");
        let edge = read(&db, &cx, &issuer, &token, "MATCH (a)-[e:R]->(b) RETURN e.p AS p, e.hidden AS hidden");
        assert_eq!(edge, vec![row(vec![scalar(5), null()]); 3]);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn optional_and_negative_existence_observe_the_visible_graph() {
    let ((), report) = run_async_under_lab(0x5ec0_1002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root); let cx = contexts.query();
        let db = database(&contexts.commit()).await;
        let issuer = authority(92, NS); let token = issuer.issue_at(&grant(), 100).unwrap();
        let none = token.attenuate(Restriction::Relations(Scope::only([]))).unwrap();
        assert_eq!(read(&db, &cx, &issuer, &none, "MATCH (a) OPTIONAL MATCH (a)-[:R]->(b) RETURN a, b"),
            vec![row(vec![vertex(1), null()]), row(vec![vertex(3), null()])]);
        assert_eq!(read(&db, &cx, &issuer, &none, "MATCH (a) WHERE NOT EXISTS { MATCH (a)-[:R]->(b) } RETURN a"),
            vec![row(vec![vertex(1)]), row(vec![vertex(3)])]);
        assert!(read(&db, &cx, &issuer, &token, "MATCH (a) WHERE NOT EXISTS { MATCH (a)-[:R]->(b) } RETURN a").is_empty());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn history_is_resolved_before_scope_and_compaction_preserves_authorized_answers() {
    let ((), report) = run_async_under_lab(0x5ec0_1003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root); let cx = contexts.query(); let commit = contexts.commit();
        let mut db = database(&commit).await;
        let before = db.frontier().unwrap();
        let issuer = authority(93, NS); let token = issuer.issue_at(&grant(), 100).unwrap();
        let p = pattern("MATCH (n) RETURN n, n.p AS p");
        let expected = vec![row(vec![vertex(1), scalar(10)]), row(vec![vertex(3), scalar(30)])];
        let mut change = WriteBatch::new(RelationId(1));
        change.set_vertex_label(VId(1), LabelId(1), false);
        change.set_vertex_label(VId(2), LabelId(1), true);
        change.set_vertex_property(VId(1), PropertyKeyId(1), Some(CanonicalScalar::Int(500)));
        db.write(&commit, change).await.unwrap();
        let current = vec![row(vec![vertex(2), scalar(20)]), row(vec![vertex(3), scalar(30)])];
        for compacted in [false, true] {
            if compacted { db.compact(&commit).await.unwrap(); }
            assert_eq!(db.execute_graph_pattern_authorized_at(&cx, &issuer, &token, BRANCH, &p, before, policy(), || 100).unwrap(), expected);
            assert_eq!(db.execute_graph_pattern_authorized(&cx, &issuer, &token, BRANCH, &p, policy(), || 100).unwrap(), current);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn namespace_signature_branch_rights_and_expiry_precede_frontier_admission() {
    let ((), report) = run_async_under_lab(0x5ec0_1004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root); let cx = contexts.query();
        let db = database(&contexts.commit()).await; let p = pattern("MATCH (n) RETURN n LIMIT 0");
        let issuer = authority(94, NS); let token = issuer.issue_at(&grant(), 100).unwrap();
        let foreign = authority(94, DatabaseSecurityNamespaceId([8; 32]));
        assert!(matches!(db.execute_graph_pattern_authorized_at(&cx, &foreign, &token, BRANCH, &p, CommitSeq(u64::MAX), policy(), || panic!("namespace check must not call the clock")),
            Err(QueryError::Authorization(Error::WrongAuthority))));
        let forged = authority(95, NS).issue_at(&grant(), 100).unwrap();
        assert!(matches!(db.execute_graph_pattern_authorized(&cx, &issuer, &forged, BRANCH, &p, policy(), || 100),
            Err(QueryError::Authorization(Error::Unauthenticated))));
        assert!(matches!(db.execute_graph_pattern_authorized(&cx, &issuer, &token, "other", &p, policy(), || 100),
            Err(QueryError::Authorization(Error::ScopeDenied))));
        assert!(matches!(db.execute_graph_pattern_authorized_at(&cx, &issuer, &token, BRANCH, &p, CommitSeq(u64::MAX), policy(), || 1000),
            Err(QueryError::Authorization(Error::Expired))));
        assert!(matches!(db.execute_graph_pattern_authorized_at(&cx, &issuer, &token, BRANCH, &p, CommitSeq(u64::MAX), policy(), || 100),
            Err(QueryError::Read(ReadError::BeyondFrontier { .. }))));
        let write_only = token.attenuate(Restriction::Rights(Rights::Write)).unwrap();
        assert!(matches!(db.execute_graph_pattern_authorized(&cx, &issuer, &write_only, BRANCH, &p, policy(), || 100),
            Err(QueryError::Authorization(Error::PermissionDenied))));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn signed_nodes_work_and_rows_and_native_limits_are_independent_fail_closed_bounds() {
    let ((), report) = run_async_under_lab(0x5ec0_1005, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root); let cx = contexts.query();
        let db = database(&contexts.commit()).await; let p = pattern("MATCH (n) RETURN n");
        let issuer = authority(96, NS); let token = issuer.issue_at(&grant(), 100).unwrap();
        let exact = token.attenuate(Restriction::MaxNodes(2)).unwrap().attenuate(Restriction::MaxRows(2)).unwrap();
        assert_eq!(db.execute_graph_pattern_authorized(&cx, &issuer, &exact, BRANCH, &p, policy(), || 100).unwrap().len(), 2);
        for (restriction, dimension) in [(Restriction::MaxNodes(1), LimitDimension::Nodes),
            (Restriction::MaxRows(1), LimitDimension::Rows), (Restriction::MaxWork(0), LimitDimension::Work)] {
            let denied = token.attenuate(restriction).unwrap();
            assert!(matches!(db.execute_graph_pattern_authorized(&cx, &issuer, &denied, BRANCH, &p, policy(), || 100),
                Err(QueryError::Authorization(Error::LimitExceeded(actual))) if actual == dimension));
        }
        assert!(matches!(db.execute_graph_pattern_authorized(&cx, &issuer, &token, BRANCH, &p,
            GqlQueryPolicy::new(1000, 1000, 0, 1000), || 100), Err(QueryError::Pattern(GqlQueryError::Evaluator(_)))));
        assert!(matches!(db.execute_graph_pattern_authorized(&cx, &issuer, &token, BRANCH, &p,
            GqlQueryPolicy::new(1, 1000, 1_000_000, 1_000_000), || 100), Err(QueryError::Pattern(GqlQueryError::Rows(_)))));
        assert!(db.execute_graph_pattern_authorized(&cx, &issuer, &token.attenuate(Restriction::MaxRows(0)).unwrap(), BRANCH,
            &pattern("MATCH (n) RETURN n LIMIT 0"), policy(), || 100).unwrap().is_empty());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn original_label_conjunctions_do_not_leak_hidden_label_names() {
    let ((), report) = run_async_under_lab(0x5ec0_1006, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root); let cx = contexts.query();
        let db = database(&contexts.commit()).await;
        let issuer = authority(97, NS); let token = issuer.issue_at(&grant(), 100).unwrap();
        // Label 99 has no catalog binding in this definition and must not fail.
        let visible = pattern("MATCH (n:L) RETURN labels(n) AS labels");
        let labels = GraphValue::List(vec![GraphValue::Scalar(CanonicalScalar::ucs_basic_text("L").unwrap())].into_boxed_slice());
        assert_eq!(db.execute_graph_pattern_authorized(&cx, &issuer, &token, BRANCH, &visible, policy(), || 100).unwrap(),
            vec![row(vec![labels.clone()]), row(vec![labels])]);
        let both = token.attenuate(Restriction::Labels(Scope::only([LabelId(99)]))).unwrap();
        // Node 1 satisfies both original-label clauses, though no label is
        // individually visible in the intersection. An output-only filter or
        // authorization over already-masked labels would get this wrong.
        assert_eq!(read(&db, &cx, &issuer, &both, "MATCH (n) RETURN n, labels(n) AS labels"),
            vec![row(vec![vertex(1), GraphValue::List(Vec::new().into_boxed_slice())])]);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn every_clock_boundary_observes_expiry_and_final_delivery_observes_retirement() {
    let ((), report) = run_async_under_lab(0x5ec0_1007, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root); let cx = contexts.query();
        let db = database(&contexts.commit()).await; let p = pattern("MATCH (a)-[:R]->(b) RETURN a, b");
        let issuer = authority(98, NS); let token = issuer.issue_at(&grant(), 100).unwrap();
        let mut calls = 0;
        let expected = db.execute_graph_pattern_authorized(&cx, &issuer, &token, BRANCH, &p, policy(), || { calls += 1; 100 }).unwrap();
        assert!(!expected.is_empty()); assert!(calls > 10);
        for stop in 1..=calls {
            let mut seen = 0;
            assert!(matches!(db.execute_graph_pattern_authorized(&cx, &issuer, &token, BRANCH, &p, policy(), || {
                seen += 1; if seen == stop { 1000 } else { 100 }
            }), Err(QueryError::Authorization(Error::Expired))), "stop={stop}");
            assert_eq!(seen, stop);
        }
        let mut seen = 0;
        assert!(matches!(db.execute_graph_pattern_authorized(&cx, &issuer, &token, BRANCH, &p, policy(), || {
            seen += 1; if seen == 2 { 99 } else { 100 }
        }), Err(QueryError::Authorization(Error::ClockWentBackwards))));
        let mut seen = 0;
        assert!(matches!(db.execute_graph_pattern_authorized(&cx, &issuer, &token, BRANCH, &p, policy(), || {
            seen += 1; if seen == calls { issuer.retire(); } 100
        }), Err(QueryError::Authorization(Error::AuthorityRetired))));
        assert!(matches!(db.execute_graph_pattern_authorized(&cx, &issuer, &token, BRANCH, &p, policy(), || 100),
            Err(QueryError::Authorization(Error::AuthorityRetired))));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
