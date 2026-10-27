use super::*;
use crate::{
    GraphIntegerBinary, GraphIntegerExpression, GraphIntegerOp, GraphSetOperand,
    GraphSetPredicateOp, GraphSetProjection, GraphSetValue,
};
use fgdb_types::CanonicalScalar;

mod completed_procedures {
    use super::*;
    use crate::{GraphProcedureExecution, GraphProcedureRowCursor};
    use fgdb_types::VId;
    use std::cell::Cell;
    use std::rc::Rc;

    #[derive(Clone, Copy)]
    enum Fault {
        Schema,
        Order,
        Short,
        Long,
        Source,
    }

    struct Rows {
        at: usize,
        count: usize,
        fault: Option<Fault>,
        pulled: Rc<Cell<usize>>,
    }

    fn row(at: usize) -> GraphValueRow {
        GraphValueRow::from_owned_values(vec![
            GraphValue::Vertex(VId((1_u128 << 100) + at as u128)),
            scalar(Some((at * 17 % 101) as i64)),
        ])
    }

    impl GraphProcedureRowCursor<&'static str, usize> for Rows {
        fn next_row(
            &mut self,
            checkpoint: &mut dyn FnMut() -> Result<(), usize>,
        ) -> Result<Option<GraphValueRow>, GqlQueryError<&'static str, usize>> {
            checkpoint().map_err(GqlQueryError::Interrupted)?;
            if self.at == self.count {
                return Ok(None);
            }
            let at = self.at;
            self.at += 1;
            self.pulled.set(self.pulled.get() + 1);
            if at + 1 == self.count {
                match self.fault {
                    Some(Fault::Schema) => return Ok(Some(GraphValueRow::unit())),
                    Some(Fault::Order) => return Ok(Some(row(0))),
                    Some(Fault::Source) => return Err(GqlQueryError::Source("late source")),
                    _ => {}
                }
            }
            Ok(Some(row(at)))
        }
    }

    struct Host {
        count: usize,
        cursor: bool,
        cursor_only: bool,
        fault: Option<Fault>,
        pulled: Rc<Cell<usize>>,
        opened: Rc<Cell<usize>>,
    }

    impl Host {
        fn new(count: usize, cursor: bool) -> Self {
            Self {
                count,
                cursor,
                cursor_only: false,
                fault: None,
                pulled: Rc::new(Cell::new(0)),
                opened: Rc::new(Cell::new(0)),
            }
        }
    }

    impl GraphSetSource<&'static str, usize> for Host {
        fn pattern(
            &mut self,
            _: &PreparedGraphPattern<GraphValueRow>,
            _: GqlQueryPolicy,
        ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<&'static str, usize>> {
            panic!("a direct analytics relation has no graph leaf")
        }
        fn supports_procedure_rows(&self) -> bool {
            self.cursor
        }
        fn procedure(
            &mut self,
            _: &PreparedProcedureCall,
            _: &[GraphValue],
            _: GqlQueryPolicy,
        ) -> Option<Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<&'static str, usize>>>
        {
            assert!(!self.cursor_only, "the parsed query must use its cursor");
            self.opened.set(self.opened.get() + 1);
            Some(Ok(GqlQueryExecution {
                value: (0..self.count).map(row).collect(),
                rows: GqlExecutionStats {
                    snapshot_records: 11,
                    result_rows: self.count as u64,
                },
                evaluator: GlaExecutionStats {
                    work_units: 13,
                    scratch_entries: self.count as u64,
                },
            }))
        }
        fn procedure_rows(
            &mut self,
            _: &PreparedProcedureCall,
            _: &[GraphValue],
            _: GqlQueryPolicy,
        ) -> Option<
            Result<
                GraphProcedureExecution<&'static str, usize>,
                GqlQueryError<&'static str, usize>,
            >,
        > {
            assert!(self.cursor);
            self.opened.set(self.opened.get() + 1);
            let actual = match self.fault {
                Some(Fault::Short) => self.count - 1,
                Some(Fault::Long) => self.count + 1,
                _ => self.count,
            };
            Some(Ok(GraphProcedureExecution {
                cursor: Box::new(Rows {
                    at: 0,
                    count: actual,
                    fault: self.fault,
                    pulled: Rc::clone(&self.pulled),
                }),
                rows: GqlExecutionStats {
                    snapshot_records: 11,
                    result_rows: self.count as u64,
                },
                evaluator: GlaExecutionStats {
                    work_units: 13,
                    scratch_entries: self.count as u64,
                },
            }))
        }
    }

    fn call() -> PreparedGraphSet {
        PreparedGraphSet::singleton()
            .procedure_call(
                "fnx".into(),
                "pagerank".into(),
                vec![],
                vec![],
                vec![("vertex".into(), "v".into()), ("score".into(), "s".into())],
                vec![],
            )
            .unwrap()
    }

    #[test]
    fn completed_cursor_folds_before_next_row_and_absorbs_source_costs_first() {
        let host = Host::new(300, true);
        let pulled = Rc::clone(&host.pulled);
        let opened = Rc::clone(&host.opened);
        let mut consumed = 0;
        let (stats, _) = call()
            .fold_governed(
                wide(),
                host,
                || Ok(()),
                |value, _| {
                    consumed += 1;
                    assert_eq!(
                        pulled.get(),
                        consumed,
                        "the source never produces the next row early"
                    );
                    assert_eq!(value, row(consumed - 1));
                    Ok(())
                },
            )
            .unwrap();
        assert_eq!(consumed, 300);
        assert_eq!(opened.get(), 1);
        assert_eq!(stats.snapshot_records, 11);
        assert_eq!(stats.result_rows, 0);
        for policy in [
            GqlQueryPolicy::new(10, u64::MAX, u64::MAX, u64::MAX),
            GqlQueryPolicy::new(u64::MAX, u64::MAX, 12, u64::MAX),
            GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, 299),
        ] {
            let host = Host::new(300, true);
            let pulled = Rc::clone(&host.pulled);
            assert!(
                call()
                    .fold_governed(policy, host, || Ok(()), |_, _| panic!("unadmitted row"))
                    .is_err()
            );
            assert_eq!(pulled.get(), 0, "complete source costs precede every pull");
        }
    }

    #[test]
    fn cursor_pages_and_vertex_preserving_projections_equal_materialized_rows() {
        let input = call()
            .project(
                vec![
                    GraphSetProjection::new("v", GraphSetValue::Column(0)),
                    GraphSetProjection::new("s", GraphSetValue::Column(1)),
                ],
                GraphSetQuantifier::All,
            )
            .unwrap();
        assert!(matches!(
            input.clone().with_order_by(&[]),
            Err(crate::algebra::GraphOrderError::EmptyOrder)
        ));
        for order in [vec![], vec![crate::algebra::GraphValueOrder::descending(1)]] {
            let ordered = if order.is_empty() {
                input.clone()
            } else {
                input.clone().with_order_by(&order).unwrap()
            };
            for (skip, count, expected_len) in [(0, 0, 0), (0, 5, 5), (3, 8, 8), (298, 9, 2)] {
                let query = ordered.clone().with_page(skip, Some(count));
                let expected = query
                    .execute_governed_from_source(wide(), Host::new(300, false), || Ok(()))
                    .unwrap();
                let host = Host::new(300, true);
                let pulled = Rc::clone(&host.pulled);
                let actual = query
                    .execute_governed_from_source(wide(), host, || Ok(()))
                    .unwrap();
                assert_eq!(actual.value.len(), expected_len);
                assert_eq!(actual.value, expected.value);
                assert_eq!(actual.rows, expected.rows);
                assert_eq!(
                    pulled.get(),
                    300,
                    "pages still drain their completed source"
                );
            }
        }
        let reordered = call()
            .project(
                vec![
                    GraphSetProjection::new("s", GraphSetValue::Column(1)),
                    GraphSetProjection::new("v", GraphSetValue::Column(0)),
                ],
                GraphSetQuantifier::All,
            )
            .unwrap();
        assert!(
            !reordered.has_foldable_expansion(),
            "dropping the unique leading key retains its sort"
        );
        let host = Host::new(300, true);
        let pulled = Rc::clone(&host.pulled);
        reordered
            .execute_governed_from_source(wide(), host, || Ok(()))
            .unwrap();
        assert_eq!(
            pulled.get(),
            0,
            "unproved ordering uses the materialized source"
        );
    }

    #[test]
    fn completed_cursor_reuses_exact_group_reducers_and_the_cumulative_allowance() {
        let query = crate::PreparedGraphSetAggregate::prepare(
            call(),
            &[1],
            &[
                crate::GraphAggregate::count_rows("n"),
                crate::GraphAggregate::sum_int("total", 1),
                crate::GraphAggregate::average_int("mean", 1),
            ],
            0,
            None,
        )
        .unwrap();
        let expected = query
            .execute_governed_from_source(wide(), Host::new(303, false), || Ok(()))
            .unwrap();
        let host = Host::new(303, true);
        let pulled = Rc::clone(&host.pulled);
        let actual = query
            .execute_governed_from_source(wide(), host, || Ok(()))
            .unwrap();
        assert_eq!(actual.value, expected.value);
        assert_eq!(pulled.get(), 303);
        assert_eq!(actual.value.len(), 101);
        for (score, row) in actual.value.iter().enumerate() {
            assert_eq!(row.keys(), [scalar(Some(score as i64))]);
            assert_eq!(row.values()[0].as_count(), Some(3));
            assert_eq!(row.values()[1].as_integer(), Some(3 * score as i128));
            assert_eq!(
                row.values()[2].as_average(),
                crate::GraphExactAverage::new(score as i128, 1)
            );
        }
        let exact = GqlQueryPolicy::new(
            actual.rows.snapshot_records,
            actual.rows.result_rows,
            actual.evaluator.work_units,
            actual.evaluator.scratch_entries,
        );
        assert_eq!(
            query
                .execute_governed_from_source(exact, Host::new(303, true), || Ok(()))
                .unwrap(),
            actual
        );
        for dimension in 0..4 {
            let mut short = exact;
            match dimension {
                0 => {
                    short = GqlQueryPolicy::new(
                        10,
                        actual.rows.result_rows,
                        actual.evaluator.work_units,
                        actual.evaluator.scratch_entries,
                    )
                }
                1 => {
                    short = GqlQueryPolicy::new(
                        11,
                        actual.rows.result_rows - 1,
                        actual.evaluator.work_units,
                        actual.evaluator.scratch_entries,
                    )
                }
                2 => short.evaluator.max_work_units -= 1,
                _ => short.evaluator.max_scratch_entries -= 1,
            }
            assert!(
                query
                    .execute_governed_from_source(short, Host::new(303, true), || Ok(()))
                    .is_err()
            );
        }
    }

    #[test]
    fn parsed_component_and_computed_key_groups_activate_the_completed_cursor() {
        use crate::{
            GqlParameters, GraphAggregateTextSlot, GraphAggregateValue,
            PreparedGraphPipelineAggregateText,
        };
        for (text, groups) in [
            (
                "CALL fnx.weakly_connected_components() YIELD vertex, component \
                 RETURN component, COUNT(*) AS n GROUP BY component ORDER BY component",
                101,
            ),
            (
                "CALL fnx.single_source_shortest_path_length(1) YIELD vertex, distance \
                 WITH vertex, distance, distance % 2 AS parity \
                 RETURN parity, COUNT(*) AS n, SUM(distance) AS total \
                 GROUP BY parity ORDER BY parity",
                2,
            ),
        ] {
            let prepared =
                PreparedGraphPipelineAggregateText::prepare(text, |_, _: &str| None).unwrap();
            let slots = prepared.output_slots().to_vec();
            let query = prepared
                .bind_relation_parameters(&GqlParameters::new())
                .unwrap();
            assert!(query.input().has_foldable_expansion_for_source(true));
            assert!(!query.input().has_foldable_expansion_for_source(false));
            let expected = query
                .execute_governed_from_source(wide(), Host::new(303, false), || Ok(()))
                .unwrap();
            let mut host = Host::new(303, true);
            host.cursor_only = true;
            let pulled = Rc::clone(&host.pulled);
            let opened = Rc::clone(&host.opened);
            let actual = query
                .execute_governed_from_source(wide(), host, || Ok(()))
                .unwrap();
            assert_eq!(actual.value, expected.value);
            assert_eq!(actual.value.len(), groups);
            assert_eq!(pulled.get(), 303);
            assert_eq!(opened.get(), 1);
            let visible: Vec<Vec<_>> = actual
                .value
                .iter()
                .map(|row| {
                    slots
                        .iter()
                        .map(|slot| match *slot {
                            GraphAggregateTextSlot::GroupKey(at) => {
                                GraphAggregateValue::Value(row.keys()[at].clone())
                            }
                            GraphAggregateTextSlot::Aggregate(at) => row.values()[at].clone(),
                        })
                        .collect()
                })
                .collect();
            if groups == 101 {
                for (component, row) in visible.iter().enumerate() {
                    assert_eq!(
                        row,
                        &[
                            GraphAggregateValue::Value(scalar(Some(component as i64))),
                            GraphAggregateValue::Count(3),
                        ]
                    );
                }
            } else {
                assert_eq!(
                    visible,
                    vec![
                        vec![
                            GraphAggregateValue::Value(scalar(Some(0))),
                            GraphAggregateValue::Count(153),
                            GraphAggregateValue::Integer(7650),
                        ],
                        vec![
                            GraphAggregateValue::Value(scalar(Some(1))),
                            GraphAggregateValue::Count(150),
                            GraphAggregateValue::Integer(7500),
                        ],
                    ]
                );
            }
        }
    }

    #[test]
    fn completed_cursor_releases_its_host_for_element_reads_and_drains_zero_pages() {
        use crate::{GraphElementKey, WithElementReads};
        use fgdb_delta_types::{ElementId, PropertyKeyId};
        let query = call()
            .project(
                vec![
                    GraphSetProjection::new("v", GraphSetValue::Column(0)),
                    GraphSetProjection::new(
                        "p",
                        GraphSetValue::MapGet {
                            map: Box::new(GraphSetValue::Column(0)),
                            key: "p".into(),
                            property: GraphElementKey::Property(PropertyKeyId(3)),
                        },
                    ),
                ],
                GraphSetQuantifier::All,
            )
            .unwrap();
        for count in [0, 2] {
            let reads = Cell::new(0);
            let host = WithElementReads {
                source: Host::new(8, true),
                elements: |element, key| {
                    assert_eq!(
                        element,
                        ElementId::Vertex(VId((1_u128 << 100) + reads.get()))
                    );
                    assert_eq!(key, PropertyKeyId(3));
                    reads.set(reads.get() + 1);
                    if reads.get() == 8 {
                        Err("last element refused")
                    } else {
                        Ok(Some(CanonicalScalar::Int(9).into()))
                    }
                },
            };
            let result = query
                .clone()
                .with_page(0, Some(count))
                .execute_governed_from_source(wide(), host, || Ok(()));
            assert_eq!(reads.get(), 8);
            assert!(matches!(
                result,
                Err(GqlQueryError::Source(GraphSetExecutionError::Source(
                    "last element refused"
                )))
            ));
        }
    }

    #[test]
    fn late_cursor_contract_failure_wins_over_downstream_error_even_with_zero_page() {
        for fault in [
            Fault::Schema,
            Fault::Order,
            Fault::Short,
            Fault::Long,
            Fault::Source,
        ] {
            for limit in [None, Some(0)] {
                let mut host = Host::new(8, true);
                host.fault = Some(fault);
                let result = call().with_page(0, limit).fold_governed(
                    wide(),
                    host,
                    || Ok(()),
                    |_, _| {
                        Err(GqlQueryError::Source(GraphSetExecutionError::Source(
                            "sink failure",
                        )))
                    },
                );
                match fault {
                    Fault::Schema => assert!(matches!(
                        result,
                        Err(GqlQueryError::Source(GraphSetExecutionError::InputSchema {
                            operand: 0
                        }))
                    )),
                    Fault::Source => assert!(matches!(
                        result,
                        Err(GqlQueryError::Source(GraphSetExecutionError::Source(
                            "late source"
                        )))
                    )),
                    _ => assert!(matches!(
                        result,
                        Err(GqlQueryError::Source(
                            GraphSetExecutionError::InvalidSourceStatistics { operand: 0 }
                        ))
                    )),
                }
            }
        }
        let result = call().fold_governed(
            wide(),
            Host::new(8, true),
            || Ok(()),
            |_, _| {
                Err(GqlQueryError::Source(GraphSetExecutionError::Source(
                    "sink failure",
                )))
            },
        );
        assert!(matches!(
            result,
            Err(GqlQueryError::Source(GraphSetExecutionError::Source(
                "sink failure"
            )))
        ));
    }

    #[test]
    fn every_completed_cursor_checkpoint_refuses_without_reopening_source() {
        let query = call().with_page(2, Some(3));
        let mut calls = 0;
        query
            .execute_governed_from_source(wide(), Host::new(20, true), || {
                calls += 1;
                Ok(())
            })
            .unwrap();
        for stop in 1..=calls {
            let mut calls = 0;
            let host = Host::new(20, true);
            let opened = Rc::clone(&host.opened);
            let result = query.execute_governed_from_source(wide(), host, || {
                calls += 1;
                if calls == stop { Err(stop) } else { Ok(()) }
            });
            assert!(matches!(result, Err(GqlQueryError::Interrupted(at)) if at == stop));
            assert!(opened.get() <= 1);
        }
    }
}

