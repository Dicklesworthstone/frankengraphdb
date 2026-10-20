//! End-to-end relational ORDER BY under the existing database/lab/spill owner.
//! Literal expected sequences deliberately do not call a production comparator.
use super::*;
use fgdb_types::{CanonicalDecimal, CanonicalF64};

fn int(value: i64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(value))
}
fn float(value: f64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Float(CanonicalF64::new(value)))
}
fn decimal(value: i128) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Decimal(
        CanonicalDecimal::from_integer(value).unwrap(),
    ))
}
fn null() -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Null)
}
fn list(values: Vec<GraphValue>) -> GraphValue {
    GraphValue::List(values.into_boxed_slice())
}
fn values(input: &[GraphValue]) -> PreparedGraphSet {
    PreparedGraphSet::singleton()
        .unwind(
            "value".into(),
            GraphSetValue::List(input.iter().cloned().map(GraphSetValue::Value).collect()),
        )
        .unwrap()
}
fn encode_rows(rows: impl IntoIterator<Item = Vec<GraphValue>>) -> Vec<Vec<u8>> {
    rows.into_iter()
        .map(|row| {
            GraphValueRow::from_owned_values(row)
                .canonical_bytes()
                .unwrap()
        })
        .collect()
}
fn encode_values(values: &[GraphValue]) -> Vec<Vec<u8>> {
    encode_rows(values.iter().cloned().map(|value| vec![value]))
}
fn pair_input(rows: &[[GraphValue; 2]]) -> PreparedGraphSet {
    let input: Vec<_> = rows.iter().map(|row| list(row.to_vec())).collect();
    values(&input)
        .project(
            [("key", 0), ("tie", 1)]
                .into_iter()
                .map(|(name, at)| {
                    GraphSetProjection::new(
                        name,
                        GraphSetValue::Index {
                            list: Box::new(GraphSetValue::Column(0)),
                            index: Box::new(GraphSetValue::Value(int(at))),
                        },
                    )
                })
                .collect(),
            GraphSetQuantifier::All,
        )
        .unwrap()
}

