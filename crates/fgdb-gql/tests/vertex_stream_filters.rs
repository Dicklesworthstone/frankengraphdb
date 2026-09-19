//! Filtered pull execution must agree with GLA without scanning an unused suffix.
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::algebra::{
    GlaDirection, GraphBooleanExpression, GraphBooleanOp as Op, GraphBooleanOperand as Arg,
    GraphColumn, GraphPatternBuilder, GraphValueRow, IntegerComparison as Cmp,
    PreparedGraphPattern,
};
use fgdb_gql::stream::{
    VertexScanCursor, VertexScanError, VertexScanEvent, VertexScanPlan, VertexScanRow,
    VertexScanSource, VertexScanSourceError, VertexScanState,
};
use fgdb_gql::{
    GqlQueryError, GqlQueryPolicy, GraphIntegerBinary, GraphIntegerExpression, GraphIntegerOp as I,
};
use fgdb_types::{CanonicalScalar, CommitSeq, VId};
use std::cell::Cell;
use std::rc::Rc;

const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);

#[derive(Clone)]
struct Record {
    vid: VId,
    visible: bool,
    properties: Vec<(PropertyKeyId, CanonicalScalar)>,
}

struct Source {
    rows: Vec<Record>,
    position: usize,
    reads: Rc<Cell<usize>>,
    dropped: Rc<Cell<bool>>,
    fail_at: Option<usize>,
}

impl Source {
    fn new(rows: &[Record]) -> Self {
        Self {
            rows: rows.to_vec(),
            position: 0,
            reads: Rc::new(Cell::new(0)),
            dropped: Rc::new(Cell::new(false)),
            fail_at: None,
        }
    }
}

impl Drop for Source {
    fn drop(&mut self) {
        self.dropped.set(true);
    }
}

impl VertexScanSource for Source {
    type Error = &'static str;

    fn snapshot_seq(&self) -> CommitSeq {
        CommitSeq(9)
    }

    fn next_vertex<C>(
        &mut self,
        control: &mut impl FnMut(VertexScanEvent) -> Result<(), C>,
    ) -> Result<Option<VId>, VertexScanSourceError<Self::Error, C>> {
        control(VertexScanEvent::Work).map_err(VertexScanSourceError::Control)?;
        if self.fail_at == Some(self.position) {
            return Err(VertexScanSourceError::Source("unread suffix"));
        }
        let next = self.rows.get(self.position).map(|row| row.vid);
        if next.is_some() {
            self.position += 1;
        }
        Ok(next)
    }

    fn vertex<'a, C>(
        &'a self,
        vid: VId,
        control: &mut impl FnMut(VertexScanEvent) -> Result<(), C>,
    ) -> Result<Option<VertexScanRow<'a>>, VertexScanSourceError<Self::Error, C>> {
        control(VertexScanEvent::Work).map_err(VertexScanSourceError::Control)?;
        let row = &self.rows[self.position - 1];
        assert_eq!(row.vid, vid);
        self.reads.set(self.reads.get() + 1);
        Ok(row.visible.then_some(VertexScanRow {
            labels: &[],
            properties: &row.properties,
        }))
    }
}

fn wide() -> GqlQueryPolicy {
    GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX)
}

fn prepare(
    expression: &GraphBooleanExpression,
    offset: u64,
    count: Option<u64>,
) -> PreparedGraphPattern<GraphValueRow> {
    let mut builder = GraphPatternBuilder::new();
    builder.vertex("n").unwrap();
    builder.filter_boolean(expression).unwrap();
    builder
        .prepare_values(&[GraphColumn::vertex("node", "n")], offset, count)
        .unwrap()
}

fn identity(row: GraphValueRow) -> VId {
    row.get(0).unwrap().as_vertex().unwrap()
}

fn property(row: &Record, key: PropertyKeyId) -> Option<&CanonicalScalar> {
    row.properties
        .iter()
        .find(|(actual, _)| *actual == key)
        .map(|(_, value)| value)
}

fn eager(pattern: &PreparedGraphPattern<GraphValueRow>, rows: &[Record]) -> Vec<VId> {
    pattern
        .plan()
        .execute_with_properties_control(
            rows.iter().filter(|row| row.visible).map(|row| row.vid),
            [],
            |vid, predicates| {
                let row = rows.iter().find(|row| row.vid == vid).unwrap();
                Ok::<_, ()>(predicates.iter().all(|p| p.matches(&[], &row.properties)))
            },
            |vid, key| {
                Ok(property(
                    rows.iter().find(|row| row.vid == vid).unwrap(),
                    key,
                ))
            },
            |_| Ok(()),
        )
        .unwrap()
        .into_iter()
        .map(identity)
        .collect()
}