fn wide() -> GqlQueryPolicy {
    GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX)
}
fn scalar(value: Option<i64>) -> GraphValue {
    GraphValue::Scalar(value.map_or(CanonicalScalar::Null, CanonicalScalar::Int))
}
fn list(values: &[Option<i64>]) -> GraphSetValue {
    GraphSetValue::List(
        values
            .iter()
            .map(|value| GraphSetValue::Value(scalar(*value)))
            .collect(),
    )
}
fn no_source(
    _: &PreparedGraphPattern<GraphValueRow>,
    _: GqlQueryPolicy,
) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<&'static str, usize>> {
    panic!("constant relations do not observe a graph")
}
fn eager(
    query: &PreparedGraphSet,
) -> SetResult<GqlQueryExecution<GraphValueRow>, &'static str, usize> {
    query.execute_governed(wide(), no_source, || Ok(()))
}
fn folded(query: &PreparedGraphSet) -> SetResult<Vec<GraphValueRow>, &'static str, usize> {
    let mut rows = Vec::new();
    let (stats, _) = query.fold_governed(
        wide(),
        no_source,
        || Ok(()),
        |row, _| {
            rows.push(row);
            Ok(())
        },
    )?;
    assert_eq!(stats.snapshot_records, 0);
    assert_eq!(stats.result_rows, 0); // These are private intermediate rows.
    Ok(rows)
}

