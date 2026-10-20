use super::*;
use crate::algebra::{GraphColumn, GraphPatternBuilder, VertexPredicate};
use crate::{
    GqlParameters, GraphPatternFactorExecution, GraphSymbol, GraphSymbolKind,
    PreparedGraphPatternFactors, PreparedGraphSetText,
};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_types::{CanonicalDecimal, CanonicalF64, CanonicalScalar, VId};
use std::collections::BTreeMap;

const P: PropertyKeyId = PropertyKeyId(1);
const LABEL: LabelId = LabelId(1);
type SourceResult = Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<&'static str, usize>>;
type Records = BTreeMap<VId, (Vec<LabelId>, Vec<(PropertyKeyId, CanonicalScalar)>)>;

// A fixed already admitted table, evaluated through the actual GLA kernel in
// both modes. The factor owner reports this domain once and carries the first
// evaluation's work/scratch into the second; it never manufactures pair rows.
struct Source {
    records: Records,
    factor: bool,
    patterns: usize,
    factors: usize,
}

impl Source {
    fn new(values: &[CanonicalScalar], factor: bool) -> Self {
        Self {
            records: values
                .iter()
                .enumerate()
                .map(|(at, value)| (VId(at as u128 + 1), (vec![LABEL], vec![(P, value.clone())])))
                .collect(),
            factor,
            patterns: 0,
            factors: 0,
        }
    }

    fn evaluate(
        &self,
        pattern: &PreparedGraphPattern<GraphValueRow>,
        policy: GqlQueryPolicy,
        admitted: u64,
    ) -> SourceResult {
        pattern.plan().execute_governed_with_properties(
            admitted,
            self.records.keys().copied(),
            std::iter::empty::<(VId, RelationId, VId)>(),
            |vid, predicates| {
                let (labels, properties) = &self.records[&vid];
                Ok(predicates.iter().all(|p| p.matches(labels, properties)))
            },
            |vid, key| {
                Ok(self.records[&vid]
                    .1
                    .iter()
                    .find(|(candidate, _)| *candidate == key)
                    .map(|(_, value)| value))
            },
            policy,
            || Ok::<_, usize>(()),
        )
    }

    fn evaluate_factors(
        &self,
        request: &PreparedGraphPatternFactors<'_>,
        policy: GqlQueryPolicy,
    ) -> Result<GraphPatternFactorExecution, GqlQueryError<&'static str, usize>> {
        let count = self.records.len() as u64;
        let first = self.evaluate(&request.factors()[0], policy, count)?;
        let remaining = GqlQueryPolicy::new(
            policy.rows.max_snapshot_records().unwrap() - count,
            u64::MAX,
            policy.evaluator.max_work_units - first.evaluator.work_units,
            policy.evaluator.max_scratch_entries - first.evaluator.scratch_entries,
        );
        let second =
            self.evaluate(&request.factors()[1], remaining, 0)
                .map_err(|error| match error {
                    GqlQueryError::Evaluator(error) => {
                        let (used, limit) = match error.dimension {
                            GlaLimitDimension::WorkUnits => {
                                (first.evaluator.work_units, policy.evaluator.max_work_units)
                            }
                            GlaLimitDimension::ScratchEntries => (
                                first.evaluator.scratch_entries,
                                policy.evaluator.max_scratch_entries,
                            ),
                        };
                        GqlQueryError::Evaluator(GlaLimitExceeded {
                            dimension: error.dimension,
                            observed: error.observed + u128::from(used),
                            limit,
                        })
                    }
                    error => error,
                })?;
        Ok(GraphPatternFactorExecution {
            rows: GqlExecutionStats {
                snapshot_records: count,
                result_rows: first.rows.result_rows + second.rows.result_rows,
            },
            evaluator: GlaExecutionStats {
                work_units: first.evaluator.work_units + second.evaluator.work_units,
                scratch_entries: first.evaluator.scratch_entries + second.evaluator.scratch_entries,
            },
            value: [first.value, second.value],
        })
    }
}

