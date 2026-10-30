//! Grouped WITH is a maintained row stage, not a per-commit query rerun.
use asupersync::lab::run_async_under_lab;
use fgdb::{
    Database, DatabaseKeys, MemVfs, PreparedNativeRead, QueryResult, QueryValue,
    StandingQueryError, StandingQueryFailure, StandingQueryHandle, WriteBatch,
};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::{
    GraphColumn, GraphPatternBuilder, GraphValue, GraphValueRow, PreparedGraphPattern,
};
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphAggregate, GraphIntegerErrorKind, GraphSymbol,
    GraphSymbolKind, PreparedGraphSet,
};
use fgdb_types::{
    CanonicalF64, CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, EId, PurposeContexts,
    QueryCx, VId,
};

const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);
fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x41; 32],
        DatabaseSecurityNamespaceId([0x42; 32]),
        [0x43; 32],
    )
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(10_000, 10_000, 50_000_000, 50_000_000)
}
fn delivery() -> GqlQueryPolicy {
    // Maintained delivery cannot consume or reopen any source records.
    GqlQueryPolicy::new(0, 10_000, 50_000_000, 50_000_000)
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "L") => Some(GraphSymbol::Label(LabelId(1))),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(RelationId(1))),
        (GraphSymbolKind::Relation, "S") => Some(GraphSymbol::Relation(RelationId(2))),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
        _ => None,
    }
}
async fn seed(cx: &CommitCx) -> Database<MemVfs> {
    let mut db = Database::open_memory(cx, keys()).await.unwrap();
    let mut batch = WriteBatch::new(RelationId(1));
    for (id, bucket, amount) in [
        (1, 1, Some(3)),
        (2, 1, Some(4)),
        (3, 2, Some(5)),
        (4, 2, Some(5)),
        (5, 3, Some(9)),
        (6, 4, None),
    ] {
        let mut properties = vec![(P, CanonicalScalar::Int(bucket))];
        if let Some(amount) = amount {
            properties.push((Q, CanonicalScalar::Int(amount)));
        }
        batch.create_vertex(VId(id), vec![LabelId(1)], properties.clone());
        if id != 1 {
            batch.add_edge(EId(100 + id), VId(1), VId(id), properties);
        }
    }
    db.write(cx, batch).await.unwrap();
    db
}
fn assert_native(
    db: &Database<MemVfs>,
    cx: &QueryCx,
    handle: &StandingQueryHandle,
    text: &str,
    parameters: &GqlParameters,
) {
    let expected = db.query(cx, text, parameters, symbols, policy()).unwrap();
    let (frontier, actual) = db.standing_native_query(cx, handle, delivery()).unwrap();
    assert_eq!(frontier, db.frontier().unwrap());
    assert_eq!(actual, expected, "{text}");
    let QueryResult::Rows { columns, rows } = actual else {
        panic!("query rows");
    };
    let mut cursor = db.standing_native_cursor(cx, handle, delivery()).unwrap();
    assert_eq!(cursor.columns(), columns);
    assert_eq!(
        cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap(),
        rows
    );
    assert_eq!(cursor.row_stats().snapshot_records, 0);
    assert!(cursor.next().is_none());
}