#[test]
fn folded_scopes_filters_projections_and_pages_preserve_occurrence_order() {
    for values in [
        vec![],
        vec![None],
        vec![Some(1), Some(1), Some(-2)],
        vec![Some(3), None, Some(-1)],
    ] {
        for inner_skip in [0, 1, u64::MAX] {
            for inner_count in [0, 1, 4] {
                for skip in [0, 1, 5] {
                    for count in [0, 1, 8] {
                        let query = PreparedGraphSet::singleton()
                            .unwind("x".into(), list(&values))
                            .unwrap()
                            .with_page(inner_skip, Some(inner_count))
                            .unwind("y".into(), list(&[Some(2), Some(-1), Some(2)]))
                            .unwrap()
                            .project(
                                vec![
                                    GraphSetProjection::new("y", GraphSetValue::Column(1)),
                                    GraphSetProjection::new("x", GraphSetValue::Column(0)),
                                ],
                                GraphSetQuantifier::All,
                            )
                            .unwrap()
                            .filter(&[GraphSetPredicateOp::IsNull {
                                operand: GraphSetOperand::Column(1),
                                is_null: false,
                            }])
                            .unwrap()
                            .nested()
                            .unwrap()
                            .with_page(skip, Some(count));
                        let before = query.canonical_bytes();
                        let mut expected = Vec::new();
                        for &x in values
                            .iter()
                            .skip(usize::try_from(inner_skip).unwrap_or(usize::MAX))
                            .take(inner_count as usize)
                        {
                            for y in [2, -1, 2] {
                                if x.is_some() {
                                    expected.push(GraphValueRow::from_owned_values(vec![
                                        scalar(Some(y)),
                                        scalar(x),
                                    ]));
                                }
                            }
                        }
                        let expected: Vec<_> = expected
                            .into_iter()
                            .skip(skip as usize)
                            .take(count as usize)
                            .collect();
                        assert!(query.has_foldable_expansion());
                        assert_eq!(folded(&query).unwrap(), expected);
                        assert_eq!(eager(&query).unwrap().value, expected);
                        assert_eq!(query.canonical_bytes(), before);
                    }
                }
            }
        }
    }
}