#[test]
fn relational_numeric_order_keeps_typed_ties_in_both_directions_and_null_placements() {
    let ((), report) = run_async_under_lab(0xc01d_3151, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let vfs = fixture(&contexts.commit()).await;
        let pool = MemoryPool::new(32_000_000, 0).unwrap();
        let mut view = view(&contexts.commit(), &vfs, &pool).await;
        let baseline = pool.used();
        // Each group is a language-order tie, already in literal typed order.
        // DESC reverses groups, never the canonical tie-break within a group.
        let groups = [
            vec![float(f64::NEG_INFINITY)],
            vec![float(-1.5)],
            vec![int(0), float(-0.0)],
            vec![int(2), decimal(2), float(2.0)],
            vec![int(7), int(7), float(7.0)],
            vec![float(9_007_199_254_740_992.0)],
            vec![int(9_007_199_254_740_993)],
            vec![float(f64::INFINITY)],
            vec![float(f64::NAN)],
        ];
        let mut input: Vec<_> = groups.iter().flatten().cloned().collect();
        input.reverse();
        input.insert(3, null());
        let original = values(&input);
        for descending in [false, true] {
            for nulls_first in [false, true] {
                let mut expected = Vec::new();
                for at in 0..groups.len() {
                    let at = if descending {
                        groups.len() - 1 - at
                    } else {
                        at
                    };
                    expected.extend(groups[at].iter().cloned());
                }
                if nulls_first {
                    expected.insert(0, null());
                } else {
                    expected.push(null());
                }
                let query = original
                    .clone()
                    .with_order_by(&[GraphValueOrder {
                        column: 0,
                        descending,
                        nulls_first,
                    }])
                    .unwrap();
                let encoded = encode_values(&expected);
                let actual = execute(&query, 2, &mut view, &cx, policy(100), 100, WORK)
                    .await
                    .unwrap();
                assert_eq!(actual.0, encoded);
                assert_eq!(actual.1.snapshot_records, 0);
                for (offset, count) in [(0, 0), (1, 5), (100, 2)] {
                    let page = query.clone().with_page(offset, Some(count));
                    assert_eq!(
                        execute(&page, 2, &mut view, &cx, policy(count), 100, WORK)
                            .await
                            .unwrap()
                            .0,
                        encoded
                            .iter()
                            .skip(offset as usize)
                            .take(count as usize)
                            .cloned()
                            .collect::<Vec<_>>()
                    );
                }
                assert_eq!(pool.used(), baseline);
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn relational_numeric_ties_obey_later_keys_and_child_pages_before_outer_reordering() {
    let ((), report) = run_async_under_lab(0xc01d_3152, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let vfs = fixture(&contexts.commit()).await;
        let pool = MemoryPool::new(32_000_000, 0).unwrap();
        let mut view = view(&contexts.commit(), &vfs, &pool).await;
        let input = [
            [int(7), int(20)],
            [float(7.0), int(10)],
            [decimal(7), int(15)],
            [int(6), int(99)],
            [float(7.0), int(20)],
            [int(7), int(20)],
        ];
        let expected = [
            [int(6), int(99)],
            [float(7.0), int(10)],
            [decimal(7), int(15)],
            [int(7), int(20)],
            [int(7), int(20)],
            [float(7.0), int(20)],
        ];
        let query = pair_input(&input)
            .with_order_by(&[GraphValueOrder::ascending(0), GraphValueOrder::ascending(1)])
            .unwrap();
        assert_eq!(
            execute(&query, 2, &mut view, &cx, policy(6), 100, WORK)
                .await
                .unwrap()
                .0,
            encode_rows(expected.iter().map(|row| row.to_vec()))
        );
        let outer = query
            .with_page(1, Some(3))
            .nested()
            .unwrap()
            .with_order_by(&[GraphValueOrder::descending(1)])
            .unwrap();
        assert_eq!(
            execute(&outer, 2, &mut view, &cx, policy(3), 100, WORK)
                .await
                .unwrap()
                .0,
            encode_rows([
                vec![int(7), int(20)],
                vec![decimal(7), int(15)],
                vec![float(7.0), int(10)],
            ])
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn relational_distinct_keeps_numeric_representations_and_implicit_canonical_order() {
    let ((), report) = run_async_under_lab(0xc01d_3153, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let vfs = fixture(&contexts.commit()).await;
        let pool = MemoryPool::new(32_000_000, 0).unwrap();
        let mut view = view(&contexts.commit(), &vfs, &pool).await;
        let distinct = values(&[int(9), float(7.0), int(7), null(), float(1.0), int(7)])
            .project(
                vec![GraphSetProjection::new("value", GraphSetValue::Column(0))],
                GraphSetQuantifier::Distinct,
            )
            .unwrap();
        assert_eq!(
            execute(&distinct, 2, &mut view, &cx, policy(5), 100, WORK)
                .await
                .unwrap()
                .0,
            encode_values(&[null(), int(7), int(9), float(1.0), float(7.0)])
        );
        let ordered = distinct
            .with_order_by(&[GraphValueOrder::ascending(0)])
            .unwrap();
        assert_eq!(
            execute(&ordered, 2, &mut view, &cx, policy(5), 100, WORK)
                .await
                .unwrap()
                .0,
            encode_values(&[float(1.0), int(7), float(7.0), int(9), null()])
        );
        let page = ordered.with_page(1, Some(2));
        assert_eq!(
            execute(&page, 2, &mut view, &cx, policy(2), 100, WORK)
                .await
                .unwrap()
                .0,
            encode_values(&[int(7), float(7.0)])
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn relational_composite_order_uses_map_size_and_one_identity_list_family() {
    let ((), report) = run_async_under_lab(0xc01d_3154, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let vfs = fixture(&contexts.commit()).await;
        let pool = MemoryPool::new(32_000_000, 0).unwrap();
        let mut view = view(&contexts.commit(), &vfs, &pool).await;
        let text = GraphValue::Scalar(CanonicalScalar::ucs_basic_text("a").unwrap());
        let expected = vec![
            GraphValue::map(vec![]).unwrap(),
            GraphValue::map(vec![("z".into(), int(1))]).unwrap(),
            GraphValue::map(vec![("a".into(), int(0)), ("b".into(), int(0))]).unwrap(),
            GraphValue::Vertex(VId(2)),
            GraphValue::Edge(EId(3)),
            list(vec![]),
            GraphValue::Vertices(vec![VId(1)].into_boxed_slice()),
            list(vec![GraphValue::Vertex(VId(1))]),
            list(vec![text.clone()]),
            list(vec![int(1)]),
            list(vec![float(1.0)]),
            list(vec![null()]),
            GraphValue::Path(fgdb_gql::algebra::GraphPath::new(VId(0), Box::new([]))),
            text,
            GraphValue::Scalar(CanonicalScalar::Bool(true)),
            int(2),
            null(),
        ];
        let input: Vec<_> = expected.iter().rev().cloned().collect();
        let query = values(&input)
            .with_order_by(&[GraphValueOrder::ascending(0)])
            .unwrap();
        assert_eq!(
            execute(&query, 2, &mut view, &cx, policy(100), 100, WORK)
                .await
                .unwrap()
                .0,
            encode_values(&expected)
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn relational_order_replays_exact_budgets_and_refunds_refusals() {
    let ((), report) = run_async_under_lab(0xc01d_3155, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let vfs = fixture(&contexts.commit()).await;
        let pool = MemoryPool::new(32_000_000, 0).unwrap();
        let mut view = view(&contexts.commit(), &vfs, &pool).await;
        let baseline = pool.used();
        let query = values(&[int(8), float(0.5), int(1), float(1.0), decimal(1), null()])
            .with_order_by(&[GraphValueOrder::ascending(0)])
            .unwrap();
        let measured = checked_execution(&query, &mut view, &cx, policy(100), WORK)
            .await
            .unwrap();
        assert_eq!(
            measured.0,
            encode_values(&[float(0.5), int(1), decimal(1), float(1.0), int(8), null()])
        );
        let exact = GqlQueryPolicy::new(
            measured.1.snapshot_records,
            measured.1.result_rows,
            measured.2.work_units,
            measured.2.scratch_entries,
        );
        assert_eq!(
            checked_execution(&query, &mut view, &cx, exact, measured.3)
                .await
                .unwrap()
                .0,
            measured.0
        );
        for dimension in 0..3 {
            let mut allowance = exact;
            let mut work = measured.3;
            match dimension {
                0 => allowance.evaluator.max_work_units -= 1,
                1 => allowance.evaluator.max_scratch_entries -= 1,
                _ => work -= 1,
            }
            let error = checked_execution(&query, &mut view, &cx, allowance, work)
                .await
                .expect_err("an ordering refusal must not publish a page or retry typed order");
            if dimension == 2 {
                assert!(
                    matches!(error, NativeSpoolError::SortWorkLimit { limit, .. } if limit == work)
                );
            } else {
                assert!(matches!(error, NativeSpoolError::BufferedExecute(error)
                    if matches!(*error, GqlQueryError::Evaluator(_))));
            }
            assert_eq!(pool.used(), baseline);
        }
        assert_eq!(
            checked_execution(&query, &mut view, &cx, exact, measured.3)
                .await
                .unwrap()
                .0,
            measured.0
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn relational_graph_history_orders_computed_mixed_numbers_at_the_requested_cut() {
    let ((), report) =
        run_async_under_lab(0xc01d_3156, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.query();
            let vfs = fixture(&contexts.commit()).await;
            let pool = MemoryPool::new(32_000_000, 0).unwrap();
            let mut view = view(&contexts.commit(), &vfs, &pool).await;
            let query = PreparedGraphSetText::prepare(
            "MATCH (a)-[r:R]->(b) WITH r.p AS v UNWIND [v, v / 2.0] AS key RETURN key ORDER BY key",
            resolve,
        ).unwrap().bind_parameters(&GqlParameters::new()).unwrap();
            for (cut, expected) in [
                (
                    1,
                    vec![
                        float(3.5),
                        float(4.0),
                        float(4.5),
                        float(5.0),
                        int(7),
                        int(8),
                        int(9),
                        int(10),
                    ],
                ),
                (
                    2,
                    vec![
                        float(4.0),
                        float(4.5),
                        float(5.0),
                        int(8),
                        float(8.5),
                        int(9),
                        int(10),
                        int(17),
                    ],
                ),
            ] {
                let measured = execute(&query, cut, &mut view, &cx, policy(8), 100, WORK)
                    .await
                    .unwrap();
                assert_eq!(measured.0, encode_values(&expected));
                assert!(measured.1.snapshot_records > 0);
            }
        });
    assert!(report.lab_test_passed(), "{report:?}");
}

// Direct scan tails reach ordered_input, not the relational-stage sorter.
// Use real committed properties so no computed projection can silently route
// these regressions back through the already covered relational-stage path.
mod scan_tails {
    use super::*;
    use crate::{Database, EmbeddedReadView, MemVfs, WriteBatch};
    use fgdb_delta_types::{PropertyKeyId, RelationId};
    use fgdb_gql::{GraphSymbol, GraphSymbolKind};
    use fgdb_types::{CommitCx, EId, VId};

    fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        match (kind, name) {
            (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(RelationId(1))),
            (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
            (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(PropertyKeyId(2))),
            _ => None,
        }
    }

    async fn numeric_graph(cx: &CommitCx) -> (Database<MemVfs>, MemVfs) {
        let vfs = MemVfs::new().unwrap();
        let mut db = Database::create_with_vfs(cx, vfs.clone(), vfs.database_dir(), keys())
            .await
            .unwrap();
        let data = [
            (int(7), 20), (float(7.0), 10), (decimal(7), 15), (int(6), 99),
            (float(7.0), 20), (int(7), 20), (float(-2.0), 40), (null(), 50),
            (int(9_007_199_254_740_993), 90), (float(9_007_199_254_740_992.0), 80),
            (GraphValue::Scalar(CanonicalScalar::Bool(false)), 70),
            (GraphValue::Scalar(CanonicalScalar::ucs_basic_text("a").unwrap()), 60),
        ];
        let mut batch = WriteBatch::new(RelationId(1));
        for (at, (key, tie)) in data.iter().enumerate() {
            batch.create_vertex(
                VId(at as u128 + 1),
                vec![],
                vec![
                    (PropertyKeyId(1), key.as_scalar().unwrap().clone()),
                    (PropertyKeyId(2), CanonicalScalar::Int(*tie)),
                ],
            );
        }
        for (at, (key, tie)) in data.iter().enumerate() {
            batch.add_edge(
                EId(at as u128 + 1), VId(1), VId(1),
                vec![
                    (PropertyKeyId(1), key.as_scalar().unwrap().clone()),
                    (PropertyKeyId(2), CanonicalScalar::Int(*tie)),
                ],
            );
        }
        assert_eq!(db.write(cx, batch).await.unwrap(), CommitSeq(1));
        (db, vfs)
    }

    struct ScanResult {
        rows: Vec<Vec<u8>>,
        stats: GqlExecutionStats,
        evaluator: GlaExecutionStats,
        work: u64,
        snapshot: CommitSeq,
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_scan(
        text: &str,
        embedded: &EmbeddedReadView,
        buffered: &mut BufferedReadView<MemVfs>,
        cx: &QueryCx,
        cold: bool,
        allowance: GqlQueryPolicy,
        inputs: u64,
        work: u64,
    ) -> Result<ScanResult> {
        let params = GqlParameters::new();
        let prepared = PreparedNativeRead::prepare(text, &params, symbols).unwrap();
        assert!(matches!(
            &prepared,
            PreparedNativeRead::Pattern(_) | PreparedNativeRead::TemporalPattern(_)
        ), "this regression must execute the direct scan tail");
        let pool = MemoryPool::new(2_000_000, 0).unwrap();
        let (mut scratch, _) = file(cx, &pool).await;
        let (mut destination, _) = file(cx, &pool).await;
        let (mut auxiliary, _) = file(cx, &pool).await;
        let result = if cold {
            prepared.prepare_buffered_order(&params).unwrap()
                .spool_in_view(
                    buffered, cx, allowance, &mut scratch, &mut destination, &mut auxiliary,
                    2, 64, 2, 128, 127, 8192, inputs, work,
                )
                .await
        } else {
            prepared.spool_ordered_in_view(
                embedded, cx, &params, allowance, &mut scratch, &mut destination,
                2, 128, 127, 8192, inputs, work,
            )
            .await
        };
        let result = match result {
            Ok((spool, work)) => Ok(ScanResult {
                rows: contents(&spool, &mut destination, cx).await,
                stats: spool.row_stats(),
                evaluator: spool.evaluator_stats(),
                work,
                snapshot: spool.snapshot_seq(),
            }),
            Err(error) => Err(error),
        };
        assert_eq!(pool.used(), 0, "direct scan success/refusal must refund scratch ownership");
        result
    }

    const SOURCES: [(&str, &str); 2] = [
        ("MATCH (n)", "n"),
        ("MATCH (a)-[r:R]->(b)", "r"),
    ];

    fn ordered_groups() -> Vec<Vec<Vec<GraphValue>>> {
        vec![
            vec![vec![GraphValue::Scalar(CanonicalScalar::ucs_basic_text("a").unwrap()), int(60)]],
            vec![vec![GraphValue::Scalar(CanonicalScalar::Bool(false)), int(70)]],
            vec![vec![float(-2.0), int(40)]],
            vec![vec![int(6), int(99)]],
            vec![
                vec![float(7.0), int(10)], vec![decimal(7), int(15)],
                vec![int(7), int(20)], vec![int(7), int(20)], vec![float(7.0), int(20)],
            ],
            vec![vec![float(9_007_199_254_740_992.0), int(80)]],
            vec![vec![int(9_007_199_254_740_993), int(90)]],
        ]
    }

    #[test]
    fn direct_vertex_and_edge_tails_use_language_order_before_later_keys() {
        let ((), report) = run_async_under_lab(0xc01d_3161, |root| async move {
            let c = PurposeContexts::narrow_runtime_root(&root);
            let cx = c.query();
            let (db, vfs) = numeric_graph(&c.commit()).await;
            let embedded = db.read_session().unwrap();
            let pool = MemoryPool::new(32_000_000, 0).unwrap();
            let mut buffered = view(&c.commit(), &vfs, &pool).await;
            let baseline = pool.used();
            for cold in [false, true] {
                for (source, variable) in SOURCES {
                    for descending in [false, true] {
                        for nulls_first in [false, true] {
                            let mut groups = ordered_groups();
                            if descending { groups.reverse(); }
                            let mut expected: Vec<_> = groups.into_iter().flatten().collect();
                            if nulls_first {
                                expected.insert(0, vec![null(), int(50)]);
                            } else {
                                expected.push(vec![null(), int(50)]);
                            }
                            let direction = if descending { "DESC" } else { "ASC" };
                            let nulls = if nulls_first { "FIRST" } else { "LAST" };
                            let text = format!(
                                "{source} RETURN {variable}.p AS key,{variable}.q AS tie ORDER BY key {direction} NULLS {nulls},tie"
                            );
                            let result = run_scan(
                                &text, &embedded, &mut buffered, &cx, cold, policy(12), 12, WORK,
                            ).await.unwrap();
                            assert_eq!(result.rows, encode_rows(expected), "{text} cold={cold}");
                            assert_eq!(result.snapshot, CommitSeq(1));
                            assert_eq!(result.stats.result_rows, 12);
                            assert!(result.stats.snapshot_records > 0);
                            assert_eq!(pool.used(), baseline);
                        }
                    }
                }
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn direct_hidden_numeric_keys_choose_the_page_before_visible_prefix_removal() {
        let ((), report) = run_async_under_lab(0xc01d_3162, |root| async move {
            let c = PurposeContexts::narrow_runtime_root(&root);
            let cx = c.query();
            let (db, vfs) = numeric_graph(&c.commit()).await;
            let embedded = db.read_session().unwrap();
            let pool = MemoryPool::new(32_000_000, 0).unwrap();
            let mut buffered = view(&c.commit(), &vfs, &pool).await;
            for cold in [false, true] {
                for (source, variable) in SOURCES {
                    for (direction, ties) in [
                        ("ASC", [70, 40, 99, 10]),
                        ("DESC", [80, 10, 15, 20]),
                    ] {
                        let text = format!(
                            "{source} RETURN {variable}.q AS tie ORDER BY {variable}.p {direction},tie SKIP 1 LIMIT 4"
                        );
                        let result = run_scan(
                            &text, &embedded, &mut buffered, &cx, cold, policy(4), 12, WORK,
                        ).await.unwrap();
                        assert_eq!(result.rows, encode_values(&ties.map(int)));
                        assert_eq!(result.stats.result_rows, 4);
                    }
                }
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn direct_distinct_retains_typed_numeric_classes_and_implicit_canonical_order() {
        let ((), report) = run_async_under_lab(0xc01d_3163, |root| async move {
            let c = PurposeContexts::narrow_runtime_root(&root);
            let cx = c.query();
            let (db, vfs) = numeric_graph(&c.commit()).await;
            let embedded = db.read_session().unwrap();
            let pool = MemoryPool::new(32_000_000, 0).unwrap();
            let mut buffered = view(&c.commit(), &vfs, &pool).await;
            let text_value = GraphValue::Scalar(CanonicalScalar::ucs_basic_text("a").unwrap());
            let boolean = GraphValue::Scalar(CanonicalScalar::Bool(false));
            let canonical = vec![
                null(), boolean.clone(), int(6), int(7), int(9_007_199_254_740_993),
                decimal(7), float(-2.0), float(7.0), float(9_007_199_254_740_992.0),
                text_value.clone(),
            ];
            let language = vec![
                null(), text_value, boolean, float(-2.0), int(6), int(7), decimal(7),
                float(7.0), float(9_007_199_254_740_992.0), int(9_007_199_254_740_993),
            ];
            for cold in [false, true] {
                for (source, variable) in SOURCES {
                    for (tail, expected) in [
                        ("", &canonical),
                        (" ORDER BY key ASC NULLS FIRST", &language),
                    ] {
                        let text = format!("{source} RETURN DISTINCT {variable}.p AS key{tail}");
                        let result = run_scan(
                            &text, &embedded, &mut buffered, &cx, cold, policy(10), 12, WORK,
                        ).await.unwrap();
                        assert_eq!(result.rows, encode_values(expected));
                        assert_eq!(result.stats.result_rows, 10);
                    }
                }
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn direct_numeric_scan_order_uses_the_requested_historical_cut_and_original_pin() {
        let ((), report) = run_async_under_lab(0xc01d_3164, |root| async move {
            let c = PurposeContexts::narrow_runtime_root(&root);
            let cx = c.query();
            let (mut db, vfs) = numeric_graph(&c.commit()).await;
            let pinned = db.read_session().unwrap();
            let mut update = WriteBatch::new(RelationId(1));
            update.set_vertex_property(VId(2), PropertyKeyId(1), Some(CanonicalScalar::Int(5)));
            update.set_edge_property(EId(2), PropertyKeyId(1), Some(CanonicalScalar::Int(5)));
            assert_eq!(db.write(&c.commit(), update).await.unwrap(), CommitSeq(2));
            let latest = db.read_session().unwrap();
            let pool = MemoryPool::new(32_000_000, 0).unwrap();
            let mut buffered = view(&c.commit(), &vfs, &pool).await;
            let old = [60, 70, 40, 99, 10, 15, 20, 20, 20, 80, 90, 50];
            let new = [60, 70, 40, 10, 99, 15, 20, 20, 20, 80, 90, 50];
            for (source, variable) in SOURCES {
                let tail = format!("RETURN {variable}.q AS tie ORDER BY {variable}.p,tie");
                let held = run_scan(
                    &format!("{source} {tail}"), &pinned, &mut buffered, &cx,
                    false, policy(12), 12, WORK,
                ).await.unwrap();
                assert_eq!(held.snapshot, CommitSeq(1));
                assert_eq!(held.rows, encode_values(&old.map(int)));
                for cold in [false, true] {
                    for (cut, expected) in [(1, old), (2, new)] {
                        let text = format!("{source} FOR SYSTEM_TIME AS OF SEQ {cut} {tail}");
                        let result = run_scan(
                            &text, &latest, &mut buffered, &cx, cold, policy(12), 12, WORK,
                        ).await.unwrap();
                        assert_eq!(result.snapshot, CommitSeq(cut));
                        assert_eq!(result.rows, encode_values(&expected.map(int)));
                    }
                }
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn direct_language_scan_budgets_remain_cumulative_and_zero_pages_do_not_hide_input_caps() {
        let ((), report) = run_async_under_lab(0xc01d_3165, |root| async move {
            let c = PurposeContexts::narrow_runtime_root(&root);
            let cx = c.query();
            let (db, vfs) = numeric_graph(&c.commit()).await;
            let embedded = db.read_session().unwrap();
            let pool = MemoryPool::new(32_000_000, 0).unwrap();
            let mut buffered = view(&c.commit(), &vfs, &pool).await;
            let baseline = pool.used();
            for cold in [false, true] {
                let text = "MATCH (n) RETURN n.q AS tie ORDER BY n.p,tie SKIP 1 LIMIT 4";
                let measured = run_scan(
                    text, &embedded, &mut buffered, &cx, cold, policy(4), 12, WORK,
                ).await.unwrap();
                assert_eq!(measured.rows, encode_values(&[int(70), int(40), int(99), int(10)]));
                let exact = GqlQueryPolicy::new(
                    measured.stats.snapshot_records, measured.stats.result_rows,
                    measured.evaluator.work_units, measured.evaluator.scratch_entries,
                );
                let replay = run_scan(
                    text, &embedded, &mut buffered, &cx, cold, exact, 12, measured.work,
                ).await.unwrap();
                assert_eq!(replay.rows, measured.rows);
                assert_eq!(replay.work, measured.work);
                for dimension in 0..3 {
                    let mut allowance = exact;
                    let mut work = measured.work;
                    match dimension {
                        0 => allowance.evaluator.max_work_units -= 1,
                        1 => allowance.evaluator.max_scratch_entries -= 1,
                        _ => work -= 1,
                    }
                    let result = run_scan(
                        text, &embedded, &mut buffered, &cx, cold, allowance, 12, work,
                    ).await;
                    assert!(result.is_err(), "refusal cannot retry canonical order");
                    if dimension == 2 {
                        assert!(matches!(result, Err(NativeSpoolError::SortWorkLimit { limit, .. }) if limit == work));
                    }
                    assert_eq!(pool.used(), baseline);
                }
                let empty = "MATCH (n) RETURN n.q AS tie ORDER BY n.p,tie LIMIT 0";
                assert!(run_scan(
                    empty, &embedded, &mut buffered, &cx, cold, policy(0), 11, WORK,
                ).await.is_err());
                assert!(run_scan(
                    empty, &embedded, &mut buffered, &cx, cold, policy(0), 12, WORK,
                ).await.unwrap().rows.is_empty());
                assert_eq!(pool.used(), baseline);
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }
}