fn expression(and: bool, negate: bool) -> GraphBooleanExpression {
    let zero = CanonicalScalar::Int(0);
    let yes = CanonicalScalar::Bool(true);
    let mut program = vec![
        Op::Compare {
            left: Arg::Property {
                variable: "n",
                key: P,
            },
            comparison: Cmp::Greater,
            right: Arg::Literal(&zero),
        },
        Op::Compare {
            left: Arg::Property {
                variable: "n",
                key: Q,
            },
            comparison: Cmp::Equal,
            right: Arg::Literal(&yes),
        },
        if and { Op::And } else { Op::Or },
    ];
    if negate {
        program.push(Op::Not);
    }
    GraphBooleanExpression::prepare(&program).unwrap()
}

// Independent truth tables, rather than calling the production comparator.
fn truth(row: &Record, and: bool, negate: bool) -> bool {
    let a = match property(row, P) {
        Some(CanonicalScalar::Int(value)) => Some(*value > 0),
        _ => None,
    };
    let b = match property(row, Q) {
        Some(CanonicalScalar::Bool(value)) => Some(*value),
        _ => None,
    };
    let result = if and {
        match (a, b) {
            (Some(false), _) | (_, Some(false)) => Some(false),
            (Some(true), Some(true)) => Some(true),
            _ => None,
        }
    } else {
        match (a, b) {
            (Some(true), _) | (_, Some(true)) => Some(true),
            (Some(false), Some(false)) => Some(false),
            _ => None,
        }
    };
    result.map(|value| if negate { !value } else { value }) == Some(true)
}

fn rows() -> Vec<Record> {
    let choices = [
        None,
        Some(CanonicalScalar::Null),
        Some(CanonicalScalar::Int(-1)),
        Some(CanonicalScalar::Int(0)),
        Some(CanonicalScalar::Int(1)),
        Some(CanonicalScalar::Bool(false)),
        Some(CanonicalScalar::Bool(true)),
    ];
    let mut result = Vec::new();
    for left in &choices {
        for right in &choices {
            let at = result.len();
            result.push(Record {
                vid: VId(if at == 0 {
                    0
                } else {
                    (1_u128 << 100) + at as u128
                }),
                visible: at % 8 != 0,
                properties: [(P, left), (Q, right)]
                    .into_iter()
                    .filter_map(|(key, value)| value.clone().map(|value| (key, value)))
                    .collect(),
            });
        }
    }
    result.last_mut().unwrap().vid = VId(u128::MAX);
    result
}

#[test]
fn boolean_pull_matches_independent_truth_tables_and_eager_gla() {
    let rows = rows();
    for and in [false, true] {
        for negate in [false, true] {
            for offset in [0, 1, 10, u64::MAX] {
                for count in [None, Some(0), Some(1), Some(10)] {
                    for all in [false, true] {
                        let query = prepare(&expression(and, negate), offset, count);
                        let query = if all { query.with_duplicates() } else { query };
                        let expected: Vec<_> = rows
                            .iter()
                            .filter(|row| row.visible && truth(row, and, negate))
                            .map(|row| row.vid)
                            .skip(usize::try_from(offset).unwrap_or(usize::MAX))
                            .take(usize::try_from(count.unwrap_or(u64::MAX)).unwrap_or(usize::MAX))
                            .collect();
                        assert_eq!(eager(&query, &rows), expected);
                        let mut cursor = VertexScanCursor::new(
                            Source::new(&rows),
                            VertexScanPlan::compile(query.plan()).unwrap(),
                            wide(),
                            || Ok::<_, ()>(()),
                        );
                        assert_eq!(
                            cursor
                                .by_ref()
                                .map(|row| row.map(identity))
                                .collect::<Result<Vec<_>, _>>()
                                .unwrap(),
                            expected
                        );
                        assert_eq!(cursor.row_stats().result_rows, expected.len() as u64);
                        assert_eq!(cursor.state(), VertexScanState::Exhausted);
                        assert!(cursor.next().is_none());
                    }
                }
            }
        }
    }
}