#[test]
fn cross_join_keeps_left_major_order_and_children_keep_their_own_windows() {
    let left = PreparedGraphSet::singleton()
        .unwind("a".into(), list(&[Some(3), Some(1), Some(3)]))
        .unwrap();
    let right = PreparedGraphSet::singleton()
        .unwind("b".into(), list(&[None, Some(4), Some(4)]))
        .unwrap();
    for left_count in [0, 1, 3] {
        for skip in 0..5 {
            let query = left
                .clone()
                .with_page(0, Some(left_count))
                .cross_join(right.clone().with_page(1, Some(2)))
                .unwrap()
                .with_page(skip, Some(3));
            let expected: Vec<_> = [3, 1, 3]
                .into_iter()
                .take(left_count as usize)
                .flat_map(|a| {
                    [4, 4].into_iter().map(move |b| {
                        GraphValueRow::from_owned_values(vec![scalar(Some(a)), scalar(Some(b))])
                    })
                })
                .skip(skip as usize)
                .take(3)
                .collect();
            assert_eq!(folded(&query).unwrap(), expected);
            assert_eq!(eager(&query).unwrap().value, expected);
        }
    }
}

fn invalid_child() -> PreparedGraphSet {
    let values = GraphSetValue::List(vec![
        GraphSetValue::List(vec![GraphSetValue::Value(scalar(Some(1)))]),
        GraphSetValue::Value(GraphValue::Scalar(
            CanonicalScalar::ucs_basic_text("private invalid list").unwrap(),
        )),
    ]);
    PreparedGraphSet::singleton()
        .unwind("x".into(), values)
        .unwrap()
        .unwind("y".into(), GraphSetValue::Column(0))
        .unwrap()
}
fn divide_zero() -> GraphSetValue {
    GraphSetValue::Integer(
        GraphIntegerExpression::prepare(&[
            GraphIntegerOp::Literal(Some(1)),
            GraphIntegerOp::Literal(Some(0)),
            GraphIntegerOp::Binary(GraphIntegerBinary::Divide),
        ])
        .unwrap(),
    )
}