#[test]
fn grouped_with_filters_top_k_unwind_and_regrouping_follow_committed_changes() {
    let ((), report) = run_async_under_lab(0x6772_3030, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = seed(&commit).await;
        let args = GqlParameters::new();
        let texts = [
            "MATCH (n:L) WITH n.p AS bucket,SUM(n.q) AS total RETURN bucket,total ORDER BY total DESC NULLS LAST,bucket ASC LIMIT 2",
            "MATCH (n:L) WITH n.p AS bucket,SUM(n.q) AS total WITH bucket,total WHERE total>=7 RETURN bucket,total ORDER BY total DESC,bucket ASC LIMIT 2",
            "MATCH (n:L) WITH n.p AS bucket,COUNT(*) AS occurrences RETURN occurrences AS frequency,COUNT(*) AS buckets ORDER BY buckets DESC,frequency ASC LIMIT 3",
            "MATCH (n:L) WITH n.p AS bucket,SUM(n.q) AS total ORDER BY total DESC NULLS LAST,bucket ASC LIMIT 2 RETURN COUNT(*) AS selected,SUM(total) AS selected_total",
            "MATCH (n:L) WITH n.p AS bucket,SUM(n.q) AS total UNWIND [total,total] AS copy RETURN bucket,copy ORDER BY copy DESC NULLS LAST,bucket ASC LIMIT 3",
            "MATCH (n:L) WHERE n.p<0 WITH COUNT(*) AS empty RETURN empty",
            "MATCH (a)-[e:R]->(b) WITH e.p AS bucket,SUM(e.q) AS total RETURN bucket,total ORDER BY total DESC NULLS LAST,bucket ASC LIMIT 2",
        ];
        let handles: Vec<_> = texts
            .iter()
            .map(|text| {
                let prepared = PreparedNativeRead::prepare(text, &args, symbols).unwrap();
                let (nodes, width) = prepared.subscription_footprint(&cx, &args).unwrap();
                assert!(nodes >= 2 && width > 0, "{text}");
                prepared
                    .register_standing(&mut db, &cx, &args, policy())
                    .unwrap()
            })
            .collect();
        for tick in 0..4 {
            for (text, handle) in texts.iter().zip(&handles) {
                assert_native(&db, &cx, handle, text, &args);
            }
            if tick == 3 {
                break;
            }
            let mut change = WriteBatch::new(RelationId(1));
            match tick {
                0 => {
                    change.set_vertex_property(VId(1), Q, Some(CanonicalScalar::Int(20)));
                }
                1 => {
                    change.set_vertex_property(VId(2), P, Some(CanonicalScalar::Int(3)));
                }
                _ => {
                    change.delete_edge(EId(103));
                }
            }
            db.write(&commit, change).await.unwrap();
        }
        for (text, handle) in texts.iter().zip(&handles) {
            db.rebuild_standing_query(&cx, handle, policy()).unwrap();
            assert_native(&db, &cx, handle, text, &args);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn grouped_top_k_refills_after_deletions_and_preserves_independent_parameters() {
    let ((), report) = run_async_under_lab(0x6772_3031, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = seed(&commit).await;
        let text = "MATCH (n:L) WITH n.p AS bucket,SUM(n.q) AS total WITH bucket,total*$scale AS score RETURN bucket,score ORDER BY score DESC NULLS LAST,bucket ASC LIMIT 2";
        let positive = GqlParameters::new().with_int64("scale", 1).unwrap();
        let negative = GqlParameters::new().with_int64("scale", -1).unwrap();
        let template = PreparedNativeRead::prepare(text, &positive, symbols).unwrap();
        let a = template
            .register_standing(&mut db, &cx, &positive, policy())
            .unwrap();
        let b = template
            .register_standing(&mut db, &cx, &negative, policy())
            .unwrap();
        drop(template);
        assert_native(&db, &cx, &a, text, &positive);
        assert_native(&db, &cx, &b, text, &negative);
        let QueryResult::Rows { rows, .. } =
            db.standing_native_query(&cx, &a, delivery()).unwrap().1
        else {
            panic!("rows");
        };
        assert_eq!(
            rows[0][0],
            QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Int(2)))
        );
        assert_eq!(
            rows[1][0],
            QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Int(3)))
        );
        let mut change = WriteBatch::new(RelationId(1));
        change.delete_edge(EId(103));
        change.delete_edge(EId(104));
        change.delete_vertex(VId(3));
        change.delete_vertex(VId(4));
        db.write(&commit, change).await.unwrap();
        assert_native(&db, &cx, &a, text, &positive);
        assert_native(&db, &cx, &b, text, &negative);
        let QueryResult::Rows { rows, .. } =
            db.standing_native_query(&cx, &a, delivery()).unwrap().1
        else {
            panic!("rows");
        };
        assert_eq!(
            rows[0][0],
            QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Int(3)))
        );
        assert_eq!(
            rows[1][0],
            QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Int(1)))
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn edge_fields_preserve_parallel_occurrences_directions_and_cascades() {
    let ((), report) = run_async_under_lab(0x6772_3036, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let (cx, commit) = (c.query(), c.commit());
        let mut db = seed(&commit).await;
        let mut extra = WriteBatch::new(RelationId(1));
        extra.add_edge(
            EId(u128::MAX),
            VId(1),
            VId(2),
            vec![(P, CanonicalScalar::Int(1)), (Q, CanonicalScalar::Int(4))],
        );
        extra.add_edge(
            EId(0),
            VId(1),
            VId(1),
            vec![(P, CanonicalScalar::Int(3)), (Q, CanonicalScalar::Int(2))],
        );
        db.write(&commit, extra).await.unwrap();
        let args = GqlParameters::new();
        let statements: Vec<_> = ["(a)-[e:R]->(b)", "(a)<-[e:R]-(b)", "(a)-[e:R]-(b)"]
            .into_iter().map(|pattern| format!(
                "MATCH {pattern} WITH e.p AS bucket,SUM(e.q) AS total,COUNT(*) AS copies RETURN bucket,total,copies ORDER BY bucket LIMIT 10"
            )).collect();
        let handles: Vec<_> = statements
            .iter()
            .map(|text| {
                db.register_standing_native(&cx, text, &args, symbols, policy())
                    .unwrap()
            })
            .collect();
        let identity_text = "MATCH (a)-[e:R]->(b) RETURN e AS edge";
        let identity = db
            .register_standing_native(&cx, identity_text, &args, symbols, policy())
            .unwrap();
        let expected_ids: [&[u128]; 5] = [
            &[0, 102, 103, 104, 105, 106, u128::MAX],
            &[0, 102, 103, 104, 105, 106, u128::MAX],
            // Removing edge102's q field retains its identity and NULL total.
            &[0, 102, 103, 104, 105, 106, u128::MAX - 1],
            &[u128::MAX - 1],
            &[],
        ];
        // Independent, hand-enumerated groups of the fixture after each tick.
        let expected = [
            vec![
                (1, Some(8), 2),
                (2, Some(10), 2),
                (3, Some(11), 2),
                (4, None, 1),
            ],
            vec![
                (1, Some(8), 2),
                (2, Some(7), 1),
                (3, Some(16), 3),
                (4, None, 1),
            ],
            vec![
                (1, None, 1),
                (2, Some(7), 1),
                (3, Some(16), 3),
                (4, None, 1),
                (5, Some(-2), 1),
            ],
            vec![(5, Some(-2), 1)],
            vec![],
        ];
        let cell = |value| QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Int(value)));
        for (tick, groups) in expected.iter().enumerate() {
            assert_native(&db, &cx, &identity, identity_text, &args);
            let QueryResult::Rows { columns, rows } = db
                .standing_native_query(&cx, &identity, delivery())
                .unwrap()
                .1
            else {
                panic!("edge rows");
            };
            assert_eq!(columns, vec![String::from("edge")]);
            let ids: Vec<_> = rows
                .iter()
                .map(|row| match row.as_slice() {
                    [QueryValue::Value(GraphValue::Edge(eid))] => eid.0,
                    _ => panic!("edge identity domain"),
                })
                .collect();
            assert_eq!(ids, expected_ids[tick], "identity tick {tick}");
            db.rebuild_standing_query(&cx, &identity, policy()).unwrap();
            assert_native(&db, &cx, &identity, identity_text, &args);
            for (direction, (text, handle)) in statements.iter().zip(&handles).enumerate() {
                assert_native(&db, &cx, handle, text, &args);
                let factor = if direction == 2 { 2 } else { 1 };
                let rows: Vec<_> = groups
                    .iter()
                    .map(|&(bucket, total, copies)| {
                        // Every non-loop has two undirected occurrences; E0 is one.
                        let loop_correction = if bucket == 3 && tick < 3 {
                            factor - 1
                        } else {
                            0
                        };
                        vec![
                            cell(bucket),
                            total.map_or(
                                QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Null)),
                                |total| cell(total * factor - 2 * loop_correction),
                            ),
                            cell(copies * factor - loop_correction),
                        ]
                    })
                    .collect();
                assert_eq!(
                    db.standing_native_query(&cx, handle, delivery()).unwrap().1,
                    QueryResult::Rows {
                        columns: vec!["bucket".into(), "total".into(), "copies".into()],
                        rows
                    }
                );
                db.rebuild_standing_query(&cx, handle, policy()).unwrap();
                assert_native(&db, &cx, handle, text, &args);
            }
            let mut change = WriteBatch::new(RelationId(1));
            match tick {
                0 => {
                    change.set_edge_property(EId(103), Q, Some(CanonicalScalar::Int(7)));
                    change.set_edge_property(EId(104), P, Some(CanonicalScalar::Int(3)));
                    // The edge field must never be read from a vertex's domain.
                    change.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(999)));
                    change.set_vertex_property(VId(1), Q, Some(CanonicalScalar::Int(999)));
                }
                1 => {
                    change.set_edge_property(EId(102), Q, None);
                    change.delete_edge(EId(u128::MAX));
                    change.add_edge(
                        EId(u128::MAX - 1),
                        VId(2),
                        VId(3),
                        vec![(P, CanonicalScalar::Int(5)), (Q, CanonicalScalar::Int(-2))],
                    );
                }
                2 => {
                    change.delete_vertex(VId(1));
                }
                3 => {
                    change.delete_vertex(VId(3));
                }
                _ => break,
            }
            db.write(&commit, change).await.unwrap();
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn edge_field_ticks_are_sparse_and_failed_groups_rebuild_from_committed_values() {
    let ((), report) = run_async_under_lab(0x6772_3037, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let (cx, commit) = (c.query(), c.commit());
        let args = GqlParameters::new();
        let text = "MATCH (a:L)-[e:R]->(b) WHERE a.p=1 WITH e.p AS bucket,SUM(e.q) AS total RETURN bucket,total ORDER BY bucket LIMIT 10";
        let mut measured = Vec::new();
        for unrelated in [0, 64] {
            let mut db = seed(&commit).await;
            if unrelated != 0 {
                let mut extra = WriteBatch::new(RelationId(1));
                for at in 0..unrelated {
                    let id = 1000 + 2 * at;
                    for vertex in [id, id + 1] {
                        extra.create_vertex(
                            VId(vertex),
                            vec![LabelId(1)],
                            vec![(P, CanonicalScalar::Int(99))],
                        );
                    }
                    extra.add_edge(
                        EId(id),
                        VId(id),
                        VId(id + 1),
                        vec![
                            (P, CanonicalScalar::Int(99)),
                            (Q, CanonicalScalar::Int(100)),
                        ],
                    );
                }
                db.write(&commit, extra).await.unwrap();
            }
            let handle = db
                .register_standing_native(&cx, text, &args, symbols, policy())
                .unwrap();
            let mut change = WriteBatch::new(RelationId(1));
            change.set_edge_property(EId(103), Q, Some(CanonicalScalar::Int(7)));
            db.write(&commit, change).await.unwrap();
            assert_native(&db, &cx, &handle, text, &args);
            measured.push(*db.standing_window(&cx, &handle).unwrap().last_maintenance());

            let healthy = db.standing_native_query(&cx, &handle, delivery()).unwrap();
            assert!(
                db.rebuild_standing_query(
                    &cx,
                    &handle,
                    GqlQueryPolicy::new(0, 100, 50_000_000, 50_000_000)
                )
                .is_err()
            );
            assert_eq!(
                db.standing_native_query(&cx, &handle, delivery()).unwrap(),
                healthy
            );
            let mut bad = WriteBatch::new(RelationId(1));
            bad.set_edge_property(
                EId(103),
                Q,
                Some(CanonicalScalar::ucs_basic_text("not numeric").unwrap()),
            );
            let bad_at = db.write(&commit, bad).await.unwrap();
            assert_eq!(db.frontier().unwrap(), bad_at);
            assert!(db.standing_native_query(&cx, &handle, delivery()).is_err());
            assert!(db.query(&cx, text, &args, symbols, policy()).is_err());
            let mut repair = WriteBatch::new(RelationId(1));
            repair.set_edge_property(EId(103), Q, Some(CanonicalScalar::Int(8)));
            db.write(&commit, repair).await.unwrap();
            db.rebuild_standing_query(&cx, &handle, policy()).unwrap();
            assert_native(&db, &cx, &handle, text, &args);
            for supported in [
                "MATCH (a)-[e:R]->(b)-[:R]->(c) RETURN e.p AS p",
                "MATCH (a)-[e:R]->(b)-[:R]->(c) RETURN e AS edge",
                "MATCH (a) OPTIONAL MATCH (a)-[e:R]->(b)-[:R]->(c) RETURN e.p AS p",
            ] {
                let handle = db
                    .register_standing_native(&cx, supported, &args, symbols, policy())
                    .unwrap();
                assert_native(&db, &cx, &handle, supported, &args);
            }
        }
        assert_eq!(measured[0], measured[1]);
        assert_eq!(measured[0].affected_edges, 1);
        assert!(measured[0].work_units > 0 && measured[0].scratch_entries > 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn intermediate_root_group_rebuild_keeps_row_representation_and_native_cursors() {
    let ((), report) = run_async_under_lab(0x6772_3032, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = seed(&commit).await;
        let mut builder = GraphPatternBuilder::new();
        builder.vertex("n").unwrap();
        let input: PreparedGraphSet = builder
            .prepare_values(
                &[
                    GraphColumn::property("bucket", "n", P),
                    GraphColumn::property("value", "n", Q),
                ],
                0,
                None,
            )
            .unwrap()
            .with_duplicates()
            .into();
        let query = input
            .group_by(&[0], &[GraphAggregate::sum_int("total", 1)])
            .unwrap();
        let handle = db
            .register_standing_relation(&cx, &query, policy())
            .unwrap();
        for step in 0..2 {
            let expected = query
                .execute_governed(
                    policy(),
                    |pattern: &PreparedGraphPattern<GraphValueRow>, allowance: GqlQueryPolicy| {
                        db.execute_graph_pattern_governed_at(
                            &cx,
                            pattern,
                            db.frontier().unwrap(),
                            allowance,
                        )
                    },
                    || cx.checkpoint(),
                )
                .unwrap()
                .value;
            let expected: Vec<Vec<QueryValue>> = expected
                .into_iter()
                .map(|row| {
                    row.values()
                        .iter()
                        .cloned()
                        .map(QueryValue::Value)
                        .collect()
                })
                .collect();
            let (at, result) = db.standing_native_query(&cx, &handle, delivery()).unwrap();
            assert_eq!(at, db.frontier().unwrap());
            assert_eq!(
                result,
                QueryResult::Rows {
                    columns: query.columns().to_vec(),
                    rows: expected.clone(),
                }
            );
            assert_eq!(
                db.standing_native_cursor(&cx, &handle, delivery())
                    .unwrap()
                    .collect::<Result<Vec<_>, _>>()
                    .unwrap(),
                expected
            );
            if step == 0 {
                let mut change = WriteBatch::new(RelationId(1));
                change.set_vertex_property(VId(1), Q, Some(CanonicalScalar::Int(1)));
                db.write(&commit, change).await.unwrap();
                db.rebuild_standing_query(&cx, &handle, policy()).unwrap();
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn conversion_errors_are_not_hidden_by_zero_pages_and_unbounded_sorts_still_refuse() {
    let ((), report) = run_async_under_lab(0x6772_3033, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(RelationId(1));
        for (id, value) in [(1, i64::MAX), (2, 1)] {
            seed.create_vertex(
                VId(id),
                vec![LabelId(1)],
                vec![(Q, CanonicalScalar::Int(value))],
            );
        }
        db.write(&commit, seed).await.unwrap();
        let text = "MATCH (n:L) WITH SUM(n.q) AS total RETURN total LIMIT 0";
        assert!(matches!(
            db.register_standing_native(&cx, text, &GqlParameters::new(), symbols, policy()),
            Err(StandingQueryError::Maintenance(
                StandingQueryFailure::OutputExpression { error, .. }
            )) if error.kind == GraphIntegerErrorKind::Overflow
        ));
        // An AVG of integers is no conversion failure: WITH carries the
        // binary64 nearest its exact mean, (2^63 - 1 + 1) / 2 = 2^62 here
        // (fgdb-y255t).
        let text = "MATCH (n:L) WITH AVG(n.q) AS mean RETURN mean";
        let handle = db
            .register_standing_native(&cx, text, &GqlParameters::new(), symbols, policy())
            .unwrap();
        assert_native(&db, &cx, &handle, text, &GqlParameters::new());
        let (_, QueryResult::Rows { rows, .. }) =
            db.standing_native_query(&cx, &handle, delivery()).unwrap()
        else {
            panic!("query rows");
        };
        assert_eq!(
            rows,
            vec![vec![QueryValue::Value(GraphValue::Scalar(
                CanonicalScalar::Float(CanonicalF64::new(4_611_686_018_427_387_904.0))
            ))]]
        );
        let text = "MATCH (n:L) WITH n.p AS bucket,COUNT(*) AS copies RETURN bucket,copies ORDER BY copies";
        assert!(matches!(
            db.register_standing_native(&cx, text, &GqlParameters::new(), symbols, policy()),
            Err(StandingQueryError::Unsupported)
        ));
        // Exact terminal results still retain their wide domain.
        assert!(
            db.query(
                &cx,
                "MATCH (n:L) RETURN SUM(n.q) AS total",
                &GqlParameters::new(),
                symbols,
                policy(),
            )
            .is_ok()
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn group_input_quotas_survive_nested_rebuild_without_relaxing_siblings_or_sources() {
    use fgdb_gql::{GraphSetOperation, GraphSetProjection, GraphSetQuantifier, GraphSetValue};
    let ((), report) = run_async_under_lab(0x6772_3034, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = seed(&commit).await;
        let mut builder = GraphPatternBuilder::new();
        builder.vertex("n").unwrap();
        let input: PreparedGraphSet = builder
            .prepare_values(&[GraphColumn::property("bucket", "n", P)], 0, None)
            .unwrap()
            .with_duplicates()
            .into();
        let global = input
            .clone()
            .group_by(&[], &[GraphAggregate::count_rows("total")])
            .unwrap();
        let nested = input
            .group_by(&[0], &[GraphAggregate::count_rows("copies")])
            .unwrap()
            .group_by(&[], &[GraphAggregate::sum_int("total", 1)])
            .unwrap();
        let queries = [global.clone(), nested];
        let one = GqlQueryPolicy::new(100, 1, 50_000_000, 50_000_000);
        let handles: Vec<_> = queries
            .iter()
            .map(|query| db.register_standing_relation(&cx, query, one).unwrap())
            .collect();
        for (step, count) in [6, 7, 6].into_iter().enumerate() {
            for handle in &handles {
                let expected = QueryResult::Rows {
                    columns: vec!["total".into()],
                    rows: vec![vec![QueryValue::Value(GraphValue::Scalar(
                        CanonicalScalar::Int(count),
                    ))]],
                };
                assert_eq!(
                    db.standing_native_query(&cx, handle, delivery()).unwrap().1,
                    expected
                );
                db.rebuild_standing_query(&cx, handle, one).unwrap();
                assert_eq!(
                    db.standing_native_query(&cx, handle, delivery()).unwrap().1,
                    expected
                );
                // A failed rebuild must leave the accepted circuit unchanged.
                for refused in [
                    GqlQueryPolicy::new(100, 0, 50_000_000, 50_000_000),
                    GqlQueryPolicy::new(0, 1, 50_000_000, 50_000_000),
                ] {
                    assert!(db.rebuild_standing_query(&cx, handle, refused).is_err());
                    assert_eq!(
                        db.standing_native_query(&cx, handle, delivery()).unwrap().1,
                        expected
                    );
                }
            }
            if step == 2 {
                break;
            }
            let mut change = WriteBatch::new(RelationId(1));
            if step == 0 {
                change.create_vertex(
                    VId(999),
                    vec![LabelId(1)],
                    vec![(P, CanonicalScalar::Int(10))],
                );
            } else {
                change.delete_vertex(VId(999));
            }
            db.write(&commit, change).await.unwrap();
        }
        // The right branch is NOT a group input. Its three equal occurrences
        // exceed a quota of two even though UNION DISTINCT returns two rows.
        let literal = GraphSetValue::Value(GraphValue::Scalar(CanonicalScalar::Int(1)));
        let right = PreparedGraphSet::singleton()
            .unwind(
                "unused".into(),
                GraphSetValue::List(vec![literal.clone(); 3]),
            )
            .unwrap()
            .project(
                vec![GraphSetProjection::new("total", literal)],
                GraphSetQuantifier::All,
            )
            .unwrap();
        let union = global
            .combine(
                GraphSetOperation::Union,
                GraphSetQuantifier::Distinct,
                right,
            )
            .unwrap();
        let sibling = db
            .register_standing_relation(&cx, &union, policy())
            .unwrap();
        let before = db.standing_native_query(&cx, &sibling, delivery()).unwrap();
        assert!(
            db.rebuild_standing_query(
                &cx,
                &sibling,
                GqlQueryPolicy::new(100, 2, 50_000_000, 50_000_000),
            )
            .is_err()
        );
        assert_eq!(
            db.standing_native_query(&cx, &sibling, delivery()).unwrap(),
            before
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn conversion_failure_fences_derived_rows_not_committed_writes_and_rebuild_recovers() {
    let ((), report) = run_async_under_lab(0x6772_3035, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut initial = WriteBatch::new(RelationId(1));
        for (id, value) in [(1, i64::MAX - 1), (2, 1)] {
            initial.create_vertex(
                VId(id),
                vec![LabelId(1)],
                vec![(Q, CanonicalScalar::Int(value))],
            );
        }
        db.write(&commit, initial).await.unwrap();
        let text = "MATCH (n:L) WITH SUM(n.q) AS total RETURN total";
        let params = GqlParameters::new();
        let handle = db
            .register_standing_native(&cx, text, &params, symbols, policy())
            .unwrap();
        assert_native(&db, &cx, &handle, text, &params);
        let mut overflow = WriteBatch::new(RelationId(1));
        overflow.set_vertex_property(VId(2), Q, Some(CanonicalScalar::Int(2)));
        let at = db.write(&commit, overflow).await.unwrap();
        assert_eq!(db.frontier().unwrap(), at);
        assert!(db.standing_native_query(&cx, &handle, delivery()).is_err());
        let QueryResult::Rows { rows, .. } = db
            .query(
                &cx,
                "MATCH (n:L) RETURN SUM(n.q) AS total",
                &params,
                symbols,
                policy(),
            )
            .unwrap()
        else {
            panic!("exact aggregate rows");
        };
        assert_eq!(rows[0][0].as_integer(), Some(i128::from(i64::MAX) + 1));
        assert!(db.rebuild_standing_query(&cx, &handle, policy()).is_err());
        assert_eq!(db.frontier().unwrap(), at);
        let mut repair = WriteBatch::new(RelationId(1));
        repair.set_vertex_property(VId(2), Q, Some(CanonicalScalar::Int(0)));
        let repaired = db.write(&commit, repair).await.unwrap();
        assert_eq!(
            db.rebuild_standing_query(&cx, &handle, policy()).unwrap(),
            repaired
        );
        assert_native(&db, &cx, &handle, text, &params);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn multi_hop_edge_captures_follow_each_atom_through_field_changes_and_rebuild() {
    fn value(edge: &fgdb::EdgeRecord, key: PropertyKeyId) -> QueryValue {
        QueryValue::Value(
            edge.props
                .iter()
                .find(|(actual, _)| *actual == key)
                .map(|(_, value)| GraphValue::from_property_value(value).unwrap())
                .unwrap_or(GraphValue::Scalar(CanonicalScalar::Null)),
        )
    }
    // Definition-level nested loop over physical EIds, independent of the
    // maintained arrangements, anchor ownership, captures and aggregate kernel.
    fn oracle(db: &Database<MemVfs>) -> (QueryResult, QueryResult) {
        let edges = db.edges().unwrap();
        let mut paths = Vec::new();
        let mut groups = std::collections::BTreeMap::<i64, (Option<i64>, i64)>::new();
        for r in edges
            .iter()
            .filter(|edge| edge.entry.relation == RelationId(1))
        {
            for s in edges.iter().filter(|edge| {
                edge.entry.relation == RelationId(2) && edge.entry.src == r.entry.dst
            }) {
                let QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Int(bucket))) =
                    value(r, P)
                else {
                    panic!("fixture bucket");
                };
                let amount = value(s, Q);
                let group = groups.entry(bucket).or_insert((None, 0));
                group.1 += 1;
                if let QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Int(amount))) = &amount
                {
                    group.0 = Some(group.0.unwrap_or(0) + *amount);
                }
                paths.push((
                    (r.entry.eid, s.entry.eid),
                    vec![
                        QueryValue::Value(GraphValue::Edge(r.entry.eid)),
                        QueryValue::Value(GraphValue::Edge(s.entry.eid)),
                        value(r, P),
                        amount,
                    ],
                ));
            }
        }
        paths.sort_by_key(|(ids, _)| *ids);
        let cell = |value| QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Int(value)));
        (
            QueryResult::Rows {
                columns: vec![
                    "first".into(),
                    "second".into(),
                    "bucket".into(),
                    "amount".into(),
                ],
                rows: paths.into_iter().map(|(_, row)| row).collect(),
            },
            QueryResult::Rows {
                columns: vec!["bucket".into(), "total".into(), "copies".into()],
                rows: groups
                    .into_iter()
                    .map(|(bucket, (total, copies))| {
                        vec![
                            cell(bucket),
                            total.map_or(
                                QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Null)),
                                cell,
                            ),
                            cell(copies),
                        ]
                    })
                    .collect(),
            },
        )
    }
    let ((), report) = run_async_under_lab(0x6772_3040, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let (cx, commit) = (c.query(), c.commit());
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut r = WriteBatch::new(RelationId(1));
        for id in 1..=5 {
            r.create_vertex(VId(id), vec![], vec![(P, CanonicalScalar::Int(999))]);
        }
        for (eid, a, b, p, q) in [
            (0, 1, 2, 10, 2),
            (u128::MAX, 1, 2, 20, 3),
            (101, 2, 3, 30, 5),
            (102, 2, 2, 40, 7),
        ] {
            r.add_edge(
                EId(eid),
                VId(a),
                VId(b),
                vec![(P, CanonicalScalar::Int(p)), (Q, CanonicalScalar::Int(q))],
            );
        }
        db.write(&commit, r).await.unwrap();
        let mut s = WriteBatch::new(RelationId(2));
        for (eid, a, b, p, q) in [
            (200, 2, 3, 50, Some(11)),
            (201, 2, 4, 60, Some(13)),
            (202, 3, 5, 70, None),
            (203, 2, 2, 80, Some(17)),
        ] {
            let mut fields = vec![(P, CanonicalScalar::Int(p))];
            if let Some(q) = q {
                fields.push((Q, CanonicalScalar::Int(q)));
            }
            s.add_edge(EId(eid), VId(a), VId(b), fields);
        }
        db.write(&commit, s).await.unwrap();
        let texts = [
            "MATCH (a)-[r:R]->(b)-[s:S]->(c) RETURN r AS first,s AS second,r.p AS bucket,s.q AS amount ORDER BY first,second LIMIT 1000",
            "MATCH (a)-[r:R]->(b)-[s:S]->(c) WITH r.p AS bucket,SUM(s.q) AS total,COUNT(*) AS copies RETURN bucket,total,copies ORDER BY bucket LIMIT 1000",
            "MATCH (c)<-[s:S]-(b)<-[r:R]-(a) RETURN r AS first,s AS second,r.p AS bucket,s.q AS amount ORDER BY first,second LIMIT 1000",
            "MATCH (a)-[r:R]->(b)-[s:S]->(c) WITH r.p AS bucket,SUM(r.q*s.q) AS total,COUNT(DISTINCT s) AS partners RETURN bucket,total,partners ORDER BY total DESC NULLS LAST,bucket LIMIT 2",
            "MATCH (a)-[r:R]-(b)-[s:S]-(c) RETURN r AS first,s AS second,r.p AS bucket,s.q AS amount ORDER BY first,second,bucket,amount LIMIT 1000",
            "MATCH (a)-[r:R]->(b)-[s:R]->(c) RETURN r AS first,s AS second,r.p AS bucket,s.q AS amount ORDER BY first,second LIMIT 1000",
            "MATCH (a)-[r]->(b)-[s]->(c) RETURN r AS first,s AS second,r.p AS bucket,s.q AS amount ORDER BY first,second LIMIT 1000",
            "MATCH (a)-[r:R]->(b)-[s:S]->(c) RETURN COUNT(r) AS lefts,COUNT(DISTINCT s) AS rights,MIN(r.p) AS smallest,MAX(s.q) AS largest",
        ];
        let args = GqlParameters::new();
        let handles: Vec<_> = texts
            .iter()
            .map(|text| {
                let prepared = PreparedNativeRead::prepare(text, &args, symbols).unwrap();
                let (_, width) = prepared.subscription_footprint(&cx, &args).unwrap();
                assert!(width >= 3);
                prepared
                    .register_standing(&mut db, &cx, &args, policy())
                    .unwrap_or_else(|error| panic!("{text}: {error:?}"))
            })
            .collect();
        for tick in 0..=8 {
            let (paths, groups) = oracle(&db);
            for (index, (text, handle)) in texts.iter().zip(&handles).enumerate() {
                assert_native(&db, &cx, handle, text, &args);
                let actual = db.standing_native_query(&cx, handle, delivery()).unwrap().1;
                if index == 0 || index == 2 {
                    assert_eq!(actual, paths, "physical edge oracle, tick {tick}");
                } else if index == 1 {
                    assert_eq!(actual, groups, "group oracle, tick {tick}");
                }
                if tick == 3 || tick == 8 {
                    db.rebuild_standing_query(&cx, handle, policy()).unwrap();
                    assert_native(&db, &cx, handle, text, &args);
                }
                if tick == 5 {
                    assert!(
                        db.standing_query_node_stats(&cx, handle)
                            .unwrap()
                            .all(|stats| stats.affected_edges == 0),
                        "unrequested property does not invalidate: {text}"
                    );
                }
            }
            if tick == 8 {
                break;
            }
            let mut change = WriteBatch::new(if matches!(tick, 1 | 3 | 7) {
                RelationId(2)
            } else {
                RelationId(1)
            });
            match tick {
                0 => {
                    // Two affected atoms can belong to one self-join occurrence.
                    change.set_edge_property(EId(0), Q, Some(CanonicalScalar::Int(4)));
                    change.set_edge_property(EId(101), P, Some(CanonicalScalar::Int(90)));
                    change.set_edge_property(EId(102), Q, Some(CanonicalScalar::Int(10)));
                    change.set_vertex_property(VId(2), P, Some(CanonicalScalar::Int(-999)));
                }
                1 => {
                    change.set_edge_property(EId(200), Q, Some(CanonicalScalar::Int(0)));
                    change.set_edge_property(EId(201), Q, None);
                    change.set_edge_property(EId(202), Q, Some(CanonicalScalar::Int(19)));
                }
                2 => {
                    change.delete_edge(EId(u128::MAX));
                    change.add_edge(
                        EId(500),
                        VId(1),
                        VId(2),
                        vec![(P, CanonicalScalar::Int(10)), (Q, CanonicalScalar::Int(-1))],
                    );
                }
                3 => {
                    change.add_edge(
                        EId(501),
                        VId(3),
                        VId(1),
                        vec![(P, CanonicalScalar::Int(10)), (Q, CanonicalScalar::Int(23))],
                    );
                }
                4 => {
                    change.set_edge_property(
                        EId(0),
                        PropertyKeyId(3),
                        Some(CanonicalScalar::Int(777)),
                    );
                }
                5 => {
                    change.delete_vertex(VId(2));
                }
                6 => {
                    change.add_edge(
                        EId(502),
                        VId(4),
                        VId(3),
                        vec![(P, CanonicalScalar::Int(10)), (Q, CanonicalScalar::Int(2))],
                    );
                }
                _ => {
                    change.set_edge_property(EId(202), Q, None);
                    change.set_edge_property(EId(501), Q, Some(CanonicalScalar::Null));
                }
            }
            db.write(&commit, change).await.unwrap();
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn multi_hop_edge_field_maintenance_visits_only_affected_join_partners() {
    let ((), report) = run_async_under_lab(0x6772_3041, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let (cx, commit) = (c.query(), c.commit());
        let args = GqlParameters::new();
        for text in [
            "MATCH (a)-[r:R]->(b)-[s:R]->(c) WITH r.p AS bucket,SUM(s.q) AS total,COUNT(*) AS copies RETURN bucket,total,copies ORDER BY bucket LIMIT 1000",
            "MATCH (a) OPTIONAL MATCH (a)-[r:R]->(b)-[s:R]->(c) WITH r.p AS bucket,SUM(s.q) AS total,COUNT(*) AS copies RETURN bucket,total,copies ORDER BY bucket LIMIT 1000",
        ] {
            let mut measurements = Vec::new();
            for unrelated in [1, 64] {
                let mut db = Database::open_memory(&commit, keys()).await.unwrap();
                let mut seed = WriteBatch::new(RelationId(1));
                for component in 0..=unrelated {
                    let a = 10 * component + 1;
                    for vertex in [a, a + 1, a + 2] {
                        seed.create_vertex(VId(vertex), vec![], vec![]);
                    }
                    seed.add_edge(
                        EId(a),
                        VId(a),
                        VId(a + 1),
                        vec![(P, CanonicalScalar::Int(1))],
                    );
                    seed.add_edge(
                        EId(a + 1),
                        VId(a + 1),
                        VId(a + 2),
                        vec![(Q, CanonicalScalar::Int(3))],
                    );
                }
                db.write(&commit, seed).await.unwrap();
                let handle = db
                    .register_standing_native(&cx, text, &args, symbols, policy())
                    .unwrap();
                let mut change = WriteBatch::new(RelationId(1));
                change.set_edge_property(EId(2), Q, Some(CanonicalScalar::Int(7)));
                db.write(&commit, change).await.unwrap();
                assert_native(&db, &cx, &handle, text, &args);
                let stats: Vec<_> = db
                    .standing_query_node_stats(&cx, &handle)
                    .unwrap()
                    .copied()
                    .collect();
                assert_eq!(
                    stats.iter().map(|stats| stats.affected_edges).max(),
                    Some(1)
                );
                measurements.push(stats);
            }
            // Both sizes keep the unchanged value row and aggregate group alive;
            // extra components do not change the source or downstream work.
            assert_eq!(measurements[0], measurements[1]);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn optional_multi_hop_edge_fields_follow_complete_witnesses_and_null_transitions() {
    fn field(edge: &fgdb::EdgeRecord, key: PropertyKeyId) -> QueryValue {
        QueryValue::Value(
            edge.props
                .iter()
                .find(|(actual, _)| *actual == key)
                .map(|(_, value)| GraphValue::from_property_value(value).unwrap())
                .unwrap_or(GraphValue::Scalar(CanonicalScalar::Null)),
        )
    }
    // Independent physical-occurrence oracle. A reachable prefix contributes
    // no relationship fields when the whole OPTIONAL pattern has no witness.
    fn oracle(db: &Database<MemVfs>) -> (QueryResult, QueryResult) {
        let edges = db.edges().unwrap();
        let mut vertices = db.vertices().unwrap();
        vertices.sort_unstable_by_key(|vertex| vertex.vid);
        let null = QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Null));
        let cell = |value| QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Int(value)));
        let mut paths = Vec::new();
        let mut groups = Vec::new();
        for vertex in vertices {
            let root = QueryValue::Value(GraphValue::Vertex(vertex.vid));
            let mut found = Vec::new();
            let mut sum = None;
            let mut ends = std::collections::BTreeSet::new();
            for r in edges
                .iter()
                .filter(|edge| edge.entry.relation == RelationId(1) && edge.entry.src == vertex.vid)
            {
                for s in edges.iter().filter(|edge| {
                    edge.entry.relation == RelationId(2) && edge.entry.src == r.entry.dst
                }) {
                    let amount = field(s, Q);
                    if let QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Int(value))) =
                        &amount
                    {
                        sum = Some(sum.unwrap_or(0) + *value);
                    }
                    ends.insert(s.entry.eid);
                    found.push((
                        (r.entry.eid, s.entry.eid),
                        vec![
                            root.clone(),
                            QueryValue::Value(GraphValue::Edge(r.entry.eid)),
                            QueryValue::Value(GraphValue::Edge(s.entry.eid)),
                            field(r, P),
                            amount,
                        ],
                    ));
                }
            }
            let witnesses = i64::try_from(found.len()).unwrap();
            groups.push(vec![
                root.clone(),
                sum.map_or_else(|| null.clone(), cell),
                cell(witnesses.max(1)),
                cell(witnesses),
                cell(i64::try_from(ends.len()).unwrap()),
            ]);
            if found.is_empty() {
                paths.push(vec![
                    root,
                    null.clone(),
                    null.clone(),
                    null.clone(),
                    null.clone(),
                ]);
            } else {
                found.sort_unstable_by_key(|(ids, _)| *ids);
                paths.extend(found.into_iter().map(|(_, row)| row));
            }
        }
        (
            QueryResult::Rows {
                columns: vec![
                    "root".into(),
                    "first".into(),
                    "second".into(),
                    "bucket".into(),
                    "amount".into(),
                ],
                rows: paths,
            },
            QueryResult::Rows {
                columns: vec![
                    "root".into(),
                    "total".into(),
                    "copies".into(),
                    "starts".into(),
                    "ends".into(),
                ],
                rows: groups,
            },
        )
    }
    let ((), report) = run_async_under_lab(0x6772_3042, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let (cx, commit) = (c.query(), c.commit());
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(RelationId(1));
        for id in 1..=6 {
            seed.create_vertex(VId(id), vec![], vec![(P, CanonicalScalar::Int(999))]);
        }
        for (eid, a, b, p) in [
            (0, 1, 2, 10),
            (u128::MAX, 1, 2, 20),
            (104, 4, 5, 40),
            (105, 2, 2, 50),
        ] {
            seed.add_edge(EId(eid), VId(a), VId(b), vec![(P, CanonicalScalar::Int(p))]);
        }
        db.write(&commit, seed).await.unwrap();
        let texts = [
            "MATCH (a) OPTIONAL MATCH (a)-[r:R]->(b)-[s:S]->(c) RETURN a AS root,r AS first,s AS second,r.p AS bucket,s.q AS amount ORDER BY root,first,second LIMIT 1000",
            "MATCH (a) OPTIONAL MATCH (a)-[r:R]->(b)-[s:S]->(c) WITH a AS root,SUM(s.q) AS total,COUNT(*) AS copies,COUNT(r) AS starts,COUNT(DISTINCT s) AS ends RETURN root,total,copies,starts,ends ORDER BY root LIMIT 1000",
            "MATCH (a) OPTIONAL MATCH (a)<-[r:R]-(b)<-[s:S]-(c) RETURN a AS root,r AS first,s AS second,r.p AS bucket,s.q AS amount ORDER BY root,first,second LIMIT 1000",
            "MATCH (a) OPTIONAL MATCH (a)-[r:R]-(b)-[s:S]-(c) RETURN a AS root,r AS first,s AS second,r.p AS bucket,s.q AS amount ORDER BY root,first,second LIMIT 1000",
            "MATCH (a) OPTIONAL MATCH (a)-[r:R]->(b)-[s:R]->(c) RETURN a AS root,r AS first,s AS second,r.p AS bucket,s.p AS amount ORDER BY root,first,second LIMIT 1000",
            "MATCH (a) OPTIONAL MATCH (a)-[r]->(b)-[s]->(c) RETURN a AS root,r AS first,s AS second,r.p AS bucket,s.q AS amount ORDER BY root,first,second LIMIT 1000",
            "MATCH (a) OPTIONAL MATCH (a)-[r:R]->(b)-[s:S]->(c)-[t:R]->(d) RETURN a AS root,r AS first,s AS second,t AS third,r.p AS bucket,s.q AS amount,t.p AS last ORDER BY root,first,second,third LIMIT 1000",
            "MATCH (a) OPTIONAL MATCH (a)-[r:R]->(b)-[s:S]->(c) WITH a AS root,r.p AS bucket,SUM(r.p*s.q) AS total,COUNT(DISTINCT r) AS starts RETURN root,bucket,total,starts ORDER BY root,bucket LIMIT 1000",
        ];
        let args = GqlParameters::new();
        let handles: Vec<_> = texts
            .iter()
            .map(|text| {
                let prepared = PreparedNativeRead::prepare(text, &args, symbols).unwrap();
                let (_, width) = prepared.subscription_footprint(&cx, &args).unwrap();
                assert!(width >= 4, "{text}");
                prepared
                    .register_standing(&mut db, &cx, &args, policy())
                    .unwrap_or_else(|error| panic!("{text}: {error:?}"))
            })
            .collect();
        for tick in 0..=9 {
            let (paths, groups) = oracle(&db);
            for (index, (text, handle)) in texts.iter().zip(&handles).enumerate() {
                assert_native(&db, &cx, handle, text, &args);
                let actual = db.standing_native_query(&cx, handle, delivery()).unwrap().1;
                if index == 0 {
                    assert_eq!(actual, paths, "physical OPTIONAL oracle, tick {tick}");
                } else if index == 1 {
                    assert_eq!(actual, groups, "nullable aggregate oracle, tick {tick}");
                }
                if tick == 2 || tick == 9 {
                    db.rebuild_standing_query(&cx, handle, policy()).unwrap();
                    assert_native(&db, &cx, handle, text, &args);
                }
                if tick == 8 {
                    assert!(
                        db.standing_query_node_stats(&cx, handle)
                            .unwrap()
                            .all(|stats| stats.affected_edges == 0),
                        "{text}"
                    );
                }
            }
            if tick == 9 {
                break;
            }
            let mut r = WriteBatch::new(RelationId(1));
            let mut s = WriteBatch::new(RelationId(2));
            match tick {
                0 => {
                    s.add_edge(EId(20), VId(2), VId(3), vec![(Q, CanonicalScalar::Int(7))]);
                    s.add_edge(EId(21), VId(2), VId(6), vec![]);
                    s.add_edge(EId(22), VId(2), VId(2), vec![(Q, CanonicalScalar::Int(9))]);
                    db.write(&commit, s).await.unwrap();
                }
                1 => {
                    r.set_edge_property(EId(0), P, Some(CanonicalScalar::Int(12)));
                    r.set_edge_property(EId(105), P, None);
                    db.write(&commit, r).await.unwrap();
                }
                2 => {
                    s.set_edge_property(EId(20), Q, Some(CanonicalScalar::Int(0)));
                    s.set_edge_property(EId(21), Q, Some(CanonicalScalar::Null));
                    s.set_edge_property(EId(22), Q, None);
                    db.write(&commit, s).await.unwrap();
                }
                3 => {
                    s.delete_edge(EId(20))
                        .delete_edge(EId(21))
                        .delete_edge(EId(22));
                    db.write(&commit, s).await.unwrap();
                }
                4 => {
                    // One atomic commit changes a projected prefix and adds
                    // its first complete witness in another relation.
                    r.set_edge_property(EId(104), P, Some(CanonicalScalar::Int(41)));
                    s.add_edge(EId(23), VId(5), VId(6), vec![(Q, CanonicalScalar::Int(11))]);
                    db.write_atomic(&commit, vec![s, r]).await.unwrap();
                }
                5 => {
                    s.delete_edge(EId(23));
                    s.add_edge(EId(24), VId(5), VId(6), vec![(Q, CanonicalScalar::Int(13))]);
                    db.write(&commit, s).await.unwrap();
                }
                6 => {
                    r.delete_vertex(VId(2));
                    db.write(&commit, r).await.unwrap();
                }
                7 => {
                    s.set_edge_property(EId(24), PropertyKeyId(3), Some(CanonicalScalar::Int(77)));
                    db.write(&commit, s).await.unwrap();
                }
                _ => {
                    r.delete_vertex(VId(5));
                    db.write(&commit, r).await.unwrap();
                }
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
