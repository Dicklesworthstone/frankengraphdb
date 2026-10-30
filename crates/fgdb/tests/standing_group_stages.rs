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
            for unsupported in [
                "MATCH (a)-[e:R]->(b)-[:R]->(c) RETURN e.p AS p",
                "MATCH (a) OPTIONAL MATCH (a)-[e:R]->(b)-[:R]->(c) RETURN e.p AS p",
                "MATCH (a)-[e:R]->(b)-[:R]->(c) RETURN e AS edge",
            ] {
                assert!(
                    matches!(
                        db.register_standing_native(&cx, unsupported, &args, symbols, policy()),
                        Err(StandingQueryError::Unsupported)
                    ),
                    "{unsupported}"
                );
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