#[test]
fn boolean_limit_and_close_do_not_prefetch_a_failing_suffix() {
    let rows = rows();
    let condition = expression(false, false);
    let first = rows
        .iter()
        .position(|row| row.visible && truth(row, false, false))
        .unwrap();
    for count in [None, Some(1)] {
        let query = prepare(&condition, 0, count);
        let mut input = Source::new(&rows);
        input.fail_at = Some(first + 1);
        let reads = Rc::clone(&input.reads);
        let dropped = Rc::clone(&input.dropped);
        let mut cursor = VertexScanCursor::new(
            input,
            VertexScanPlan::compile(query.plan()).unwrap(),
            wide(),
            || Ok::<_, ()>(()),
        );
        assert_eq!(reads.get(), 0);
        assert_eq!(identity(cursor.next().unwrap().unwrap()), rows[first].vid);
        assert_eq!(reads.get(), first + 1);
        if count.is_some() {
            assert_eq!(cursor.state(), VertexScanState::Exhausted);
        } else {
            cursor.close();
            assert_eq!(cursor.state(), VertexScanState::Closed);
        }
        assert!(dropped.get());
        assert!(cursor.next().is_none());
        assert_eq!(reads.get(), first + 1);
    }
}

#[test]
fn boolean_interruptions_are_terminal_at_every_checkpoint() {
    let rows = rows();
    let query = prepare(&expression(true, true), 1, Some(3));
    let scan = VertexScanPlan::compile(query.plan()).unwrap();
    let calls = Cell::new(0);
    let expected = VertexScanCursor::new(Source::new(&rows), scan.clone(), wide(), || {
        calls.set(calls.get() + 1);
        Ok::<_, usize>(())
    })
    .map(|row| row.map(identity))
    .collect::<Result<Vec<_>, _>>()
    .unwrap();
    assert_eq!(expected.len(), 3);
    for stop in 1..=calls.get() {
        let input = Source::new(&rows);
        let dropped = Rc::clone(&input.dropped);
        let at = Cell::new(0);
        let mut cursor = VertexScanCursor::new(input, scan.clone(), wide(), || {
            at.set(at.get() + 1);
            if at.get() == stop { Err(stop) } else { Ok(()) }
        });
        let mut prefix = Vec::new();
        let mut failure = None;
        for next in cursor.by_ref() {
            match next {
                Ok(row) => prefix.push(identity(row)),
                Err(error) => {
                    failure = Some(error);
                    break;
                }
            }
        }
        assert!(matches!(failure, Some(GqlQueryError::Interrupted(observed)) if observed == stop));
        assert!(expected.starts_with(&prefix));
        assert_eq!(cursor.row_stats().result_rows, prefix.len() as u64);
        assert_eq!(cursor.state(), VertexScanState::Failed);
        assert!(dropped.get());
        assert!(cursor.next().is_none());
        assert_eq!(at.get(), stop);
    }
}

#[test]
fn boolean_work_allowance_is_cumulative_and_source_errors_are_not_nulls() {
    let rows = rows();
    let query = prepare(&expression(false, false), 0, None);
    let scan = VertexScanPlan::compile(query.plan()).unwrap();
    let mut cursor =
        VertexScanCursor::new(Source::new(&rows), scan.clone(), wide(), || Ok::<_, ()>(()));
    let expected = cursor
        .by_ref()
        .map(|row| row.map(identity))
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let work = cursor.evaluator_stats().work_units;
    assert!(work > rows.len() as u64);
    for allowed in [work, work - 1] {
        let input = Source::new(&rows);
        let dropped = Rc::clone(&input.dropped);
        let mut cursor = VertexScanCursor::new(
            input,
            scan.clone(),
            GqlQueryPolicy::new(u64::MAX, u64::MAX, allowed, u64::MAX),
            || Ok::<_, ()>(()),
        );
        let result = cursor
            .by_ref()
            .map(|row| row.map(identity))
            .collect::<Result<Vec<_>, _>>();
        if allowed == work {
            assert_eq!(result.unwrap(), expected);
        } else {
            assert!(matches!(result, Err(GqlQueryError::Evaluator(_))));
            assert_eq!(cursor.state(), VertexScanState::Failed);
        }
        assert!(dropped.get());
        assert!(cursor.next().is_none());
    }
    let mut input = Source::new(&rows);
    input.fail_at = Some(0);
    let mut cursor = VertexScanCursor::new(input, scan, wide(), || Ok::<_, ()>(()));
    assert!(matches!(
        cursor.next(),
        Some(Err(GqlQueryError::Source(VertexScanError::Source(
            "unread suffix"
        ))))
    ));
    assert!(cursor.next().is_none());
}

