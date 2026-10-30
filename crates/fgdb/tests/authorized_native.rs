//! Product reads through signed capabilities and complete native definitions.
//! These resident-source laws do not establish physical side-channel isolation.
use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb::{
    Database, DatabaseKeys, MemVfs, NativeReadClass, PreparedNativeRead, QueryError, QueryResult,
    QueryValue, ReadError, WriteBatch,
};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId, SchemaEpoch};
use fgdb_gql::algebra::{GraphValueRow, IntegerComparison, PreparedGraphPattern};
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GraphAggregate, GraphAggregateColumn,
    GraphAggregateFilter, GraphAggregateOrder, GraphAggregateTest, GraphSetProjection,
    GraphSetValue, GraphSymbol, GraphSymbolKind, PreparedGraphAggregate,
    PreparedGraphAggregateText, PreparedGraphText,
};
use fgdb_types::{
    CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId,
};
use fgdb_warden::{
    Authority, CapabilityToken, Error, Grant, LimitDimension, QueryLimits, Restriction, Rights,
    Scope,
};

const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x6d; 32]);
const BRANCH: &str = "host-branch";
fn authority(seed: u64) -> Authority {
    Authority::new(
        AuthKey::from_seed(seed),
        NS,
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
fn aggregate(text: &str) -> PreparedGraphAggregate {
    PreparedGraphAggregateText::prepare(text, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}
async fn database(cx: &CommitCx) -> Database<MemVfs> {
    let mut db = Database::open_memory(cx, DatabaseKeys::new([0x32; 32], NS, [0x67; 32]))
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
                (PropertyKeyId(2), CanonicalScalar::Int(p * 10)),
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

#[test]
fn native_graph_aggregate_arguments_and_hidden_transit_paths_use_scoped_sources() {
    let ((), report) = run_async_under_lab(0x5ec0_3001, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let db = database(&c.commit()).await;
        let issuer = authority(301);
        let token = issuer.issue_at(&grant(), 100).unwrap();
        for (text, count, sum) in [
            (
                "MATCH (n) RETURN count(*) AS rows, sum(n.p) AS total, count(n.hidden) AS hidden",
                2,
                40,
            ),
            (
                "MATCH (a)-[e:R]->(b) RETURN count(*) AS rows, sum(e.p) AS total, count(e.hidden) AS hidden",
                3,
                15,
            ),
        ] {
            let query = aggregate(text);
            let frozen = query.canonical_bytes();
            let rows = db
                .execute_graph_aggregate_authorized(
                    &cx,
                    &issuer,
                    &token,
                    BRANCH,
                    &query,
                    policy(),
                    || 100,
                )
                .unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].get(0).unwrap().as_count(), Some(count));
            assert_eq!(rows[0].get(1).unwrap().as_integer(), Some(sum));
            assert_eq!(rows[0].get(2).unwrap().as_count(), Some(0));
            assert_eq!(query.canonical_bytes(), frozen);
        }
        let query = aggregate("MATCH (a)-[:R]->(x)-[:R]->(b) RETURN count(*) AS rows");
        assert_eq!(
            db.execute_graph_aggregate_authorized(
                &cx,
                &issuer,
                &token,
                BRANCH,
                &query,
                policy(),
                || 100
            )
            .unwrap()[0]
                .get(0)
                .unwrap()
                .as_count(),
            Some(4)
        );
        assert_ne!(
            db.execute_graph_aggregate_governed(&cx, &query, policy())
                .unwrap()
                .value[0]
                .get(0)
                .unwrap()
                .as_count(),
            Some(4)
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn computed_group_keys_having_hidden_columns_and_order_remain_one_definition() {
    let ((), report) = run_async_under_lab(0x5ec0_3002, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let db = database(&c.commit()).await;
        let issuer = authority(302);
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let query = PreparedGraphAggregate::prepare_projected(
            pattern("MATCH (n) RETURN n.p AS p, n.hidden AS hidden"),
            vec![
                GraphSetProjection::new("key", GraphSetValue::Column(1)),
                GraphSetProjection::new("value", GraphSetValue::Column(0)),
            ],
            &[0],
            &[
                GraphAggregate::sum_int("total", 1),
                GraphAggregate::count_rows("rows"),
            ],
            0,
            Some(1),
        )
        .unwrap()
        .with_result_clauses(
            &[GraphAggregateFilter {
                column: GraphAggregateColumn::Aggregate(1),
                test: GraphAggregateTest::Integer {
                    comparison: IntegerComparison::GreaterOrEqual,
                    value: 2,
                },
            }],
            &[GraphAggregateOrder::descending(
                GraphAggregateColumn::Aggregate(0),
            )],
        )
        .unwrap()
        .with_key_output_columns(&[])
        .unwrap()
        .with_aggregate_output_prefix(1)
        .unwrap();
        let rows = db
            .execute_graph_aggregate_authorized(
                &cx,
                &issuer,
                &token,
                BRANCH,
                &query,
                policy(),
                || 100,
            )
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].keys().is_empty());
        assert_eq!(rows[0].values().len(), 1);
        assert_eq!(rows[0].get(0).unwrap().as_integer(), Some(40));
        assert!(
            db.execute_graph_aggregate_governed(&cx, &query, policy())
                .unwrap()
                .value
                .is_empty(),
            "grouping on original private values must disagree with masking before grouping"
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn historical_aggregate_scope_and_exact_delivery_limits_survive_compaction() {
    let ((), report) = run_async_under_lab(0x5ec0_3003, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = database(&commit).await;
        let issuer = authority(303);
        let mut allowed = grant();
        allowed.limits.max_rows = 1;
        allowed.limits.max_nodes = 2;
        let token = issuer.issue_at(&allowed, 100).unwrap();
        let query = aggregate("MATCH (n) RETURN sum(n.p) AS total");
        let basis = db.frontier().unwrap();
        let before = db
            .execute_graph_aggregate_authorized(
                &cx,
                &issuer,
                &token,
                BRANCH,
                &query,
                policy(),
                || 100,
            )
            .unwrap();
        assert_eq!(before[0].get(0).unwrap().as_integer(), Some(40));
        for (dimension, limits) in [
            (
                LimitDimension::Nodes,
                QueryLimits {
                    max_nodes: 1,
                    ..allowed.limits
                },
            ),
            (
                LimitDimension::Rows,
                QueryLimits {
                    max_rows: 0,
                    ..allowed.limits
                },
            ),
            (
                LimitDimension::Work,
                QueryLimits {
                    max_work: 0,
                    ..allowed.limits
                },
            ),
        ] {
            let mut denied = allowed.clone();
            denied.limits = limits;
            let denied = issuer.issue_at(&denied, 100).unwrap();
            assert!(
                matches!(db.execute_graph_aggregate_authorized(&cx, &issuer, &denied, BRANCH, &query, policy(), || 100),
                // ubs:ignore -- test assertion on a limit dimension, not secret material.
                Err(QueryError::Authorization(Error::LimitExceeded(actual))) if actual == dimension)
            );
        }
        assert!(matches!(
            db.execute_graph_aggregate_authorized(
                &cx,
                &issuer,
                &token,
                BRANCH,
                &query,
                GqlQueryPolicy::new(1000, 0, 1_000_000, 1_000_000),
                || 100
            ),
            Err(QueryError::Aggregate(GqlQueryError::Rows(_)))
        ));
        let mut change = WriteBatch::new(RelationId(1));
        change.set_vertex_label(VId(1), LabelId(1), false);
        db.write(&commit, change).await.unwrap();
        for compact in [false, true] {
            if compact {
                db.compact(&commit).await.unwrap();
            }
            assert_eq!(
                db.execute_graph_aggregate_authorized_at(
                    &cx,
                    &issuer,
                    &token,
                    BRANCH,
                    &query,
                    basis,
                    policy(),
                    || 100
                )
                .unwrap(),
                before
            );
            assert_eq!(
                db.execute_graph_aggregate_authorized(
                    &cx,
                    &issuer,
                    &token,
                    BRANCH,
                    &query,
                    policy(),
                    || 100
                )
                .unwrap()[0]
                    .get(0)
                    .unwrap()
                    .as_integer(),
                Some(30)
            );
        }
        assert!(matches!(
            db.execute_graph_aggregate_authorized_at(
                &cx,
                &issuer,
                &token,
                BRANCH,
                &query,
                CommitSeq(u64::MAX),
                policy(),
                || 100
            ),
            Err(QueryError::Read(ReadError::BeyondFrontier { .. }))
        ));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn every_aggregate_expiry_boundary_and_final_retirement_prevent_delivery() {
    let ((), report) = run_async_under_lab(0x5ec0_3004, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let db = database(&c.commit()).await;
        let issuer = authority(304);
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let query = aggregate("MATCH (n) RETURN sum(n.p) AS total");
        let mut calls = 0;
        let expected = db
            .execute_graph_aggregate_authorized(
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
        for stop in 1..=calls {
            let mut seen = 0;
            assert!(matches!(
                db.execute_graph_aggregate_authorized(
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
            ));
        }
        assert_eq!(
            db.execute_graph_aggregate_authorized(
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
        let mut seen = 0;
        assert!(matches!(
            db.execute_graph_aggregate_authorized(
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

fn native_rows(columns: &[&str], rows: Vec<Vec<QueryValue>>) -> QueryResult {
    QueryResult::Rows {
        columns: columns.iter().map(|name| (*name).to_owned()).collect(),
        rows,
    }
}
fn value(value: i64) -> QueryValue {
    QueryValue::Value(fgdb_gql::algebra::GraphValue::Scalar(CanonicalScalar::Int(
        value,
    )))
}
fn p_rows() -> QueryResult {
    native_rows(&["p"], vec![vec![value(10)], vec![value(30)]])
}

/// The physically filtered twin omits hidden labels, fields, relations and
/// transit vertices. Both contain 512 unrelated visible vertices, so a tiny
/// source allowance distinguishes identity admission from a corpus scan.
async fn identity_seed_database(cx: &CommitCx, hidden: bool) -> Database<MemVfs> {
    let mut db = Database::open_memory(cx, DatabaseKeys::new([0x72; 32], NS, [0x73; 32]))
        .await
        .unwrap();
    let mut batch = WriteBatch::new(RelationId(1));
    for id in [1, 3, 4].into_iter().chain(1000..1512) {
        let labels = if hidden && id == 1 {
            vec![LabelId(1), LabelId(99)]
        } else {
            vec![LabelId(1)]
        };
        let mut properties = vec![(PropertyKeyId(1), CanonicalScalar::Int(id as i64 * 10))];
        if hidden {
            properties.push((PropertyKeyId(2), CanonicalScalar::Int(999)));
        }
        batch.create_vertex(VId(id), labels, properties);
    }
    if hidden {
        batch.create_vertex(
            VId(2),
            vec![LabelId(99)],
            vec![(PropertyKeyId(1), CanonicalScalar::Int(20))],
        );
    }
    // Parallel edges and a cycle must retain the original bag and path laws.
    for (eid, from, to) in [(12, 1, 3), (13, 1, 3), (14, 3, 1)] {
        batch.add_edge(EId(eid), VId(from), VId(to), vec![]);
    }
    if hidden {
        for (eid, from, to) in [(10, 1, 2), (11, 2, 3), (15, 2, 4)] {
            batch.add_edge(EId(eid), VId(from), VId(to), vec![]);
        }
    }
    db.write(cx, batch).await.unwrap();
    if hidden {
        let mut denied_relation = WriteBatch::new(RelationId(2));
        denied_relation.add_edge(EId(20), VId(1), VId(1000), vec![]);
        db.write(cx, denied_relation).await.unwrap();
    }
    db
}

fn identity_seed_arguments() -> GqlParameters {
    use fgdb_gql::algebra::GraphValue;
    GqlParameters::new()
        .with_list(
            "nodes",
            vec![
                GraphValue::Vertex(VId(1)),
                GraphValue::Vertex(VId(1)),
                GraphValue::Vertex(VId(2)),
                GraphValue::Vertex(VId(999)),
                GraphValue::Scalar(CanonicalScalar::Null),
            ],
        )
        .unwrap()
}

#[test]
fn authorized_identity_seeds_are_reauthorized_and_admit_only_their_visible_roots() {
    let ((), report) = run_async_under_lab(0x5ec0_4021, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let full = identity_seed_database(&c.commit(), true).await;
        let filtered = identity_seed_database(&c.commit(), false).await;
        let issuer = authority(421);
        let mut allowed = grant();
        allowed.limits.max_nodes = 1;
        let token = issuer.issue_at(&allowed, 100).unwrap();
        let args = identity_seed_arguments();
        let text = "UNWIND $nodes AS n MATCH (n) RETURN n.p AS p, n.hidden AS hidden";
        let null = QueryValue::Value(fgdb_gql::algebra::GraphValue::Scalar(CanonicalScalar::Null));
        let expected = native_rows(
            &["p", "hidden"],
            vec![vec![value(10), null.clone()], vec![value(10), null]],
        );
        let small = GqlQueryPolicy::new(1, 1000, 1_000_000, 1_000_000);
        assert_eq!(
            filtered.query(&cx, text, &args, symbols, policy()).unwrap(),
            expected
        );
        for db in [&full, &filtered] {
            assert_eq!(
                db.query_authorized(
                    &cx,
                    &issuer,
                    &token,
                    BRANCH,
                    text,
                    &args,
                    symbols,
                    small,
                    || 100
                )
                .unwrap(),
                expected,
            );
        }
        // The record allowance remains live; an ordinary unbound leaf still
        // refuses after the first record rather than silently widening it.
        assert!(
            full.query_authorized(
                &cx,
                &issuer,
                &token,
                BRANCH,
                "MATCH (n) RETURN n.p AS p",
                &GqlParameters::new(),
                symbols,
                small,
                || 100,
            )
            .is_err()
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn authorized_identity_seeded_closures_preserve_directions_bags_and_aggregates() {
    let ((), report) = run_async_under_lab(0x5ec0_4022, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let full = identity_seed_database(&c.commit(), true).await;
        let filtered = identity_seed_database(&c.commit(), false).await;
        let issuer = authority(422);
        let mut allowed = grant();
        allowed.limits.max_nodes = 2;
        let token = issuer.issue_at(&allowed, 100).unwrap();
        let args = identity_seed_arguments();
        // Two visible vertices and at most their three original edges. Hidden
        // seed 2, its bridge to visible 4, and denied S->1000 cost no records.
        let small = GqlQueryPolicy::new(5, 1000, 1_000_000, 1_000_000);
        for (path, values) in [
            ("(n)-[:R]->(m)", vec![30; 4]),
            ("(n)-[]->(m)", vec![30; 4]),
            ("(n)<-[:R]-(m)", vec![30; 2]),
            ("(n)-[:R]-(m)", vec![30; 6]),
            ("(n)-[:R]->(x)-[:R]->(m)", vec![10; 4]),
            (
                "(n)-[:R]->{0,2}(m)",
                vec![10, 10, 10, 10, 10, 10, 30, 30, 30, 30],
            ),
        ] {
            let text = format!("UNWIND $nodes AS n MATCH {path} RETURN m.p AS p ORDER BY p");
            let expected =
                native_rows(&["p"], values.into_iter().map(|n| vec![value(n)]).collect());
            assert_eq!(
                filtered
                    .query(&cx, &text, &args, symbols, policy())
                    .unwrap(),
                expected,
                "{text}"
            );
            for db in [&full, &filtered] {
                assert_eq!(
                    db.query_authorized(
                        &cx,
                        &issuer,
                        &token,
                        BRANCH,
                        &text,
                        &args,
                        symbols,
                        small,
                        || 100
                    )
                    .unwrap(),
                    expected,
                    "{text}",
                );
            }
        }
        let text = "UNWIND $nodes AS n MATCH (n)-[:R]->(m) RETURN COUNT(*) AS n, COUNT(m.hidden) AS hidden";
        let expected = native_rows(
            &["n", "hidden"],
            vec![vec![QueryValue::Count(4), QueryValue::Count(0)]],
        );
        assert_eq!(
            filtered.query(&cx, text, &args, symbols, policy()).unwrap(),
            expected
        );
        for db in [&full, &filtered] {
            assert_eq!(
                db.query_authorized(
                    &cx,
                    &issuer,
                    &token,
                    BRANCH,
                    text,
                    &args,
                    symbols,
                    small,
                    || 100
                )
                .unwrap(),
                expected,
            );
        }
        allowed.limits.max_nodes = 1;
        let too_few_nodes = issuer.issue_at(&allowed, 100).unwrap();
        for tail in ["RETURN m.p AS p", "RETURN m.p AS p LIMIT 0"] {
            let text = format!("UNWIND $nodes AS n MATCH (n)-[:R]->(m) {tail}");
            assert!(matches!(
                full.query_authorized(
                    &cx,
                    &issuer,
                    &too_few_nodes,
                    BRANCH,
                    &text,
                    &args,
                    symbols,
                    small,
                    || 100
                ),
                Err(QueryError::Authorization(Error::LimitExceeded(
                    LimitDimension::Nodes
                )))
            ));
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn hidden_seeded_transit_does_not_change_the_signed_work_refusal_boundary() {
    let ((), report) = run_async_under_lab(0x5ec0_4023, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let mut full = identity_seed_database(&c.commit(), true).await;
        let filtered = identity_seed_database(&c.commit(), false).await;
        // More versions of a visible vertex's hidden field and of a wholly
        // hidden vertex leave the visible graph unchanged. Winner seeks and
        // source accounting must not expose that extra history through work.
        for version in 0..8 {
            let mut change = WriteBatch::new(RelationId(1));
            change.set_vertex_property(
                VId(1),
                PropertyKeyId(2),
                Some(CanonicalScalar::Int(version)),
            );
            change.set_vertex_property(
                VId(2),
                PropertyKeyId(1),
                Some(CanonicalScalar::Int(version)),
            );
            full.write(&c.commit(), change).await.unwrap();
        }
        let issuer = authority(423);
        let args = identity_seed_arguments();
        let text = "UNWIND $nodes AS n MATCH (n)-[]->{0,2}(m) RETURN m.p AS p ORDER BY p";
        let small = GqlQueryPolicy::new(5, 1000, 1_000_000, 1_000_000);
        let least_work = |db: &Database<MemVfs>| {
            let (mut low, mut high) = (1, 1_000_000);
            while low < high {
                let middle = low + (high - low) / 2;
                let mut allowed = grant();
                allowed.limits.max_nodes = 2;
                allowed.limits.max_work = middle;
                let token = issuer.issue_at(&allowed, 100).unwrap();
                match db.query_authorized(
                    &cx,
                    &issuer,
                    &token,
                    BRANCH,
                    text,
                    &args,
                    symbols,
                    small,
                    || 100,
                ) {
                    Ok(_) => high = middle,
                    Err(QueryError::Authorization(Error::LimitExceeded(LimitDimension::Work))) => {
                        low = middle + 1
                    }
                    other => panic!("unexpected work-threshold result: {other:?}"),
                }
            }
            low
        };
        let full_work = least_work(&full);
        assert!(full_work > 1 && full_work < 1_000_000);
        assert_eq!(full_work, least_work(&filtered));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn seeded_reads_choose_the_historical_winner_before_masking() {
    let ((), report) = run_async_under_lab(0x5ec0_4024, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = identity_seed_database(&commit, true).await;
        let issuer = authority(424);
        let mut allowed = grant();
        allowed.limits.max_nodes = 1;
        let token = issuer.issue_at(&allowed, 100).unwrap();
        let basis = db.frontier().unwrap();
        let old = identity_seed_arguments()
            .with_uint64("at", basis.0)
            .unwrap();
        let text = "UNWIND $nodes AS n MATCH (n) FOR SYSTEM_TIME AS OF SEQ $at RETURN n.p AS p";
        let prepared = PreparedNativeRead::prepare(text, &old, symbols).unwrap();
        let mut hide = WriteBatch::new(RelationId(1));
        hide.set_vertex_label(VId(1), LabelId(1), false);
        hide.set_vertex_property(VId(1), PropertyKeyId(1), Some(CanonicalScalar::Int(99)));
        hide.set_vertex_property(VId(4), PropertyKeyId(1), Some(CanonicalScalar::Int(10)));
        let after = db.write(&commit, hide).await.unwrap();
        let current = identity_seed_arguments()
            .with_uint64("at", after.0)
            .unwrap();
        let keyed_args = |at: CommitSeq| {
            use fgdb_gql::algebra::GraphValue;
            GqlParameters::new()
                .with_list(
                    "keys",
                    vec![GraphValue::Scalar(CanonicalScalar::Int(10)); 2],
                )
                .unwrap()
                .with_uint64("at", at.0)
                .unwrap()
        };
        let keyed_old = keyed_args(basis);
        let keyed_current = keyed_args(after);
        let keyed_text = "UNWIND $keys AS k MATCH (n {p:k}) FOR SYSTEM_TIME AS OF SEQ $at RETURN n";
        let keyed = PreparedNativeRead::prepare(keyed_text, &keyed_old, symbols).unwrap();
        let small = GqlQueryPolicy::new(1, 1000, 1_000_000, 1_000_000);
        for compact in [false, true] {
            if compact {
                db.compact(&commit).await.unwrap();
            }
            assert_eq!(
                prepared
                    .execute_authorized(&db, &cx, &issuer, &token, BRANCH, &old, small, || 100)
                    .unwrap(),
                native_rows(&["p"], vec![vec![value(10)], vec![value(10)]]),
            );
            assert_eq!(
                prepared
                    .execute_authorized(&db, &cx, &issuer, &token, BRANCH, &current, small, || 100)
                    .unwrap(),
                native_rows(&["p"], vec![]),
                "a hidden successor cannot revive the previously visible root",
            );
            // One shared historical posting set names both vertices. The
            // selected cut, followed by visibility, must choose 1 before the
            // rekey and 4 afterwards, still within one admitted record.
            for (args, id) in [(&keyed_old, VId(1)), (&keyed_current, VId(4))] {
                assert_eq!(
                    keyed
                        .execute_authorized(&db, &cx, &issuer, &token, BRANCH, args, small, || 100,)
                        .unwrap(),
                    native_rows(
                        &["n"],
                        vec![vec![QueryValue::Value(fgdb_gql::algebra::GraphValue::Vertex(id))]; 2],
                    ),
                    "historical property seeds, compact={compact}",
                );
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

fn property_seed_arguments() -> GqlParameters {
    use fgdb_gql::algebra::GraphValue;
    use fgdb_types::CanonicalF64;
    GqlParameters::new()
        .with_list(
            "keys",
            vec![
                GraphValue::Scalar(CanonicalScalar::Int(10)),
                GraphValue::Scalar(CanonicalScalar::Int(10)),
                GraphValue::Scalar(CanonicalScalar::Int(20)),
                GraphValue::Scalar(CanonicalScalar::Float(CanonicalF64::new(30.0))),
                GraphValue::Scalar(CanonicalScalar::Float(CanonicalF64::new(30.5))),
                GraphValue::Scalar(CanonicalScalar::Int(999)),
                GraphValue::Scalar(CanonicalScalar::Null),
                GraphValue::Scalar(CanonicalScalar::ucs_basic_text("10").unwrap()),
            ],
        )
        .unwrap()
}

#[test]
fn authorized_property_seeds_probe_visible_keys_before_expansion_and_aggregation() {
    let ((), report) = run_async_under_lab(0x5ec0_4025, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let full = identity_seed_database(&c.commit(), true).await;
        let filtered = identity_seed_database(&c.commit(), false).await;
        let issuer = authority(425);
        let mut allowed = grant();
        allowed.limits.max_nodes = 2;
        let token = issuer.issue_at(&allowed, 100).unwrap();
        let args = property_seed_arguments();
        let expected = native_rows(
            &["p"],
            vec![vec![value(10)], vec![value(10)], vec![value(30)]],
        );
        let small = GqlQueryPolicy::new(2, 1000, 1_000_000, 1_000_000);
        for text in [
            "UNWIND $keys AS k MATCH (n {p:k}) RETURN n.p AS p ORDER BY p",
            "UNWIND $keys AS k MATCH (n) WHERE n.p = k RETURN n.p AS p ORDER BY p",
            "UNWIND $keys AS k WITH k MATCH (n {p:k}) RETURN n.p AS p ORDER BY p",
        ] {
            assert_eq!(
                filtered.query(&cx, text, &args, symbols, policy()).unwrap(),
                expected,
                "{text}"
            );
            for db in [&full, &filtered] {
                assert_eq!(
                    db.query_authorized(
                        &cx,
                        &issuer,
                        &token,
                        BRANCH,
                        text,
                        &args,
                        symbols,
                        small,
                        || 100,
                    )
                    .unwrap(),
                    expected,
                    "{text}"
                );
            }
        }
        let text =
            "UNWIND $keys AS k MATCH (n {p:k}) RETURN count(*) AS n, count(n.hidden) AS hidden";
        for db in [&full, &filtered] {
            assert_eq!(
                db.query_authorized(
                    &cx,
                    &issuer,
                    &token,
                    BRANCH,
                    text,
                    &args,
                    symbols,
                    small,
                    || 100,
                )
                .unwrap(),
                native_rows(
                    &["n", "hidden"],
                    vec![vec![QueryValue::Count(3), QueryValue::Count(0)]],
                ),
            );
        }
        // The same root probe feeds original parallel edges and both visible
        // endpoints. The hidden transit vertex and denied relation stay out.
        let text = "UNWIND $keys AS k MATCH (n {p:k})-[]->(m) RETURN m.p AS p ORDER BY p";
        let expected = native_rows(
            &["p"],
            vec![
                vec![value(10)],
                vec![value(30)],
                vec![value(30)],
                vec![value(30)],
                vec![value(30)],
            ],
        );
        for db in [&full, &filtered] {
            assert_eq!(
                db.query_authorized(
                    &cx,
                    &issuer,
                    &token,
                    BRANCH,
                    text,
                    &args,
                    symbols,
                    GqlQueryPolicy::new(5, 1000, 1_000_000, 1_000_000),
                    || 100,
                )
                .unwrap(),
                expected,
            );
        }
        // The optimization retains the original limits. Two distinct roots
        // cannot fit one record, and an unkeyed range still reads the corpus.
        for text in [
            "UNWIND $keys AS k MATCH (n {p:k}) RETURN n.p AS p",
            "UNWIND $keys AS k MATCH (n) WHERE n.p > k RETURN n.p AS p",
        ] {
            assert!(
                full.query_authorized(
                    &cx,
                    &issuer,
                    &token,
                    BRANCH,
                    text,
                    &args,
                    symbols,
                    GqlQueryPolicy::new(1, 1000, 1_000_000, 1_000_000),
                    || 100,
                )
                .is_err()
            );
        }
        // A hidden field is not eligible to choose candidates. Its values
        // are NULL in both twins; with a small allowance both keep the same
        // full-source refusal, even though only one has matching postings.
        let text = "UNWIND $keys AS k MATCH (n {hidden:k}) RETURN n.p AS p";
        let token = issuer.issue_at(&grant(), 100).unwrap();
        for db in [&full, &filtered] {
            assert_eq!(
                db.query_authorized(
                    &cx,
                    &issuer,
                    &token,
                    BRANCH,
                    text,
                    &args,
                    symbols,
                    policy(),
                    || 100,
                )
                .unwrap(),
                native_rows(&["p"], vec![]),
            );
            assert!(
                db.query_authorized(
                    &cx,
                    &issuer,
                    &token,
                    BRANCH,
                    text,
                    &args,
                    symbols,
                    small,
                    || 100,
                )
                .is_err()
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn hidden_property_postings_and_stale_winners_do_not_change_admission_limits() {
    let ((), report) = run_async_under_lab(0x5ec0_4026, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let mut full = identity_seed_database(&c.commit(), true).await;
        let filtered = identity_seed_database(&c.commit(), false).await;
        let mut hidden = WriteBatch::new(RelationId(1));
        for id in 600..856 {
            hidden.create_vertex(
                VId(id),
                vec![LabelId(99)],
                vec![(PropertyKeyId(1), CanonicalScalar::Int(10))],
            );
        }
        full.write(&c.commit(), hidden).await.unwrap();
        // Vertex 4's stale value must not admit a third visible root. Its
        // latest value is 40 in both twins. Extra hidden-field revisions of
        // vertex 1 and matching hidden vertices may only add physical polls.
        for version in 0..8 {
            let mut change = WriteBatch::new(RelationId(1));
            change.set_vertex_property(
                VId(1),
                PropertyKeyId(2),
                Some(CanonicalScalar::Int(version)),
            );
            change.set_vertex_property(VId(4), PropertyKeyId(1), Some(CanonicalScalar::Int(10)));
            full.write(&c.commit(), change).await.unwrap();
        }
        let mut restore = WriteBatch::new(RelationId(1));
        restore.set_vertex_property(VId(4), PropertyKeyId(1), Some(CanonicalScalar::Int(40)));
        full.write(&c.commit(), restore).await.unwrap();
        let issuer = authority(426);
        let args = property_seed_arguments();
        let text = "UNWIND $keys AS k MATCH (n {p:k}) RETURN n.p AS p ORDER BY p";
        let expected = native_rows(
            &["p"],
            vec![vec![value(10)], vec![value(10)], vec![value(30)]],
        );
        // Find each observable threshold independently of implementation
        // event counts: signed work, native work, then native scratch.
        for dimension in 0..3 {
            let least = |db: &Database<MemVfs>| {
                let (mut low, mut high) = (1, 1_000_000);
                while low < high {
                    let middle = low + (high - low) / 2;
                    let mut allowed = grant();
                    allowed.limits.max_nodes = 2;
                    let mut bounded = GqlQueryPolicy::new(2, 1000, 1_000_000, 1_000_000);
                    match dimension {
                        0 => allowed.limits.max_work = middle,
                        1 => bounded.evaluator.max_work_units = middle,
                        _ => bounded.evaluator.max_scratch_entries = middle,
                    }
                    let token = issuer.issue_at(&allowed, 100).unwrap();
                    match db.query_authorized(
                        &cx,
                        &issuer,
                        &token,
                        BRANCH,
                        text,
                        &args,
                        symbols,
                        bounded,
                        || 100,
                    ) {
                        Ok(rows) => {
                            assert_eq!(rows, expected);
                            high = middle;
                        }
                        Err(_) => low = middle + 1,
                    }
                }
                low
            };
            let full_limit = least(&full);
            assert!(full_limit > 1 && full_limit < 1_000_000);
            assert_eq!(full_limit, least(&filtered), "dimension {dimension}");
        }
        // Every observed live authority boundary can refuse the private
        // result; a later execution still sees exactly the same complete bag.
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let bounded = GqlQueryPolicy::new(2, 1000, 1_000_000, 1_000_000);
        let mut calls = 0;
        assert_eq!(
            full.query_authorized(
                &cx,
                &issuer,
                &token,
                BRANCH,
                text,
                &args,
                symbols,
                bounded,
                || {
                    calls += 1;
                    100
                },
            )
            .unwrap(),
            expected,
        );
        for stop in 1..=calls {
            let mut seen = 0;
            assert!(matches!(
                full.query_authorized(
                    &cx,
                    &issuer,
                    &token,
                    BRANCH,
                    text,
                    &args,
                    symbols,
                    bounded,
                    || {
                        seen += 1;
                        if seen == stop { 1000 } else { 100 }
                    },
                ),
                Err(QueryError::Authorization(Error::Expired))
            ));
        }
        assert_eq!(
            full.query_authorized(
                &cx,
                &issuer,
                &token,
                BRANCH,
                text,
                &args,
                symbols,
                bounded,
                || 100,
            )
            .unwrap(),
            expected,
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn all_seven_native_facades_use_scoped_data_and_lossless_output_slots() {
    let ((), report) = run_async_under_lab(0x5ec0_4001, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let db = database(&c.commit()).await;
        let issuer = authority(401);
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let params = GqlParameters::new();
        let summary = native_rows(
            &["rows", "total"],
            vec![vec![QueryValue::Count(2), QueryValue::Integer(40)]],
        );
        let union = native_rows(
            &["p"],
            vec![
                vec![value(10)],
                vec![value(10)],
                vec![value(30)],
                vec![value(30)],
            ],
        );
        for (class, text, expected) in [
            (
                NativeReadClass::Pattern,
                "MATCH (n) RETURN n.p AS p",
                p_rows(),
            ),
            (
                NativeReadClass::Aggregate,
                "MATCH (n) RETURN count(*) AS rows, sum(n.p) AS total",
                summary.clone(),
            ),
            (
                NativeReadClass::PipelineAggregate,
                "MATCH (n) WITH n.p AS p RETURN count(*) AS rows, sum(p) AS total",
                summary.clone(),
            ),
            (
                NativeReadClass::Set,
                "MATCH (n) RETURN n.p AS p UNION ALL MATCH (n) RETURN n.p AS p",
                union.clone(),
            ),
            (
                NativeReadClass::TemporalPattern,
                "MATCH (n) FOR SYSTEM_TIME AS OF SEQ 2 RETURN n.p AS p",
                p_rows(),
            ),
            (
                NativeReadClass::TemporalAggregate,
                "MATCH (n) FOR SYSTEM_TIME AS OF SEQ 2 RETURN count(*) AS rows, sum(n.p) AS total",
                summary,
            ),
            (
                NativeReadClass::TemporalSet,
                "MATCH (n) FOR SYSTEM_TIME AS OF SEQ 2 RETURN n.p AS p UNION ALL MATCH (n) RETURN n.p AS p",
                union,
            ),
        ] {
            let prepared = PreparedNativeRead::prepare(text, &params, symbols).unwrap();
            assert_eq!(prepared.facade_class(), class, "{text}");
            assert_eq!(
                db.query_authorized(
                    &cx,
                    &issuer,
                    &token,
                    BRANCH,
                    text,
                    &params,
                    symbols,
                    policy(),
                    || 100
                )
                .unwrap(),
                expected
            );
            assert_eq!(
                prepared
                    .execute_authorized(
                        &db,
                        &cx,
                        &issuer,
                        &token,
                        BRANCH,
                        &params,
                        policy(),
                        || 100
                    )
                    .unwrap(),
                expected
            );
            // The trusted unscoped control sees the hidden vertex too. Agreement
            // between two incorrectly privileged adapters cannot pass this law.
            assert_ne!(
                prepared.execute(&db, &cx, &params, policy()).unwrap(),
                expected
            );
        }
        let text = "MATCH (n) RETURN sum(n.p) AS total, count(*) AS rows";
        assert_eq!(
            db.query_authorized(
                &cx,
                &issuer,
                &token,
                BRANCH,
                text,
                &params,
                symbols,
                policy(),
                || 100
            )
            .unwrap(),
            native_rows(
                &["total", "rows"],
                vec![vec![QueryValue::Integer(40), QueryValue::Count(2)]]
            )
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn prepared_native_rebinding_never_caches_the_previous_scope_or_argument_values() {
    let ((), report) = run_async_under_lab(0x5ec0_4002, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let db = database(&c.commit()).await;
        let issuer = authority(402);
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let first = GqlParameters::new().with_int64("minimum", 5).unwrap();
        let second = GqlParameters::new().with_int64("minimum", 20).unwrap();
        let text = "MATCH (n) WHERE n.p >= $minimum RETURN n.p AS p";
        let prepared = PreparedNativeRead::prepare(text, &first, symbols).unwrap();
        let frozen = prepared.statement().to_owned();
        assert_eq!(
            prepared
                .execute_authorized(&db, &cx, &issuer, &token, BRANCH, &first, policy(), || 100)
                .unwrap(),
            p_rows()
        );
        assert_eq!(
            prepared
                .execute_authorized(&db, &cx, &issuer, &token, BRANCH, &second, policy(), || 100)
                .unwrap(),
            native_rows(&["p"], vec![vec![value(30)]])
        );
        let masked = token
            .attenuate(Restriction::DenyProperties(
                [PropertyKeyId(1)].into_iter().collect(),
            ))
            .unwrap();
        assert_eq!(
            prepared
                .execute_authorized(&db, &cx, &issuer, &masked, BRANCH, &first, policy(), || 100)
                .unwrap(),
            native_rows(&["p"], vec![])
        );
        let no_vertices = token
            .attenuate(Restriction::Labels(Scope::only([])))
            .unwrap();
        assert_eq!(
            prepared
                .execute_authorized(
                    &db,
                    &cx,
                    &issuer,
                    &no_vertices,
                    BRANCH,
                    &first,
                    policy(),
                    || 100
                )
                .unwrap(),
            native_rows(&["p"], vec![])
        );
        for invalid in [
            GqlParameters::new(),
            GqlParameters::new().with_text("minimum", "20").unwrap(),
            first.clone().with_int64("extra", 0).unwrap(),
        ] {
            assert!(matches!(
                prepared.execute_authorized(
                    &db,
                    &cx,
                    &issuer,
                    &token,
                    BRANCH,
                    &invalid,
                    policy(),
                    || 100
                ),
                Err(QueryError::PatternText(_))
            ));
        }
        // Credential admission wins even over a missing required argument.
        assert!(matches!(
            prepared.execute_authorized(
                &db,
                &cx,
                &issuer,
                &token,
                BRANCH,
                &GqlParameters::new(),
                policy(),
                || 1000
            ),
            Err(QueryError::Authorization(Error::Expired))
        ));
        assert_eq!(prepared.statement(), frozen);
        assert_eq!(
            prepared
                .execute_authorized(&db, &cx, &issuer, &token, BRANCH, &first, policy(), || 100)
                .unwrap(),
            p_rows()
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn authentication_precedes_text_catalog_and_future_cut_admission() {
    let ((), report) = run_async_under_lab(0x5ec0_4003, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let db = database(&c.commit()).await;
        let issuer = authority(403);
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let wrong_key = authority(404);
        let wrong_namespace = Authority::new(
            AuthKey::from_seed(403),
            DatabaseSecurityNamespaceId([0; 32]),
            "host-graph",
            SchemaEpoch(0),
            1,
        )
        .unwrap();
        let mut no_read = grant();
        no_read.rights = Rights::Write;
        let no_read = issuer.issue_at(&no_read, 100).unwrap();
        for (authority, token, now, error) in [
            (&wrong_namespace, &token, 100, Error::WrongAuthority),
            (&wrong_key, &token, 100, Error::Unauthenticated),
            (&issuer, &token, 1000, Error::Expired),
            (&issuer, &no_read, 100, Error::PermissionDenied),
        ] {
            for text in [
                "MATCH (n:L) RETURN n",
                "not a query (",
                "MATCH (n:L) FOR SYSTEM_TIME AS OF SEQ 999999 RETURN n",
            ] {
                assert!(
                    matches!(db.query_authorized(&cx, authority, token, BRANCH, text,
                    &GqlParameters::new(), |_, _: &str| -> Option<GraphSymbol> { panic!("unauthenticated catalog access") },
                    // ubs:ignore -- test assertion on an authorization error value, not secret material.
                    policy(), || now), Err(QueryError::Authorization(actual)) if actual == error)
                );
            }
        }
        assert!(matches!(
            db.query_authorized(
                &cx,
                &issuer,
                &token,
                "different-host-branch",
                "MATCH (n:L) RETURN n",
                &GqlParameters::new(),
                |_, _: &str| -> Option<GraphSymbol> { panic!("wrong-branch catalog access") },
                policy(),
                || 100
            ),
            Err(QueryError::Authorization(Error::ScopeDenied))
        ));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn textual_branch_selectors_can_only_confirm_the_already_authorized_host_branch() {
    let ((), report) = run_async_under_lab(0x5ec0_4004, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let db = database(&c.commit()).await;
        let issuer = authority(405);
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let parameters = GqlParameters::new().with_text("branch", BRANCH).unwrap();
        for (text, args) in [
            (
                "AT BRANCH 'host-branch' MATCH (n) RETURN n.p AS p",
                GqlParameters::new(),
            ),
            (
                "MATCH (n) RETURN n.p AS p AT BRANCH 'host-branch'",
                GqlParameters::new(),
            ),
            (
                "AT BRANCH $branch MATCH (n) RETURN n.p AS p",
                parameters.clone(),
            ),
        ] {
            assert_eq!(
                db.query_authorized(
                    &cx,
                    &issuer,
                    &token,
                    BRANCH,
                    text,
                    &args,
                    symbols,
                    policy(),
                    || 100
                )
                .unwrap(),
                p_rows()
            );
        }
        for selected in ["other", "HOST-BRANCH", "host-branch' MATCH (n) RETURN n"] {
            let args = GqlParameters::new().with_text("branch", selected).unwrap();
            assert!(matches!(
                db.query_authorized(
                    &cx,
                    &issuer,
                    &token,
                    BRANCH,
                    "AT BRANCH $branch MATCH (n:L) RETURN n.p AS p",
                    &args,
                    |_, _: &str| -> Option<GraphSymbol> { panic!("selector switched catalog") },
                    policy(),
                    || 100
                ),
                Err(QueryError::Authorization(Error::ScopeDenied))
            ));
        }
        // The selector consumes ONLY its own exclusive argument; unrelated
        // arguments remain visible to the ordinary native schema validator.
        let extra = parameters.clone().with_int64("unused", 1).unwrap();
        assert!(
            db.query_authorized(
                &cx,
                &issuer,
                &token,
                BRANCH,
                "AT BRANCH $branch MATCH (n) RETURN n.p AS p",
                &extra,
                symbols,
                policy(),
                || 100
            )
            .is_err()
        );
        // A selector argument also used in the row expression remains bound.
        let result = db
            .query_authorized(
                &cx,
                &issuer,
                &token,
                BRANCH,
                "AT BRANCH $branch RETURN $branch AS name",
                &parameters,
                symbols,
                policy(),
                || 100,
            )
            .unwrap();
        assert_eq!(
            result,
            native_rows(
                &["name"],
                vec![vec![QueryValue::Value(
                    fgdb_gql::algebra::GraphValue::Scalar(
                        CanonicalScalar::ucs_basic_text(BRANCH).unwrap()
                    ),
                )]]
            )
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn temporal_native_rebinding_uses_one_exact_cut_but_current_credential_validity() {
    let ((), report) = run_async_under_lab(0x5ec0_4005, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = database(&commit).await;
        let issuer = authority(406);
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let basis = db.frontier().unwrap();
        let args = GqlParameters::new().with_uint64("at", basis.0).unwrap();
        let text = "MATCH (n) FOR SYSTEM_TIME AS OF SEQ $at RETURN n.p AS p UNION ALL MATCH (n) RETURN n.p AS p";
        let prepared = PreparedNativeRead::prepare(text, &args, symbols).unwrap();
        assert_eq!(prepared.facade_class(), NativeReadClass::TemporalSet);
        let expected = native_rows(
            &["p"],
            vec![
                vec![value(10)],
                vec![value(10)],
                vec![value(30)],
                vec![value(30)],
            ],
        );
        let mut change = WriteBatch::new(RelationId(1));
        change.set_vertex_label(VId(1), LabelId(1), false);
        let after = db.write(&commit, change).await.unwrap();
        let current = GqlParameters::new().with_uint64("at", after.0).unwrap();
        for compact in [false, true] {
            if compact {
                db.compact(&commit).await.unwrap();
            }
            assert_eq!(
                prepared
                    .execute_authorized(&db, &cx, &issuer, &token, BRANCH, &args, policy(), || 100)
                    .unwrap(),
                expected
            );
            assert_eq!(
                prepared
                    .execute_authorized(
                        &db,
                        &cx,
                        &issuer,
                        &token,
                        BRANCH,
                        &current,
                        policy(),
                        || 100
                    )
                    .unwrap(),
                native_rows(&["p"], vec![vec![value(30)], vec![value(30)]])
            );
            assert_eq!(
                db.query_authorized(
                    &cx,
                    &issuer,
                    &token,
                    BRANCH,
                    text,
                    &args,
                    symbols,
                    policy(),
                    || 100
                )
                .unwrap(),
                expected
            );
        }
        let future = GqlParameters::new().with_uint64("at", after.0 + 1).unwrap();
        assert!(
            matches!(prepared.execute_authorized(&db, &cx, &issuer, &token, BRANCH, &future, policy(), || 100),
            Err(QueryError::Read(ReadError::BeyondFrontier { asked, frontier })) if asked.0 == after.0 + 1 && frontier == after)
        );
        assert!(matches!(
            prepared
                .execute_authorized(&db, &cx, &issuer, &token, BRANCH, &args, policy(), || 1000),
            Err(QueryError::Authorization(Error::Expired))
        ));
        let zero = GqlParameters::new().with_uint64("at", 0).unwrap();
        assert_eq!(
            db.query_authorized(
                &cx,
                &issuer,
                &token,
                BRANCH,
                "MATCH (n) FOR SYSTEM_TIME AS OF SEQ $at RETURN count(*) AS rows",
                &zero,
                symbols,
                policy(),
                || 100
            )
            .unwrap(),
            native_rows(&["rows"], vec![vec![QueryValue::Count(0)]])
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn compound_native_permits_do_not_reset_and_private_rows_do_not_consume_delivery() {
    let ((), report) = run_async_under_lab(0x5ec0_4006, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let db = database(&c.commit()).await;
        let issuer = authority(407);
        let mut limited = grant();
        limited.limits.max_nodes = 2;
        let token = issuer.issue_at(&limited, 100).unwrap();
        let params = GqlParameters::new();
        let union = "MATCH (n) RETURN n.p AS p UNION ALL MATCH (n) RETURN n.p AS p";
        assert_eq!(
            db.query_authorized(
                &cx,
                &issuer,
                &token,
                BRANCH,
                "MATCH (n) RETURN n.p AS p",
                &params,
                symbols,
                policy(),
                || 100
            )
            .unwrap(),
            p_rows()
        );
        assert!(matches!(
            db.query_authorized(
                &cx,
                &issuer,
                &token,
                BRANCH,
                union,
                &params,
                symbols,
                policy(),
                || 100
            ),
            Err(QueryError::Authorization(Error::LimitExceeded(
                LimitDimension::Nodes
            )))
        ));
        limited.limits.max_nodes = 4;
        limited.limits.max_rows = 4;
        let token = issuer.issue_at(&limited, 100).unwrap();
        assert_eq!(
            db.query_authorized(
                &cx,
                &issuer,
                &token,
                BRANCH,
                union,
                &params,
                symbols,
                policy(),
                || 100
            )
            .unwrap(),
            native_rows(
                &["p"],
                vec![
                    vec![value(10)],
                    vec![value(10)],
                    vec![value(30)],
                    vec![value(30)]
                ]
            )
        );
        limited.limits.max_rows = 3;
        let token = issuer.issue_at(&limited, 100).unwrap();
        assert!(matches!(
            db.query_authorized(
                &cx,
                &issuer,
                &token,
                BRANCH,
                union,
                &params,
                symbols,
                policy(),
                || 100
            ),
            Err(QueryError::Authorization(Error::LimitExceeded(
                LimitDimension::Rows
            )))
        ));
        limited.limits.max_rows = 1;
        limited.limits.max_nodes = 2;
        let token = issuer.issue_at(&limited, 100).unwrap();
        assert_eq!(
            db.query_authorized(
                &cx,
                &issuer,
                &token,
                BRANCH,
                "MATCH (n) WITH n.p AS p RETURN count(*) AS rows, sum(p) AS total",
                &params,
                symbols,
                policy(),
                || 100
            )
            .unwrap(),
            native_rows(
                &["rows", "total"],
                vec![vec![QueryValue::Count(2), QueryValue::Integer(40)]]
            )
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn source_free_and_empty_pages_authenticate_and_unsupported_text_never_falls_back() {
    let ((), report) = run_async_under_lab(0x5ec0_4007, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let db = database(&c.commit()).await;
        let issuer = authority(408);
        let mut limited = grant();
        limited.limits.max_nodes = 0;
        limited.limits.max_rows = 1;
        let token = issuer.issue_at(&limited, 100).unwrap();
        let params = GqlParameters::new();
        assert_eq!(
            db.query_authorized(
                &cx,
                &issuer,
                &token,
                BRANCH,
                "RETURN 1 AS one",
                &params,
                symbols,
                policy(),
                || 100
            )
            .unwrap(),
            native_rows(&["one"], vec![vec![value(1)]])
        );
        let text = "UNWIND [1, 2, 2] AS x RETURN count(*) AS rows, sum(x) AS total";
        let prepared = PreparedNativeRead::prepare(text, &params, symbols).unwrap();
        assert!(
            matches!(&prepared, PreparedNativeRead::PipelineAggregate(query) if query.is_source_free())
        );
        assert_eq!(
            db.query_authorized(
                &cx,
                &issuer,
                &token,
                BRANCH,
                text,
                &params,
                symbols,
                policy(),
                || 100
            )
            .unwrap(),
            native_rows(
                &["rows", "total"],
                vec![vec![QueryValue::Count(3), QueryValue::Integer(5)]]
            )
        );
        limited.limits.max_rows = 0;
        let token = issuer.issue_at(&limited, 100).unwrap();
        assert_eq!(
            db.query_authorized(
                &cx,
                &issuer,
                &token,
                BRANCH,
                "RETURN 1 AS one LIMIT 0",
                &params,
                symbols,
                policy(),
                || 100
            )
            .unwrap(),
            native_rows(&["one"], vec![])
        );
        assert!(matches!(
            db.query_authorized(
                &cx,
                &issuer,
                &token,
                BRANCH,
                "RETURN 1 AS one LIMIT 0",
                &params,
                symbols,
                policy(),
                || 1000
            ),
            Err(QueryError::Authorization(Error::Expired))
        ));
        // This must be an EXECUTION failure, not a parser refusal or a silently
        // skipped expression merely because the final page contains no rows.
        let invalid = "UNWIND [1, 0] AS x RETURN 10 / x AS quotient LIMIT 0";
        let prepared = PreparedNativeRead::prepare(invalid, &params, symbols).unwrap();
        assert!(matches!(
            prepared.execute_authorized(
                &db,
                &cx,
                &issuer,
                &token,
                BRANCH,
                &params,
                policy(),
                || 100
            ),
            Err(QueryError::Set(GqlQueryError::Source(_)))
                | Err(QueryError::Aggregate(GqlQueryError::Source(_)))
        ));
        let before = db.frontier().unwrap();
        // EXPLAIN lists the text-derived plan (fgdb-ooiik) and writes nothing.
        // Its listing is delivered rows, so this token's zero row allowance
        // refuses it while a token without that limit receives it.
        let explain = |token: &CapabilityToken| {
            db.query_authorized(
                &cx,
                &issuer,
                token,
                BRANCH,
                "EXPLAIN MATCH (n) RETURN n",
                &params,
                symbols,
                policy(),
                || 100,
            )
        };
        assert!(matches!(
            explain(&token),
            Err(QueryError::Authorization(Error::LimitExceeded(
                LimitDimension::Rows
            )))
        ));
        let listed = explain(&issuer.issue_at(&grant(), 100).unwrap()).unwrap();
        assert!(matches!(&listed, QueryResult::Rows { columns, rows }
            if *columns == ["operator", "detail"] && !rows.is_empty()));
        assert_eq!(db.frontier().unwrap(), before);
        for refused in [
            "EXPLAIN (CERTIFICATE) MATCH (n) RETURN n",
            "EXPLAIN INSERT (n:L)",
            "INSERT (n:L)",
            "CALL fnx.pagerank()",
            "MATCH (",
        ] {
            assert!(
                db.query_authorized(
                    &cx,
                    &issuer,
                    &token,
                    BRANCH,
                    refused,
                    &params,
                    symbols,
                    policy(),
                    || 100
                )
                .is_err()
            );
            assert_eq!(db.frontier().unwrap(), before);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn native_preparation_and_final_delivery_recheck_expiry_and_resolver_retirement() {
    let ((), report) = run_async_under_lab(0x5ec0_4008, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let db = database(&c.commit()).await;
        let issuer = authority(409);
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let params = GqlParameters::new();
        let text = "MATCH (n:L) WITH n.p AS p RETURN count(*) AS rows, sum(p) AS total";
        let mut calls = 0;
        let expected = db
            .query_authorized(
                &cx,
                &issuer,
                &token,
                BRANCH,
                text,
                &params,
                symbols,
                policy(),
                || {
                    calls += 1;
                    100
                },
            )
            .unwrap();
        for stop in 1..=calls {
            let mut seen = 0;
            assert!(matches!(
                db.query_authorized(
                    &cx,
                    &issuer,
                    &token,
                    BRANCH,
                    text,
                    &params,
                    symbols,
                    policy(),
                    || {
                        seen += 1;
                        if seen == stop { 1000 } else { 100 }
                    }
                ),
                Err(QueryError::Authorization(Error::Expired))
            ));
            assert_eq!(seen, stop);
        }
        assert_eq!(
            db.query_authorized(
                &cx,
                &issuer,
                &token,
                BRANCH,
                text,
                &params,
                symbols,
                policy(),
                || 100
            )
            .unwrap(),
            expected
        );
        let mut seen = 0;
        assert!(matches!(
            db.query_authorized(
                &cx,
                &issuer,
                &token,
                BRANCH,
                text,
                &params,
                symbols,
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
        for resolve in [true, false] {
            let issuer = authority(410);
            let token = issuer.issue_at(&grant(), 100).unwrap();
            let mut entered = false;
            assert!(matches!(
                db.query_authorized(
                    &cx,
                    &issuer,
                    &token,
                    BRANCH,
                    "MATCH (n:L) RETURN n",
                    &params,
                    |kind, name: &str| {
                        entered = true;
                        issuer.retire();
                        if resolve { symbols(kind, name) } else { None }
                    },
                    policy(),
                    || 100
                ),
                Err(QueryError::Authorization(Error::AuthorityRetired))
            ));
            assert!(
                entered,
                "the retirement must occur inside real native catalog resolution"
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

/// fgdb-xnd7x under a capability: `x.key` on a path's elements reads through
/// the authorized host. Every element there was admitted under the token, and
/// a property outside its scope reads exactly as an absent one (FG-INV-20),
/// as the pattern tables mask it. The trusted unscoped control reads the
/// hidden values, so a privileged reader cannot pass this law.
#[test]
fn element_property_reads_apply_the_capability_property_scope() {
    use fgdb_gql::algebra::GraphValue;
    let ((), report) = run_async_under_lab(0x5ec0_4011, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let db = database(&c.commit()).await;
        let issuer = authority(411);
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let params = GqlParameters::new();
        let list = |values: &[Option<i64>]| {
            QueryValue::Value(GraphValue::List(
                values
                    .iter()
                    .map(|value| {
                        GraphValue::Scalar(
                            value.map_or(CanonicalScalar::Null, CanonicalScalar::Int),
                        )
                    })
                    .collect(),
            ))
        };
        let text = "MATCH p = (a)-[:R]->(b) \
            RETURN [n IN nodes(p) | n.p] AS ps, [n IN nodes(p) | n.hidden] AS hs, \
            [e IN relationships(p) | e.hidden] AS ehs";
        // Visible R edges join only vertices 1 and 3: two 1->3, one 3->1.
        let hidden = list(&[None, None]);
        let expected = native_rows(
            &["ps", "hs", "ehs"],
            vec![
                vec![list(&[Some(10), Some(30)]), hidden.clone(), list(&[None])],
                vec![list(&[Some(10), Some(30)]), hidden.clone(), list(&[None])],
                vec![list(&[Some(30), Some(10)]), hidden, list(&[None])],
            ],
        );
        assert_eq!(
            db.query_authorized(
                &cx,
                &issuer,
                &token,
                BRANCH,
                text,
                &params,
                symbols,
                policy(),
                || 100
            )
            .unwrap(),
            expected
        );
        let prepared = PreparedNativeRead::prepare(text, &params, symbols).unwrap();
        let QueryResult::Rows { rows, .. } = prepared.execute(&db, &cx, &params, policy()).unwrap()
        else {
            panic!("a row query")
        };
        assert!(
            rows.iter()
                .any(|row| row[1] == list(&[Some(100), Some(300)])),
            "the unscoped control reads the hidden property"
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

/// A capability that hides nothing serves a seekable root from the property
/// index, as the owner source does, so a point or fixed-hop read admits its
/// root's closure instead of the whole graph (fgdbd answered every statement
/// over a 500k-edge graph with a SnapshotRecords refusal). The answers equal
/// the owner's under a snapshot budget far below the graph's size, an unbound
/// scan under that budget still refuses, and a scoped capability keeps masking
/// a bound predicate on a hidden property.
#[test]
fn unrestricted_capabilities_admit_only_an_index_bound_closure() {
    let ((), report) = run_async_under_lab(0x5ec0_4012, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, DatabaseKeys::new([0x33; 32], NS, [0x68; 32]))
            .await
            .unwrap();
        // A 300-vertex chain 0 -> 1 -> ... -> 299: 599 records in all.
        let mut chain = WriteBatch::new(RelationId(1));
        for at in 0..300_i64 {
            chain.create_vertex(
                VId(1000 + at as u128),
                vec![LabelId(1)],
                vec![
                    (PropertyKeyId(1), CanonicalScalar::Int(at)),
                    (PropertyKeyId(2), CanonicalScalar::Int(at * 10)),
                ],
            );
        }
        for at in 0..299_i64 {
            chain.add_edge(
                EId(5000 + at as u128),
                VId(1000 + at as u128),
                VId(1001 + at as u128),
                vec![],
            );
        }
        db.write(&commit, chain).await.unwrap();
        let issuer = authority(412);
        let limits = QueryLimits {
            max_nodes: 1_000_000,
            max_work: 100_000_000,
            max_rows: 1000,
        };
        let mut open = Grant::read_only(BRANCH, 1000, limits);
        open.labels = Scope::All;
        open.relations = Scope::All;
        open.properties = Scope::All;
        let open = issuer.issue_at(&open, 100).unwrap();
        let mut scoped = Grant::read_only(BRANCH, 1000, limits);
        scoped.labels = Scope::All;
        scoped.relations = Scope::All;
        scoped.properties = Scope::only([PropertyKeyId(1)]);
        let scoped = issuer.issue_at(&scoped, 100).unwrap();
        let params = GqlParameters::new();
        let small = GqlQueryPolicy::new(32, 1000, 1_000_000, 1_000_000);
        let large = GqlQueryPolicy::new(10_000, 1000, 10_000_000, 10_000_000);
        let read = |token: &CapabilityToken, text: &str, policy: GqlQueryPolicy| {
            db.query_authorized(
                &cx,
                &issuer,
                token,
                BRANCH,
                text,
                &params,
                symbols,
                policy,
                || 100,
            )
        };
        for (text, expected) in [
            ("MATCH (n:L {p: 7}) RETURN n.p AS p", vec![vec![value(7)]]),
            (
                "MATCH (n:L) WHERE n.p = 7 RETURN n.p AS p",
                vec![vec![value(7)]],
            ),
            (
                "MATCH (a:L {p: 7})-[:R]->(b) RETURN b.p AS p",
                vec![vec![value(8)]],
            ),
            (
                "MATCH (a:L {p: 7})<-[:R]-(b) RETURN b.p AS p",
                vec![vec![value(6)]],
            ),
            (
                "MATCH (a:L {p: 7})-[:R]->()-[:R]->(c) RETURN c.p AS p",
                vec![vec![value(9)]],
            ),
            (
                "MATCH (a:L {p: 7})-[:R]->(b) RETURN count(b) AS n",
                vec![vec![QueryValue::Count(1)]],
            ),
            // Quantified reaches from the bound root admit only their walk.
            (
                "MATCH (a:L {p: 7})-[:R]->{1,3}(b) RETURN count(DISTINCT b) AS n",
                vec![vec![QueryValue::Count(3)]],
            ),
            (
                "MATCH (a:L {p: 7})<-[:R]-{0,2}(b) RETURN count(b) AS n",
                vec![vec![QueryValue::Count(3)]],
            ),
        ] {
            let prepared = PreparedNativeRead::prepare(text, &params, symbols).unwrap();
            let QueryResult::Rows { columns, rows } =
                prepared.execute(&db, &cx, &params, large).unwrap()
            else {
                panic!("a row query: {text}")
            };
            assert_eq!(rows, expected, "owner answer for {text}");
            let owner = QueryResult::Rows { columns, rows };
            assert_eq!(read(&open, text, small).unwrap(), owner, "{text}");
            assert_eq!(read(&open, text, large).unwrap(), owner, "{text}");
        }
        // The small budget is real: an unbound scan under it still refuses.
        let unbound = read(&open, "MATCH (n:L) RETURN n.p AS p", small);
        assert!(
            matches!(
                &unbound,
                Err(QueryError::Pattern(GqlQueryError::Rows(error)))
                    if error.dimension == fgdb_gql::GqlBudgetDimension::SnapshotRecords
            ),
            "{unbound:?}"
        );
        let unbound = read(&open, "MATCH (n:L) RETURN count(*) AS n", small);
        assert!(unbound.is_err(), "{unbound:?}");
        // A scoped capability cannot match through the property it cannot see,
        // even where the owner's index holds the value; visible keys still bind.
        let hidden = "MATCH (n:L {hidden: 70}) RETURN n.p AS p";
        let QueryResult::Rows { rows, .. } = PreparedNativeRead::prepare(hidden, &params, symbols)
            .unwrap()
            .execute(&db, &cx, &params, large)
            .unwrap()
        else {
            panic!("a row query")
        };
        assert_eq!(
            rows,
            vec![vec![value(7)]],
            "the owner sees the hidden value"
        );
        assert_eq!(
            read(&scoped, hidden, large).unwrap(),
            native_rows(&["p"], vec![])
        );
        assert_eq!(
            read(
                &scoped,
                "MATCH (a:L {p: 7})-[:R]->(b) RETURN b.p AS p",
                large
            )
            .unwrap(),
            native_rows(&["p"], vec![vec![value(8)]])
        );
        // Every scoped capability, whatever its scope hides, keeps the complete
        // admission: its charge is the visible graph's, never a function of
        // which hidden values the index selects. A label or relation scope
        // that hides nothing in this graph still decides by the capability
        // alone. Under the small budget each refuses where the bound path
        // would answer.
        let mut by_label = Grant::read_only(BRANCH, 1000, limits);
        by_label.labels = Scope::only([LabelId(1)]);
        by_label.relations = Scope::All;
        by_label.properties = Scope::All;
        let by_label = issuer.issue_at(&by_label, 100).unwrap();
        let mut by_relation = Grant::read_only(BRANCH, 1000, limits);
        by_relation.labels = Scope::All;
        by_relation.relations = Scope::only([RelationId(1)]);
        by_relation.properties = Scope::All;
        let by_relation = issuer.issue_at(&by_relation, 100).unwrap();
        for (token, name) in [
            (&scoped, "property-scoped"),
            (&by_label, "label-scoped"),
            (&by_relation, "relation-scoped"),
        ] {
            for text in [hidden, "MATCH (a:L {p: 7})-[:R]->(b) RETURN b.p AS p"] {
                let refused = read(token, text, small);
                assert!(
                    matches!(
                        &refused,
                        Err(QueryError::Pattern(GqlQueryError::Rows(error)))
                            if error.dimension == fgdb_gql::GqlBudgetDimension::SnapshotRecords
                    ),
                    "{name} {text}: {refused:?}"
                );
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