#[test]
fn upstream_data_errors_win_over_earlier_downstream_errors_and_empty_pages() {
    for count in [0, 1, 8] {
        let query = invalid_child()
            .project(
                vec![GraphSetProjection::new("bad", divide_zero())],
                GraphSetQuantifier::All,
            )
            .unwrap()
            .with_page(0, Some(count));
        let expected = eager(&query).unwrap_err();
        assert!(matches!(
            &expected,
            GqlQueryError::Source(GraphSetExecutionError::Projection {
                row: 1,
                column: 1,
                ..
            })
        ));
        assert_eq!(folded(&query).unwrap_err(), expected);
        assert!(!format!("{expected}").contains("private invalid list"));
    }
    let mut calls = 0;
    let error = invalid_child()
        .fold_governed(
            wide(),
            no_source,
            || Ok(()),
            |_, _| {
                calls += 1;
                Err(GqlQueryError::Source(GraphSetExecutionError::Source(
                    "sink failed",
                )))
            },
        )
        .unwrap_err();
    assert_eq!(calls, 1);
    assert!(matches!(
        error,
        GqlQueryError::Source(GraphSetExecutionError::Projection { row: 1, .. })
    ));
}

#[test]
fn folding_yields_before_expansion_finishes_and_never_charges_private_result_rows() {
    let values: Vec<_> = (0..32).map(Some).collect();
    let query = PreparedGraphSet::singleton()
        .unwind("a".into(), list(&values))
        .unwrap()
        .unwind("b".into(), list(&values))
        .unwrap()
        .unwind("c".into(), list(&values))
        .unwrap();
    let mut checkpoints = 0;
    let mut visited = 0;
    let result = query.fold_governed(
        wide(),
        no_source,
        || {
            checkpoints += 1;
            Ok(())
        },
        |_, _| {
            visited += 1;
            Err(GqlQueryError::Interrupted(999))
        },
    );
    assert!(matches!(result, Err(GqlQueryError::Interrupted(999))));
    assert_eq!(visited, 1);
    assert!(
        checkpoints < 1024,
        "the first row must not wait for the 32768-row expansion"
    );
    let mut count = 0;
    let (rows, _) = query
        .fold_governed(
            GqlQueryPolicy::new(0, 0, u64::MAX, u64::MAX),
            no_source,
            || Ok(()),
            |_, _| {
                count += 1;
                Ok(())
            },
        )
        .unwrap();
    assert_eq!(count, 32 * 32 * 32);
    assert_eq!(rows.result_rows, 0);
}