#[test]
fn boolean_identity_led_value_projection_preserves_the_property_payload() {
    let rows = rows();
    let condition = expression(false, false);
    let mut builder = GraphPatternBuilder::new();
    builder.vertex("n").unwrap();
    builder.filter_boolean(&condition).unwrap();
    let query = builder
        .prepare_values(
            &[
                GraphColumn::vertex("node", "n"),
                GraphColumn::property("value", "n", P),
            ],
            0,
            Some(2),
        )
        .unwrap()
        .with_duplicates();
    let actual = VertexScanCursor::new(
        Source::new(&rows),
        VertexScanPlan::compile(query.plan()).unwrap(),
        wide(),
        || Ok::<_, ()>(()),
    )
    .collect::<Result<Vec<_>, _>>()
    .unwrap();
    let expected = query
        .plan()
        .execute_with_properties_control(
            rows.iter().filter(|row| row.visible).map(|row| row.vid),
            [],
            |_, _| Ok::<_, ()>(true),
            |vid, key| {
                Ok(property(
                    rows.iter().find(|row| row.vid == vid).unwrap(),
                    key,
                ))
            },
            |_| Ok(()),
        )
        .unwrap();
    assert_eq!(actual, expected);
    assert_eq!(actual.len(), 2);
}

#[test]
fn graph_expansion_with_a_boolean_filter_still_refuses_stream_compilation() {
    let zero = CanonicalScalar::Int(0);
    let condition = GraphBooleanExpression::prepare(&[Op::Compare {
        left: Arg::Property {
            variable: "other",
            key: P,
        },
        comparison: Cmp::Greater,
        right: Arg::Literal(&zero),
    }])
    .unwrap();
    let mut builder = GraphPatternBuilder::new();
    builder.vertex("n").unwrap();
    builder.vertex("other").unwrap();
    builder
        .edge("n", RelationId(1), GlaDirection::Forward, "other")
        .unwrap();
    builder.filter_boolean(&condition).unwrap();
    let query = builder
        .prepare_values(&[GraphColumn::vertex("node", "n")], 0, None)
        .unwrap();
    assert!(VertexScanPlan::compile(query.plan()).is_err());
}

fn comparison_query(
    comparison: Cmp,
    right_key: PropertyKeyId,
    offset: u64,
    count: Option<u64>,
) -> PreparedGraphPattern<GraphValueRow> {
    let mut builder = GraphPatternBuilder::new();
    builder.vertex("n").unwrap();
    builder
        .compare_properties("n", P, comparison, "n", right_key)
        .unwrap();
    builder
        .prepare_values(&[GraphColumn::vertex("node", "n")], offset, count)
        .unwrap()
}

fn pair_truth(row: &Record, right_key: PropertyKeyId, comparison: Cmp) -> bool {
    use std::cmp::Ordering::{Equal, Greater, Less};
    let order = match (property(row, P), property(row, right_key)) {
        (Some(CanonicalScalar::Int(a)), Some(CanonicalScalar::Int(b))) => a.cmp(b),
        (Some(CanonicalScalar::Bool(a)), Some(CanonicalScalar::Bool(b))) => a.cmp(b),
        _ => return false,
    };
    match comparison {
        Cmp::Equal => order == Equal,
        Cmp::NotEqual => order != Equal,
        Cmp::Less => order == Less,
        Cmp::LessOrEqual => order != Greater,
        Cmp::Greater => order == Greater,
        Cmp::GreaterOrEqual => order != Less,
    }
}

#[test]
fn local_property_pairs_match_an_independent_oracle_for_all_comparisons() {
    let rows = rows();
    for comparison in [
        Cmp::Equal,
        Cmp::NotEqual,
        Cmp::Less,
        Cmp::LessOrEqual,
        Cmp::Greater,
        Cmp::GreaterOrEqual,
    ] {
        for right_key in [P, Q] {
            for offset in [0, 1, u64::MAX] {
                for count in [None, Some(0), Some(2)] {
                    for all in [false, true] {
                        let query = comparison_query(comparison, right_key, offset, count);
                        let query = if all { query.with_duplicates() } else { query };
                        let expected: Vec<_> = rows
                            .iter()
                            .filter(|row| row.visible && pair_truth(row, right_key, comparison))
                            .map(|row| row.vid)
                            .skip(usize::try_from(offset).unwrap_or(usize::MAX))
                            .take(usize::try_from(count.unwrap_or(u64::MAX)).unwrap_or(usize::MAX))
                            .collect();
                        assert_eq!(eager(&query, &rows), expected);
                        let before = query.canonical_bytes();
                        let scan = VertexScanPlan::compile(query.plan()).unwrap();
                        assert_eq!(query.canonical_bytes(), before);
                        let actual =
                            VertexScanCursor::new(Source::new(&rows), scan, wide(), || {
                                Ok::<_, ()>(())
                            })
                            .map(|row| row.map(identity))
                            .collect::<Result<Vec<_>, _>>()
                            .unwrap();
                        assert_eq!(actual, expected);
                    }
                }
            }
        }
    }
}