impl GraphSetSource<&'static str, usize> for Source {
    fn pattern(
        &mut self,
        pattern: &PreparedGraphPattern<GraphValueRow>,
        policy: GqlQueryPolicy,
    ) -> SourceResult {
        self.patterns += 1;
        self.evaluate(pattern, policy, self.records.len() as u64)
    }
    fn supports_pattern_factors(&self) -> bool {
        self.factor
    }
    fn pattern_factors(
        &mut self,
        request: &PreparedGraphPatternFactors<'_>,
        policy: GqlQueryPolicy,
    ) -> Option<Result<GraphPatternFactorExecution, GqlQueryError<&'static str, usize>>> {
        assert!(self.factor);
        self.factors += 1;
        Some(self.evaluate_factors(request, policy))
    }
}

fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(10_000, 100_000, 100_000_000, 100_000_000)
}
fn int(value: i64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(value))
}
fn imported(source: GraphValue, destination: GraphValue, weight: i64) -> GraphValue {
    GraphValue::map(vec![
        ("src".into(), source),
        ("dst".into(), destination),
        ("weight".into(), int(weight)),
    ])
    .unwrap()
}

fn prepare(rows: Vec<GraphValue>) -> PreparedGraphSet {
    let arguments = GqlParameters::new().with_list("rows", rows).unwrap();
    PreparedGraphSetText::prepare(
        "UNWIND $rows AS row MATCH (a:Source {p:row.src}), (b:Source {p:row.dst}) \
         RETURN row.weight AS weight,a,b,a.p AS source_key,b.p AS destination_key",
        |kind, name| match (kind, name) {
            (GraphSymbolKind::Label, "Source") => Some(GraphSymbol::Label(LABEL)),
            (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
            _ => None,
        },
    )
    .unwrap()
    .bind_parameters(&arguments)
    .unwrap()
}

fn execute(
    query: &PreparedGraphSet,
    source: &mut impl GraphSetSource<&'static str, usize>,
    policy: GqlQueryPolicy,
) -> SetResult<GqlQueryExecution<GraphValueRow>, &'static str, usize> {
    query
        .execute_with_source(policy, source, || Ok(()))
        .map(|execution| {
            assert_eq!(execution.rows.result_rows, execution.value.len() as u64);
            execution
        })
}

#[test]
fn factored_import_matches_original_gla_sequence_for_any_null_numeric_and_duplicate_keys() {
    let float = CanonicalScalar::Float(CanonicalF64::new(1.0));
    let decimal = CanonicalScalar::Decimal(CanonicalDecimal::from_scaled_half_even(1, 0).unwrap());
    let null = GraphValue::Scalar(CanonicalScalar::Null);
    let query = prepare(vec![
        imported(int(2), GraphValue::Scalar(float.clone()), 20),
        imported(GraphValue::Scalar(float.clone()), int(2), 10),
        imported(int(2), GraphValue::Scalar(float.clone()), 20),
        imported(null.clone(), int(1), 0),
        imported(GraphValue::List(vec![int(1)].into_boxed_slice()), int(1), 0),
        imported(
            GraphValue::map(vec![("x".into(), int(1))]).unwrap(),
            int(1),
            0,
        ),
        imported(
            GraphValue::Scalar(decimal.clone()),
            GraphValue::Scalar(decimal.clone()),
            30,
        ),
        imported(GraphValue::Vertex(VId(1)), int(1), 0),
    ]);
    let values = [
        CanonicalScalar::Int(1),
        float,
        CanonicalScalar::Int(2),
        CanonicalScalar::Int(2),
        CanonicalScalar::Null,
        decimal,
    ];
    let before = query.canonical_bytes();
    let mut ordinary = Source::new(&values, false);
    let mut factored = Source::new(&values, true);
    let expected = execute(&query, &mut ordinary, policy()).unwrap();
    let actual = execute(&query, &mut factored, policy()).unwrap();
    assert_eq!(actual.value, expected.value);
    assert_eq!(actual.rows, expected.rows);
    assert_eq!((ordinary.patterns, ordinary.factors), (1, 0));
    assert_eq!((factored.patterns, factored.factors), (0, 1));
    assert_eq!(query.canonical_bytes(), before);
    let first: Vec<_> = actual.value[..12]
        .iter()
        .map(|row| row.values()[..3].to_vec())
        .collect();
    let pair = |weight, a, b| {
        vec![
            int(weight),
            GraphValue::Vertex(VId(a)),
            GraphValue::Vertex(VId(b)),
        ]
    };
    assert_eq!(
        first,
        vec![
            pair(20, 3, 1),
            pair(20, 3, 2),
            pair(20, 4, 1),
            pair(20, 4, 2),
            pair(10, 1, 3),
            pair(10, 1, 4),
            pair(10, 2, 3),
            pair(10, 2, 4),
            pair(20, 3, 1),
            pair(20, 3, 2),
            pair(20, 4, 1),
            pair(20, 4, 2),
        ]
    );
}