#[test]
fn every_checkpoint_and_exact_work_scratch_limits_are_fail_stop() {
    let query = PreparedGraphSet::singleton()
        .unwind("x".into(), list(&[Some(2), None, Some(2)]))
        .unwrap()
        .unwind("y".into(), list(&[Some(-1), Some(3)]))
        .unwrap()
        .with_page(1, Some(3));
    let expected = folded(&query).unwrap();
    let mut calls = 0;
    let (_, used) = query
        .fold_governed(
            wide(),
            no_source,
            || {
                calls += 1;
                Ok(())
            },
            |_, _| Ok(()),
        )
        .unwrap();
    let exact = GqlQueryPolicy::new(0, 0, used.work_units, used.scratch_entries);
    assert!(
        query
            .fold_governed(exact, no_source, || Ok(()), |_, _| Ok(()))
            .is_ok()
    );
    for stop in 1..=calls {
        let mut seen = 0;
        let mut prefix = Vec::new();
        let result = query.fold_governed(
            exact,
            no_source,
            || {
                seen += 1;
                if seen == stop { Err(stop) } else { Ok(()) }
            },
            |row, _| {
                prefix.push(row);
                Ok(())
            },
        );
        assert!(matches!(result, Err(GqlQueryError::Interrupted(at)) if at == stop));
        assert_eq!(seen, stop);
        assert!(expected.starts_with(&prefix));
    }
    for policy in [
        GqlQueryPolicy::new(0, 0, used.work_units - 1, u64::MAX),
        GqlQueryPolicy::new(0, 0, u64::MAX, used.scratch_entries - 1),
    ] {
        assert!(matches!(
            query.fold_governed(policy, no_source, || Ok(()), |_, _| Ok(())),
            Err(GqlQueryError::Evaluator(_))
        ));
    }
}