#[test]
fn property_pair_payload_work_is_charged_before_delivery_and_limit_drops_the_source() {
    let row = Record {
        vid: VId(7),
        visible: true,
        properties: vec![
            (
                P,
                CanonicalScalar::ucs_basic_text(&"a".repeat(4096)).unwrap(),
            ),
            (
                Q,
                CanonicalScalar::ucs_basic_text(&"b".repeat(8192)).unwrap(),
            ),
        ],
    };
    let rows = [row];
    let query = comparison_query(Cmp::Less, Q, 0, Some(1));
    let scan = VertexScanPlan::compile(query.plan()).unwrap();
    let mut input = Source::new(&rows);
    input.fail_at = Some(1);
    let reads = Rc::clone(&input.reads);
    let dropped = Rc::clone(&input.dropped);
    let mut cursor = VertexScanCursor::new(input, scan.clone(), wide(), || Ok::<_, ()>(()));
    assert_eq!(identity(cursor.next().unwrap().unwrap()), VId(7));
    assert_eq!(reads.get(), 1);
    assert!(dropped.get());
    assert!(cursor.next().is_none());
    let measured = cursor.evaluator_stats();
    let payload_work = 4096_usize.div_ceil(fgdb_gql::algebra::GRAPH_VALUE_PAYLOAD_UNIT_BYTES)
        + 8192_usize.div_ceil(fgdb_gql::algebra::GRAPH_VALUE_PAYLOAD_UNIT_BYTES);
    assert!(measured.work_units >= payload_work as u64);

    for work in [measured.work_units, measured.work_units - 1] {
        let input = Source::new(&rows);
        let dropped = Rc::clone(&input.dropped);
        let mut cursor = VertexScanCursor::new(
            input,
            scan.clone(),
            GqlQueryPolicy::new(1, 1, work, measured.scratch_entries),
            || Ok::<_, ()>(()),
        );
        let next = cursor.next().unwrap();
        if work == measured.work_units {
            assert_eq!(identity(next.unwrap()), VId(7));
            assert_eq!(cursor.row_stats().result_rows, 1);
        } else {
            assert!(matches!(next, Err(GqlQueryError::Evaluator(_))));
            assert_eq!(cursor.row_stats().result_rows, 0);
            assert_eq!(cursor.state(), VertexScanState::Failed);
        }
        assert!(dropped.get());
        assert!(cursor.next().is_none());
    }
}

fn guarded_scalar() -> GraphIntegerExpression {
    // CASE WHEN p > 0 THEN true ELSE 1 / 0 = 0 END. Positive values must
    // never evaluate the invalid arm; other values yield UNKNOWN in WHERE.
    GraphIntegerExpression::prepare_scalar(&[
        I::Column(0),
        I::Literal(Some(0)),
        I::Compare(Cmp::Greater),
        I::Truth(Some(true)),
        I::Literal(Some(1)),
        I::Literal(Some(0)),
        I::Binary(GraphIntegerBinary::Divide),
        I::Literal(Some(0)),
        I::Compare(Cmp::Equal),
        I::Case,
    ])
    .unwrap()
}