#[test]
fn one_original_record_and_one_public_row_admit_two_private_factor_rows() {
    let query = prepare(vec![imported(int(1), int(1), 9)]);
    let mut source = Source::new(&[CanonicalScalar::Int(1)], true);
    let result = execute(
        &query,
        &mut source,
        GqlQueryPolicy::new(1, 1, 100_000, 100_000),
    )
    .unwrap();
    assert_eq!(
        result.rows,
        GqlExecutionStats {
            snapshot_records: 1,
            result_rows: 1
        }
    );
    assert_eq!(
        result.value[0].values()[1..3],
        [GraphValue::Vertex(VId(1)), GraphValue::Vertex(VId(1))]
    );
    let budget = GqlQueryPolicy::new(
        1,
        1,
        result.evaluator.work_units,
        result.evaluator.scratch_entries,
    );
    assert!(execute(&query, &mut source, budget).is_ok());
    for (work, scratch, dimension) in [
        (
            budget.evaluator.max_work_units - 1,
            budget.evaluator.max_scratch_entries,
            GlaLimitDimension::WorkUnits,
        ),
        (
            budget.evaluator.max_work_units,
            budget.evaluator.max_scratch_entries - 1,
            GlaLimitDimension::ScratchEntries,
        ),
    ] {
        assert!(
            matches!(execute(&query, &mut source, GqlQueryPolicy::new(1,1,work,scratch)),
            Err(GqlQueryError::Evaluator(error)) if error.dimension == dimension
                && error.limit == if dimension == GlaLimitDimension::WorkUnits { work } else { scratch })
        );
    }
    assert!(
        matches!(execute(&query, &mut source, GqlQueryPolicy::new(0,1,100_000,100_000)),
        Err(GqlQueryError::Rows(error)) if error.dimension == GqlBudgetDimension::SnapshotRecords)
    );
    assert!(
        matches!(execute(&query, &mut source, GqlQueryPolicy::new(1,0,100_000,100_000)),
        Err(GqlQueryError::Rows(error)) if error.dimension == GqlBudgetDimension::ResultRows)
    );
}

#[test]
fn absent_second_keys_do_not_materialize_graph_pairs_or_partial_leading_matches() {
    let n = 128;
    let query = prepare(
        (0..n)
            .map(|weight| imported(int(1), int(2), weight))
            .collect(),
    );
    let values = vec![CanonicalScalar::Int(1); n as usize];
    let mut ordinary = Source::new(&values, false);
    let expected = execute(&query, &mut ordinary, policy()).unwrap();
    let mut factored = Source::new(&values, true);
    let actual = execute(&query, &mut factored, policy()).unwrap();
    assert_eq!(actual.value, expected.value);
    assert!(actual.value.is_empty());
    assert_eq!(actual.rows.snapshot_records, n as u64);
    assert_eq!((factored.patterns, factored.factors), (0, 1));
    assert!(
        actual.evaluator.scratch_entries < n as u64 * 256,
        "{:?}",
        actual.evaluator
    );
    assert!(actual.evaluator.scratch_entries * 4 < expected.evaluator.scratch_entries);
    assert!(actual.evaluator.work_units * 4 < expected.evaluator.work_units);
}