#[test]
fn sorting_and_distinct_barriers_remain_in_the_original_executor() {
    let input = PreparedGraphSet::singleton()
        .unwind("x".into(), list(&[Some(3), Some(1), Some(3)]))
        .unwrap();
    let distinct = input
        .clone()
        .project(
            vec![GraphSetProjection::new("x", GraphSetValue::Column(0))],
            GraphSetQuantifier::Distinct,
        )
        .unwrap();
    assert!(!distinct.has_foldable_expansion());
    let query = distinct
        .unwind("y".into(), list(&[Some(4), Some(-1)]))
        .unwrap();
    assert_eq!(folded(&query).unwrap(), eager(&query).unwrap().value);
    let ranked = query
        .with_order_by(&[GraphValueOrder {
            column: 1,
            descending: true,
            nulls_first: false,
        }])
        .unwrap();
    assert!(!ranked.has_foldable_expansion());
    assert_eq!(folded(&ranked).unwrap(), eager(&ranked).unwrap().value);
}

#[test]
fn all_graph_children_are_observed_once_even_with_an_empty_left_relation() {
    let leaf = |name: &str| {
        let mut builder = crate::algebra::GraphPatternBuilder::new();
        builder.vertex("n").unwrap();
        PreparedGraphSet::from(
            builder
                .prepare_values(&[crate::algebra::GraphColumn::vertex(name, "n")], 0, None)
                .unwrap(),
        )
    };
    let query = leaf("left")
        .cross_join(leaf("right"))
        .unwrap()
        .with_page(0, Some(0));
    let mut calls = 0;
    let result = query.fold_governed(
        wide(),
        |_: &crate::algebra::PreparedGraphPattern<crate::algebra::GraphValueRow>,
         _: crate::GqlQueryPolicy| {
            calls += 1;
            if calls == 2 {
                return Err(GqlQueryError::Source("right failed"));
            }
            Ok(GqlQueryExecution {
                value: vec![],
                rows: GqlExecutionStats {
                    snapshot_records: 2,
                    result_rows: 0,
                },
                evaluator: GlaExecutionStats::default(),
            })
        },
        || Ok::<_, usize>(()),
        |_, _| panic!("empty product"),
    );
    assert_eq!(calls, 2);
    assert!(matches!(
        result,
        Err(GqlQueryError::Source(GraphSetExecutionError::Source(
            "right failed"
        )))
    ));
}