#[test]
fn scalar_filters_preserve_lazy_case_and_exact_cumulative_scratch_limits() {
    let rows = rows();
    let scalar = guarded_scalar();
    let columns = [Arg::Property {
        variable: "n",
        key: P,
    }];
    let condition = GraphBooleanExpression::prepare(&[Op::Expression {
        expression: &scalar,
        columns: &columns,
    }])
    .unwrap();
    let query = prepare(&condition, 0, None);
    let expected: Vec<_> = rows
        .iter()
        .filter(|row| {
            row.visible
                && matches!(property(row, P), Some(CanonicalScalar::Int(value)) if *value > 0)
        })
        .map(|row| row.vid)
        .collect();
    assert!(!expected.is_empty());
    assert_eq!(eager(&query, &rows), expected);
    let scan = VertexScanPlan::compile(query.plan()).unwrap();
    let mut cursor =
        VertexScanCursor::new(Source::new(&rows), scan.clone(), wide(), || Ok::<_, ()>(()));
    assert_eq!(
        cursor
            .by_ref()
            .map(|row| row.map(identity))
            .collect::<Result<Vec<_>, _>>()
            .unwrap(),
        expected
    );
    let measured = cursor.evaluator_stats();
    assert!(measured.scratch_entries > expected.len() as u64);
    for scratch in [measured.scratch_entries, measured.scratch_entries - 1, 0] {
        let input = Source::new(&rows);
        let dropped = Rc::clone(&input.dropped);
        let mut cursor = VertexScanCursor::new(
            input,
            scan.clone(),
            GqlQueryPolicy::new(u64::MAX, u64::MAX, measured.work_units, scratch),
            || Ok::<_, ()>(()),
        );
        let result = cursor
            .by_ref()
            .map(|row| row.map(identity))
            .collect::<Result<Vec<_>, _>>();
        if scratch == measured.scratch_entries {
            assert_eq!(result.unwrap(), expected);
        } else {
            assert!(matches!(result, Err(GqlQueryError::Evaluator(_))));
            assert_eq!(cursor.state(), VertexScanState::Failed);
            if scratch == 0 {
                assert_eq!(cursor.row_stats().result_rows, 0);
                assert_eq!(cursor.evaluator_stats().scratch_entries, 0);
            }
        }
        assert!(dropped.get());
        assert!(cursor.next().is_none());
    }
}

#[test]
fn a_true_or_operand_does_not_bypass_scalar_work_or_cancellation() {
    let rows = rows();
    let scalar = guarded_scalar();
    let columns = [Arg::Property {
        variable: "n",
        key: P,
    }];
    let condition = GraphBooleanExpression::prepare(&[
        Op::Truth(Some(true)),
        Op::Expression {
            expression: &scalar,
            columns: &columns,
        },
        Op::Or,
    ])
    .unwrap();
    let query = prepare(&condition, 0, Some(1));
    let scan = VertexScanPlan::compile(query.plan()).unwrap();
    let calls = Cell::new(0);
    let baseline = VertexScanCursor::new(Source::new(&rows), scan.clone(), wide(), || {
        calls.set(calls.get() + 1);
        Ok::<_, usize>(())
    })
    .map(|row| row.map(identity))
    .collect::<Result<Vec<_>, _>>()
    .unwrap();
    assert_eq!(
        baseline,
        vec![rows.iter().find(|row| row.visible).unwrap().vid]
    );
    for stop in 1..=calls.get() {
        let input = Source::new(&rows);
        let dropped = Rc::clone(&input.dropped);
        let at = Cell::new(0);
        let mut cursor = VertexScanCursor::new(input, scan.clone(), wide(), || {
            at.set(at.get() + 1);
            if at.get() == stop { Err(stop) } else { Ok(()) }
        });
        assert!(
            matches!(cursor.next(), Some(Err(GqlQueryError::Interrupted(observed)))
            if observed == stop)
        );
        assert_eq!(cursor.row_stats().result_rows, 0);
        assert_eq!(cursor.state(), VertexScanState::Failed);
        assert!(dropped.get());
        assert!(cursor.next().is_none());
        assert_eq!(at.get(), stop);
    }
    let mut cursor = VertexScanCursor::new(
        Source::new(&rows),
        scan,
        GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, 0),
        || Ok::<_, ()>(()),
    );
    assert!(matches!(
        cursor.next(),
        Some(Err(GqlQueryError::Evaluator(_)))
    ));
    assert_eq!(cursor.row_stats().result_rows, 0);
    assert!(cursor.next().is_none());
}

#[test]
fn property_sensitive_filters_keep_the_legacy_projection_refusal() {
    let mut builder = GraphPatternBuilder::new();
    builder.vertex("n").unwrap();
    builder.filter_boolean(&expression(false, false)).unwrap();
    assert!(matches!(
        builder.prepare("n", 0, None),
        Err(fgdb_gql::algebra::PatternBuildError::RequiresValueProjection)
    ));
    let query = builder
        .prepare_values(&[GraphColumn::vertex("node", "n")], 0, None)
        .unwrap();
    assert!(VertexScanPlan::compile(query.plan()).is_ok());
}