fn direct(keys: Vec<GraphSetPredicateOp>) -> PreparedGraphSet {
    let mut builder = GraphPatternBuilder::new();
    builder.vertex("a").unwrap().vertex("b").unwrap();
    let pattern = builder
        .prepare_values(
            &[
                GraphColumn::vertex("a", "a"),
                GraphColumn::vertex("b", "b"),
                GraphColumn::property("ap", "a", P),
                GraphColumn::property("bp", "b", P),
            ],
            0,
            None,
        )
        .unwrap();
    PreparedGraphSet::singleton()
        .unwind(
            "key".into(),
            GraphSetValue::List(vec![GraphSetValue::Value(int(1))]),
        )
        .unwrap()
        .cross_join(pattern.into())
        .unwrap()
        .filter(&keys)
        .unwrap()
}

fn equal(a: usize, b: usize) -> GraphSetPredicateOp {
    GraphSetPredicateOp::Compare {
        left: GraphSetOperand::Column(a),
        comparison: IntegerComparison::Equal,
        right: GraphSetOperand::Column(b),
    }
}

#[test]
fn unsupported_predicate_fallback_has_identical_admission_and_work() {
    for code in [
        vec![equal(0, 3)],
        vec![equal(0, 3), equal(0, 4), GraphSetPredicateOp::Or],
        vec![
            equal(0, 3),
            equal(0, 4),
            GraphSetPredicateOp::And,
            GraphSetPredicateOp::Not,
        ],
    ] {
        let query = direct(code);
        let mut ordinary = Source::new(&[CanonicalScalar::Int(1)], false);
        let mut capable = Source::new(&[CanonicalScalar::Int(1)], true);
        let expected = execute(&query, &mut ordinary, policy()).unwrap();
        let actual = execute(&query, &mut capable, policy()).unwrap();
        assert_eq!(actual, expected);
        assert_eq!((capable.patterns, capable.factors), (1, 0));
        let exact = GqlQueryPolicy::new(
            1,
            1,
            expected.evaluator.work_units,
            expected.evaluator.scratch_entries,
        );
        assert_eq!(
            execute(&query, &mut capable, exact),
            execute(&query, &mut ordinary, exact)
        );
    }
}

#[test]
fn local_graph_predicates_keep_the_original_source_and_its_statistics() {
    let mut builder = GraphPatternBuilder::new();
    builder.vertex("a").unwrap().vertex("b").unwrap();
    builder
        .filter(
            "b",
            VertexPredicate::IntegerProperty {
                key: P,
                comparison: IntegerComparison::Greater,
                value: 0,
            },
        )
        .unwrap();
    let pattern = builder
        .prepare_values(
            &[
                GraphColumn::vertex("a", "a"),
                GraphColumn::vertex("b", "b"),
                GraphColumn::property("ap", "a", P),
                GraphColumn::property("bp", "b", P),
            ],
            0,
            None,
        )
        .unwrap();
    let query = PreparedGraphSet::singleton()
        .unwind(
            "key".into(),
            GraphSetValue::List(vec![GraphSetValue::Value(int(1))]),
        )
        .unwrap()
        .cross_join(pattern.into())
        .unwrap()
        .filter(&[equal(0, 3), equal(0, 4), GraphSetPredicateOp::And])
        .unwrap();
    let mut ordinary = Source::new(&[CanonicalScalar::Int(1)], false);
    let mut capable = Source::new(&[CanonicalScalar::Int(1)], true);
    assert_eq!(
        execute(&query, &mut capable, policy()),
        execute(&query, &mut ordinary, policy())
    );
    assert_eq!((capable.patterns, capable.factors), (1, 0));
}

struct FailedSource {
    calls: usize,
    unavailable: bool,
}
impl GraphSetSource<&'static str, usize> for FailedSource {
    fn pattern(
        &mut self,
        _: &PreparedGraphPattern<GraphValueRow>,
        _: GqlQueryPolicy,
    ) -> SourceResult {
        panic!("factor refusal never reopens the original product")
    }
    fn supports_pattern_factors(&self) -> bool {
        true
    }
    fn pattern_factors(
        &mut self,
        _: &PreparedGraphPatternFactors<'_>,
        _: GqlQueryPolicy,
    ) -> Option<Result<GraphPatternFactorExecution, GqlQueryError<&'static str, usize>>> {
        self.calls += 1;
        (!self.unavailable).then_some(Err(GqlQueryError::Source("late source admission")))
    }
}

#[test]
fn empty_or_null_leading_keys_cannot_skip_source_failure_or_trigger_rescan() {
    let null = GraphValue::Scalar(CanonicalScalar::Null);
    for rows in [Vec::new(), vec![imported(null.clone(), null.clone(), 0)]] {
        let query = prepare(rows);
        for unavailable in [false, true] {
            let mut source = FailedSource {
                calls: 0,
                unavailable,
            };
            let result = execute(&query, &mut source, policy());
            if unavailable {
                assert_eq!(
                    result,
                    Err(GqlQueryError::Source(
                        GraphSetExecutionError::PatternFactorsUnavailable { operand: 0 }
                    ))
                );
            } else {
                assert_eq!(
                    result,
                    Err(GqlQueryError::Source(GraphSetExecutionError::Source(
                        "late source admission"
                    )))
                );
            }
            assert_eq!(source.calls, 1);
        }
    }
}

#[test]
fn invalid_leading_map_fields_finish_before_original_graph_source() {
    let query = prepare(vec![int(7)]);
    let mut source = FailedSource {
        calls: 0,
        unavailable: false,
    };
    assert!(matches!(
        execute(&query, &mut source, policy()),
        Err(GqlQueryError::Source(
            GraphSetExecutionError::Projection { .. }
        ))
    ));
    assert_eq!(source.calls, 0);
}

struct InvalidSource {
    kind: usize,
}
impl GraphSetSource<&'static str, usize> for InvalidSource {
    fn pattern(
        &mut self,
        _: &PreparedGraphPattern<GraphValueRow>,
        _: GqlQueryPolicy,
    ) -> SourceResult {
        panic!("an invalid factor source must not fall back")
    }
    fn supports_pattern_factors(&self) -> bool {
        true
    }
    fn pattern_factors(
        &mut self,
        request: &PreparedGraphPatternFactors<'_>,
        policy: GqlQueryPolicy,
    ) -> Option<Result<GraphPatternFactorExecution, GqlQueryError<&'static str, usize>>> {
        if self.kind == 2 {
            return Some(Err(GqlQueryError::Interrupted(77)));
        }
        let source = Source::new(&[CanonicalScalar::Int(1)], true);
        Some(
            source
                .evaluate_factors(request, policy)
                .map(|mut execution| {
                    if self.kind == 0 {
                        execution.rows.result_rows += 1;
                    } else if self.kind == 1 {
                        // Even an empty first factor cannot hide the second schema.
                        execution.rows.result_rows -= execution.value[0].len() as u64;
                        execution.value[0].clear();
                        execution.value[1][0] = GraphValueRow::unit();
                    } else {
                        // An empty second factor cannot hide an invalid identity
                        // domain in the first, including a correctly typed NULL.
                        execution.value[1].clear();
                        if self.kind == 3 {
                            let duplicate = execution.value[0][0].clone();
                            execution.value[0].push(duplicate);
                        } else {
                            let mut values = execution.value[0][0].values().to_vec();
                            values[0] = GraphValue::Scalar(CanonicalScalar::Null);
                            execution.value[0][0] = GraphValueRow::from_owned_values(values);
                        }
                        execution.rows.result_rows = execution.value[0].len() as u64;
                    }
                    execution
                }),
        )
    }
}

#[test]
fn both_factor_schemas_statistics_and_interruption_are_admitted_before_filtering() {
    for rows in [Vec::new(), vec![imported(int(1), int(1), 0)]] {
        let query = prepare(rows);
        assert!(matches!(
            execute(&query, &mut InvalidSource { kind: 0 }, policy()),
            Err(GqlQueryError::Source(
                GraphSetExecutionError::InvalidSourceStatistics { operand: 0 }
            ))
        ));
        for kind in [1, 3, 4] {
            assert!(matches!(
                execute(&query, &mut InvalidSource { kind }, policy()),
                Err(GqlQueryError::Source(GraphSetExecutionError::InputSchema {
                    operand: 0
                }))
            ));
        }
        assert_eq!(
            execute(&query, &mut InvalidSource { kind: 2 }, policy()),
            Err(GqlQueryError::Interrupted(77))
        );
    }
}
