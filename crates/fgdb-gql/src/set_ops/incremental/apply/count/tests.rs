//! Correlated scalar summary laws over full weighted bags.
//! Expected values are computed independently; the circuit uses native operators.

use super::*;
use crate::algebra::{GraphValueOrder, GraphValueRow, IntegerComparison};
use crate::row_aggregate::definition::operator::IncrementalGroupAggregate;
use crate::row_join::IncrementalRowJoin;
use crate::row_projection::IncrementalRowProjection;
use crate::row_window::IncrementalRowWindow;
use crate::spill_set::SpillSetAggregateRowError;
use crate::{
    GqlParameters, GraphAggregateError, GraphIntegerBinary, GraphIntegerError,
    GraphIntegerErrorKind, GraphSymbol, GraphSymbolKind, PreparedGraphSetText, PreparedGraphText,
};
use fgdb_delta_types::{LabelId, LimbLimit, PropertyKeyId, RelationId, ZSet, ZWeight};
use fgdb_types::VId;
use std::collections::{BTreeMap, BTreeSet};

const LIMBS: LimbLimit = LimbLimit::new(4);
type Bag = BTreeMap<GraphValueRow, i128>;

fn allow(_: GlaExecutionEvent) -> Result<(), usize> {
    Ok(())
}

fn scalar(value: Option<i64>) -> GraphValue {
    GraphValue::Scalar(value.map_or(CanonicalScalar::Null, CanonicalScalar::Int))
}

fn vertex(value: u128) -> GraphValue {
    GraphValue::Vertex(VId(value))
}

fn row(values: Vec<GraphValue>) -> GraphValueRow {
    GraphValueRow::from_owned_values(values)
}

fn bag(rows: impl IntoIterator<Item = (GraphValueRow, i128)>) -> ZSet<GraphValueRow> {
    ZSet::from_updates(
        rows.into_iter()
            .map(|(row, count)| (row, ZWeight::from_i128(count))),
        LIMBS,
        &mut |_| Ok::<_, ()>(()),
    )
    .unwrap()
}

fn plain(rows: &ZSet<GraphValueRow>) -> Bag {
    rows.iter()
        .map(|(row, count)| (row.clone(), count.to_i128().unwrap()))
        .collect()
}

fn difference(after: &ZSet<GraphValueRow>, before: &ZSet<GraphValueRow>) -> ZSet<GraphValueRow> {
    bag(plain(after)
        .into_iter()
        .chain(plain(before).into_iter().map(|(row, count)| (row, -count))))
}

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(RelationId(1))),
        (GraphSymbolKind::Label, "L") => Some(GraphSymbol::Label(LabelId(1))),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
        _ => None,
    }
}

fn outer() -> PreparedGraphSet {
    PreparedGraphText::prepare(
        "MATCH (n)-[:R]->(m) RETURN m.p AS token,n AS owner,m AS other",
        symbols,
    )
    .unwrap()
    .bind_parameters(&GqlParameters::new())
    .unwrap()
    .with_duplicates()
    .into()
}

fn right() -> PreparedGraphSet {
    PreparedGraphText::prepare("MATCH (a)-[:R]->(b) RETURN a,b", symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
        .with_duplicates()
        .into()
}

fn arguments(types: Vec<GraphSetColumnType>) -> PreparedGraphSet {
    PreparedGraphSet::arguments(
        (0..types.len())
            .map(|column| format!("arg{column}"))
            .collect(),
        types,
    )
    .unwrap()
}

fn base() -> PreparedGraphSet {
    let args = arguments(vec![GraphSetColumnType::Vertex; 2]);
    let source = right();
    let spec = RowJoinSpec::new(
        args.column_types(),
        source.column_types(),
        &[(0, 0), (1, 0)],
    )
    .unwrap();
    args.join(source, spec).unwrap()
}

fn counted(body: PreparedGraphSet) -> PreparedGraphSet {
    body.group_by(&[], &[GraphAggregate::count_rows("token")])
        .unwrap()
}

fn query() -> PreparedGraphSet {
    outer().apply(counted(base()), vec![1, 1]).unwrap()
}

struct Circuit {
    restrict: IncrementalRowJoin,
    group: IncrementalGroupAggregate<PreparedGraphSetAggregate>,
    join: IncrementalRowJoin,
    output: Vec<IncrementalRowProjection>,
}

impl Circuit {
    fn new(query: &PreparedGraphSet) -> Self {
        let plan = query
            .incremental_apply_count_with_control(&mut allow)
            .unwrap()
            .unwrap();
        assert_eq!(plan.restrict.kind(), RowJoinKind::Semi);
        assert_eq!(plan.join.kind(), RowJoinKind::Left);
        assert_eq!(plan.output.len(), 2);
        let schema = plan.group.input().column_types().to_vec();
        Self {
            restrict: IncrementalRowJoin::new(plan.restrict),
            group: IncrementalGroupAggregate::new(plan.group, &schema).unwrap(),
            join: IncrementalRowJoin::new(plan.join),
            output: plan
                .output
                .into_iter()
                .map(IncrementalRowProjection::new)
                .collect(),
        }
    }

    fn tick(
        &mut self,
        left: &ZSet<GraphValueRow>,
        right: &ZSet<GraphValueRow>,
    ) -> Result<ZSet<GraphValueRow>, SpillSetAggregateRowError<usize>> {
        let selected = self
            .restrict
            .prepare(right, left, LIMBS, None, &mut |_| Ok::<_, ()>(()))
            .unwrap();
        let grouped = self
            .group
            .prepare(selected.delta(), LIMBS, None, &mut |_| Ok::<_, ()>(()))
            .unwrap();
        // Use the host's checked native row conversion. This kernel harness
        // holds both prepared inputs until conversion succeeds, so a failed
        // activation can be retried without the host's failure-fence/rebuild.
        let mut values = Vec::new();
        for (row, weight) in grouped.delta().iter() {
            values.push((
                row.to_value_row_with_control(&mut allow)?,
                weight.checked_clone(LIMBS).unwrap(),
            ));
        }
        let values = ZSet::from_updates(values, LIMBS, &mut |_| Ok::<_, ()>(())).unwrap();
        let joined = self
            .join
            .prepare(left, &values, LIMBS, None, &mut |_| Ok::<_, ()>(()))
            .unwrap();
        let mut delta = joined
            .delta()
            .checked_clone(LIMBS, &mut |_| Ok::<_, ()>(()))
            .unwrap();
        for stage in &mut self.output {
            delta = stage
                .prepare(&delta, LIMBS, None, &mut |_| Ok::<_, ()>(()))
                .unwrap()
                .commit();
        }
        let _ = joined.commit();
        let _ = grouped.commit();
        let _ = selected.commit();
        Ok(delta)
    }

    fn rows(&self) -> &ZSet<GraphValueRow> {
        self.output.last().unwrap().rows()
    }
}

fn left_rows(mask: usize) -> ZSet<GraphValueRow> {
    bag([
        (row(vec![scalar(Some(3)), vertex(1), vertex(17)]), 2),
        (row(vec![scalar(Some(4)), vertex(1), vertex(18)]), 3),
    ]
    .into_iter()
    .enumerate()
    .filter_map(|(index, item)| (mask & (1 << index) != 0).then_some(item)))
}

fn right_rows(mask: usize) -> ZSet<GraphValueRow> {
    bag([
        (row(vec![vertex(1), vertex(8)]), 2),
        (row(vec![vertex(1), vertex(9)]), 3),
    ]
    .into_iter()
    .enumerate()
    .filter_map(|(index, item)| (mask & (1 << index) != 0).then_some(item)))
}

fn oracle(left: &ZSet<GraphValueRow>, right: &ZSet<GraphValueRow>, keys: &[(usize, usize)]) -> Bag {
    let mut output = Bag::new();
    for (outer, weight) in left.iter() {
        let count: i128 = right
            .iter()
            .filter(|(inner, _)| {
                keys.iter().all(|&(a, b)| {
                    !outer.values()[a].is_null()
                        && !inner.values()[b].is_null()
                        && outer.values()[a] == inner.values()[b]
                })
            })
            .map(|(_, count)| count.to_i128().unwrap())
            .sum();
        let mut cells = outer.values().to_vec();
        cells.push(scalar(Some(i64::try_from(count).unwrap())));
        output.insert(row(cells), weight.to_i128().unwrap());
    }
    output
}

#[test]
fn weighted_counts_match_all_256_simultaneous_bag_transitions() {
    let query = query();
    for before in 0..16 {
        for after in 0..16 {
            let mut circuit = Circuit::new(&query);
            let old_left = left_rows(before & 3);
            let old_right = right_rows(before >> 2);
            circuit.tick(&old_left, &old_right).unwrap();
            assert_eq!(
                plain(circuit.rows()),
                oracle(&old_left, &old_right, &[(1, 0)])
            );
            let next_left = left_rows(after & 3);
            let next_right = right_rows(after >> 2);
            let delta = circuit
                .tick(
                    &difference(&next_left, &old_left),
                    &difference(&next_right, &old_right),
                )
                .unwrap();
            assert_eq!(
                plain(circuit.rows()),
                oracle(&next_left, &next_right, &[(1, 0)])
            );
            let expected = bag(oracle(&next_left, &next_right, &[(1, 0)])
                .into_iter()
                .chain(
                    oracle(&old_left, &old_right, &[(1, 0)])
                        .into_iter()
                        .map(|(row, count)| (row, -count)),
                ));
            assert_eq!(delta, expected);
        }
    }
}

fn counted_value(distinct: bool) -> PreparedGraphSet {
    let projected = base()
        .project(
            vec![
                GraphSetProjection::new("unused", GraphSetValue::Value(scalar(Some(7)))),
                GraphSetProjection::new("target", GraphSetValue::Column(3)),
                GraphSetProjection::new("import", GraphSetValue::Column(0)),
            ],
            GraphSetQuantifier::All,
        )
        .unwrap()
        .project(
            vec![GraphSetProjection::new("renamed", GraphSetValue::Column(1))],
            GraphSetQuantifier::All,
        )
        .unwrap();
    let aggregate = if distinct {
        GraphAggregate::count_distinct("count", 0)
    } else {
        GraphAggregate::count("count", 0)
    };
    outer()
        .apply(projected.group_by(&[], &[aggregate]).unwrap(), vec![1, 1])
        .unwrap()
}

fn value_rows(mask: usize) -> ZSet<GraphValueRow> {
    bag([
        (row(vec![vertex(1), vertex(8)]), 2),
        (row(vec![vertex(1), vertex(9)]), 3),
        (row(vec![vertex(1), scalar(None)]), 4),
    ]
    .into_iter()
    .enumerate()
    .filter_map(|(index, item)| (mask & (1 << index) != 0).then_some(item)))
}

fn value_oracle(left: &ZSet<GraphValueRow>, right: &ZSet<GraphValueRow>, distinct: bool) -> Bag {
    let mut output = Bag::new();
    for (outer, weight) in left.iter() {
        let mut values = BTreeSet::new();
        let mut count = 0_i128;
        for (inner, occurrences) in right.iter() {
            if !outer.values()[1].is_null()
                && outer.values()[1] == inner.values()[0]
                && !inner.values()[1].is_null()
            {
                count += occurrences.to_i128().unwrap();
                values.insert(inner.values()[1].clone());
            }
        }
        let count = if distinct {
            i64::try_from(values.len()).unwrap()
        } else {
            i64::try_from(count).unwrap()
        };
        let mut cells = outer.values().to_vec();
        cells.push(scalar(Some(count)));
        output.insert(row(cells), weight.to_i128().unwrap());
    }
    output
}

#[test]
fn nonnull_and_distinct_counts_match_2048_weighted_transitions_through_reordered_aliases() {
    for distinct in [false, true] {
        let query = counted_value(distinct);
        let recipe = query
            .incremental_apply_count_with_control(&mut allow)
            .unwrap()
            .unwrap();
        let function = if distinct {
            GraphAggregateFunction::CountDistinct
        } else {
            GraphAggregateFunction::Count
        };
        assert_eq!(
            recipe.group.aggregate_specs().collect::<Vec<_>>(),
            [(function, Some(1))]
        );
        for before in 0..32 {
            for after in 0..32 {
                let old_left = left_rows(before & 3);
                let old_right = value_rows(before >> 2);
                let next_left = left_rows(after & 3);
                let next_right = value_rows(after >> 2);
                let old = value_oracle(&old_left, &old_right, distinct);
                let next = value_oracle(&next_left, &next_right, distinct);
                let mut circuit = Circuit::new(&query);
                circuit.tick(&old_left, &old_right).unwrap();
                assert_eq!(plain(circuit.rows()), old);
                let delta = circuit
                    .tick(
                        &difference(&next_left, &old_left),
                        &difference(&next_right, &old_right),
                    )
                    .unwrap();
                assert_eq!(plain(circuit.rows()), next);
                assert_eq!(
                    delta,
                    bag(next
                        .into_iter()
                        .chain(old.into_iter().map(|(row, weight)| (row, -weight))))
                );
            }
        }
    }
}

#[test]
fn distinct_count_retracts_only_at_last_support_and_never_converts_occurrence_count() {
    let mut circuit = Circuit::new(&counted_value(true));
    let left = left_rows(3);
    let huge = i128::from(i64::MAX) + 1;
    let right = bag([(row(vec![vertex(1), vertex(8)]), huge)]);
    circuit.tick(&left, &right).unwrap();
    assert_eq!(plain(circuit.rows()), value_oracle(&left, &right, true));
    let delta = circuit
        .tick(
            &ZSet::new(),
            &bag([(row(vec![vertex(1), vertex(8)]), 1 - huge)]),
        )
        .unwrap();
    assert!(delta.is_empty());
    let one = bag([(row(vec![vertex(1), vertex(8)]), 1)]);
    assert_eq!(plain(circuit.rows()), value_oracle(&left, &one, true));
    circuit
        .tick(&ZSet::new(), &difference(&ZSet::new(), &one))
        .unwrap();
    assert_eq!(
        plain(circuit.rows()),
        value_oracle(&left, &ZSet::new(), true)
    );
}

#[test]
fn repeated_rhs_keys_keep_every_outer_constraint_and_nulls_receive_zero() {
    let query = outer().apply(counted(base()), vec![1, 2]).unwrap();
    let plan = query
        .incremental_apply_count_with_control(&mut allow)
        .unwrap()
        .unwrap();
    assert_eq!(plan.restrict.keys(), &[(0, 1), (0, 2)]);
    assert_eq!(plan.group.group_key_columns(), &[0]);
    assert_eq!(plan.join.keys(), &[(1, 0), (2, 0)]);
    let left = bag([
        (row(vec![scalar(Some(7)), vertex(1), vertex(1)]), 2),
        (row(vec![scalar(Some(8)), vertex(1), vertex(2)]), 3),
        (row(vec![scalar(Some(9)), scalar(None), vertex(1)]), 4),
        (row(vec![scalar(Some(10)), vertex(2), vertex(2)]), 5),
        (row(vec![scalar(Some(11)), scalar(None), scalar(None)]), 6),
    ]);
    let right = bag([
        (row(vec![vertex(1), vertex(8)]), 3),
        (row(vec![vertex(2), vertex(9)]), 7),
        (row(vec![scalar(None), vertex(10)]), 9),
    ]);
    let mut circuit = Circuit::new(&query);
    circuit.tick(&left, &right).unwrap();
    assert_eq!(
        plain(circuit.rows()),
        oracle(&left, &right, &[(1, 0), (2, 0)])
    );
    assert_eq!(
        plain(circuit.restrict.rows()),
        Bag::from([
            (row(vec![vertex(1), vertex(8)]), 3),
            (row(vec![vertex(2), vertex(9)]), 7),
        ])
    );
    circuit
        .tick(&ZSet::new(), &difference(&ZSet::new(), &right))
        .unwrap();
    assert_eq!(
        plain(circuit.rows()),
        oracle(&left, &ZSet::new(), &[(1, 0), (2, 0)])
    );
}

#[test]
fn keyless_counts_include_null_rows_and_keep_one_zero_per_outer_occurrence() {
    let source = right()
        .project(
            vec![GraphSetProjection::new(
                "missing",
                GraphSetValue::Value(scalar(None)),
            )],
            GraphSetQuantifier::All,
        )
        .unwrap();
    let body = arguments(vec![]).cross_join(source).unwrap();
    let query = outer().apply(counted(body), vec![]).unwrap();
    let mut circuit = Circuit::new(&query);
    let left = left_rows(3);
    circuit.tick(&left, &ZSet::new()).unwrap();
    assert_eq!(plain(circuit.rows()), oracle(&left, &ZSet::new(), &[]));
    let right = bag([(row(vec![scalar(None)]), 7)]);
    circuit.tick(&ZSet::new(), &right).unwrap();
    assert_eq!(plain(circuit.rows()), oracle(&left, &right, &[]));
    circuit
        .tick(&difference(&ZSet::new(), &left), &ZSet::new())
        .unwrap();
    assert!(circuit.rows().is_empty());
    circuit.tick(&left, &ZSet::new()).unwrap();
    assert_eq!(plain(circuit.rows()), oracle(&left, &right, &[]));
    circuit
        .tick(&ZSet::new(), &difference(&ZSet::new(), &right))
        .unwrap();
    assert_eq!(plain(circuit.rows()), oracle(&left, &ZSet::new(), &[]));
}

#[test]
fn inactive_huge_counts_skip_conversion_and_staged_activation_is_retryable() {
    let huge = i128::from(i64::MAX) + 1;
    for global in [false, true] {
        let query = if global {
            outer().apply(counted(right()), vec![]).unwrap()
        } else {
            self::query()
        };
        let left = left_rows(1);
        let right = bag([(row(vec![vertex(1), vertex(8)]), huge)]);
        let mut circuit = Circuit::new(&query);
        circuit.tick(&ZSet::new(), &right).unwrap();
        assert!(circuit.rows().is_empty());
        assert!(circuit.restrict.rows().is_empty());
        let old_groups = circuit
            .group
            .rows()
            .checked_clone(LIMBS, &mut |_| Ok::<_, ()>(()))
            .unwrap();
        let error = circuit.tick(&left, &ZSet::new()).unwrap_err();
        assert!(matches!(
            error,
            SpillSetAggregateRowError::Value(GraphAggregateError::OutputExpression {
                error: GraphIntegerError {
                    kind: GraphIntegerErrorKind::Overflow,
                    ..
                },
                ..
            })
        ));
        assert!(circuit.rows().is_empty());
        assert!(circuit.restrict.rows().is_empty());
        assert_eq!(circuit.group.rows(), &old_groups);
        // The failed outer insertion was not published. Simultaneously bring
        // the previously stored huge RHS down to two and retry that insertion.
        circuit
            .tick(&left, &bag([(row(vec![vertex(1), vertex(8)]), 2 - huge)]))
            .unwrap();
        assert_eq!(
            plain(circuit.rows()),
            oracle(&left, &bag([(row(vec![vertex(1), vertex(8)]), 2)]), &[],)
        );

        let mut leaving = Circuit::new(&query);
        let one = bag([(row(vec![vertex(1), vertex(8)]), 1)]);
        leaving.tick(&left, &one).unwrap();
        // Final active keys are empty: an intermediate count of huge must
        // never be converted before the simultaneous outer retraction.
        leaving
            .tick(
                &difference(&ZSet::new(), &left),
                &bag([(row(vec![vertex(1), vertex(8)]), huge - 1)]),
            )
            .unwrap();
        assert!(leaving.rows().is_empty());
        assert!(leaving.restrict.rows().is_empty());
    }

    // A different active outer key must not make an uninvoked group visible.
    let mut circuit = Circuit::new(&query());
    let left = left_rows(1);
    let foreign = bag([(row(vec![vertex(2), vertex(8)]), huge)]);
    circuit.tick(&left, &foreign).unwrap();
    assert_eq!(plain(circuit.rows()), oracle(&left, &ZSet::new(), &[]));
    assert!(circuit.restrict.rows().is_empty());
}

fn equal(left: usize, right: usize) -> GraphSetPredicateOp {
    GraphSetPredicateOp::Compare {
        left: GraphSetOperand::Column(left),
        comparison: IntegerComparison::Equal,
        right: GraphSetOperand::Column(right),
    }
}

#[test]
fn count_profile_preserves_effects_and_never_treats_numeric_where_as_typed_keys() {
    let literal = base()
        .project(
            vec![GraphSetProjection::new(
                "unused",
                GraphSetValue::Value(scalar(Some(9))),
            )],
            GraphSetQuantifier::All,
        )
        .unwrap();
    let admitted = outer()
        .apply(
            counted(literal)
                .project(
                    vec![GraphSetProjection::new("renamed", GraphSetValue::Column(0))],
                    GraphSetQuantifier::All,
                )
                .unwrap(),
            vec![1, 1],
        )
        .unwrap();
    assert!(
        admitted
            .incremental_apply_count_with_control(&mut allow)
            .unwrap()
            .is_some()
    );

    let fallible = GraphSetValue::Integer(
        GraphIntegerExpression::prepare(&[
            GraphIntegerOp::Literal(Some(12)),
            GraphIntegerOp::Literal(Some(0)),
            GraphIntegerOp::Binary(GraphIntegerBinary::Divide),
        ])
        .unwrap(),
    );
    let args = arguments(vec![GraphSetColumnType::Vertex; 2]);
    let source = right();
    let on = RowJoinSpec::new(args.column_types(), source.column_types(), &[(0, 0)])
        .unwrap()
        .with_predicate(&[equal(0, 2)])
        .unwrap();
    let property = PreparedGraphText::prepare("MATCH (n) RETURN n.p AS payload", symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
        .with_duplicates();
    for body in [
        counted(base().with_page(0, Some(1))),
        counted(
            base()
                .with_order_by(&[GraphValueOrder::ascending(0)])
                .unwrap(),
        ),
        counted(
            base()
                .filter(&[GraphSetPredicateOp::Truth(Some(false))])
                .unwrap(),
        ),
        counted(args.join(source, on).unwrap()),
        counted(
            base()
                .project(
                    vec![GraphSetProjection::new("unused", fallible.clone())],
                    GraphSetQuantifier::All,
                )
                .unwrap(),
        ),
        counted(
            base()
                .project(
                    vec![GraphSetProjection::new("id", GraphSetValue::Column(2))],
                    GraphSetQuantifier::Distinct,
                )
                .unwrap(),
        ),
        base()
            .group_by(&[], &[GraphAggregate::count("outer_value", 0)])
            .unwrap(),
        base()
            .group_by(&[], &[GraphAggregate::count_distinct("outer_value", 1)])
            .unwrap(),
        base()
            .group_by(&[2], &[GraphAggregate::count_rows("rows")])
            .unwrap(),
        counted(base()).with_page(0, Some(0)),
        counted(base())
            .project(
                vec![GraphSetProjection::new("changed", fallible)],
                GraphSetQuantifier::All,
            )
            .unwrap(),
        counted(
            arguments(vec![GraphSetColumnType::Vertex; 2])
                .cross_join(property.into())
                .unwrap(),
        ),
    ] {
        let query = outer().apply(body, vec![1, 1]).unwrap();
        assert!(
            query
                .incremental_apply_count_with_control(&mut allow)
                .unwrap()
                .is_none()
        );
    }
    let score = right()
        .project(
            vec![GraphSetProjection::new(
                "score",
                GraphSetValue::Value(scalar(Some(10))),
            )],
            GraphSetQuantifier::All,
        )
        .unwrap();
    let numeric = arguments(vec![GraphSetColumnType::Stored])
        .cross_join(score)
        .unwrap()
        .filter(&[equal(0, 1)])
        .unwrap();
    let query = outer().apply(counted(numeric), vec![0]).unwrap();
    assert!(
        query
            .incremental_apply_count_with_control(&mut allow)
            .unwrap()
            .is_none()
    );
    let optional = outer().optional_apply(counted(base()), vec![1, 1]).unwrap();
    assert!(
        optional
            .incremental_apply_count_with_control(&mut allow)
            .unwrap()
            .is_none()
    );
}

fn admitted_apply(query: &PreparedGraphSet) -> bool {
    if query
        .incremental_apply_count_with_control(&mut allow)
        .unwrap()
        .is_some()
        || query
            .incremental_collected_count_with_control(&mut allow)
            .unwrap()
            .is_some()
    {
        return true;
    }
    match &query.node {
        SetNode::Scope(input)
        | SetNode::Project { input, .. }
        | SetNode::Filter { input, .. }
        | SetNode::Unwind { input, .. } => admitted_apply(input),
        _ => false,
    }
}

#[test]
fn native_count_and_returning_call_reach_the_count_recipe() {
    for text in [
        "MATCH (a:L) WITH a RETURN a,COUNT { MATCH (a)-[:R]->(b) } AS c",
        "MATCH (a:L) WITH DISTINCT a RETURN a,COUNT { MATCH (a)-[:R]->(b) } AS c",
        "MATCH (a:L) WITH a CALL (a) { MATCH (a)-[:R]->(b) RETURN count(*) AS c } RETURN a,c",
        "MATCH (a:L) WITH a CALL (a) { MATCH (a)-[:R]->(b) RETURN count(b) AS c } RETURN a,c",
        "MATCH (a:L) WITH a CALL (a) { MATCH (a)-[:R]->(b) RETURN count(DISTINCT b) AS c } RETURN a,c",
        "MATCH (a:L) WITH a CALL (a) { MATCH (a)-[r:R]->(b) RETURN count(DISTINCT r) AS c } RETURN a,c",
    ] {
        let query = PreparedGraphSetText::prepare(text, symbols)
            .unwrap()
            .bind_parameters(&GqlParameters::new())
            .unwrap();
        assert!(admitted_apply(&query), "{text}");
    }
}

#[test]
fn recipe_owns_exact_active_input_and_checkpoints_preserve_public_identity() {
    for query in [query(), counted_value(false), counted_value(true)] {
        check_recipe_identity_and_cancellation(query);
    }
}

fn check_recipe_identity_and_cancellation(query: PreparedGraphSet) {
    let original = query.canonical_bytes();
    let mut calls = 0;
    let plan = query
        .incremental_apply_count_with_control(&mut |_| {
            calls += 1;
            Ok::<_, usize>(())
        })
        .unwrap()
        .unwrap();
    assert_eq!(plan.left, &outer());
    assert_eq!(plan.right, &right());
    assert_eq!(
        plan.group.input(),
        &right().join(outer(), plan.restrict.clone()).unwrap(),
    );
    let output = plan.output.last().unwrap();
    assert_eq!(output.column_types(), query.column_types());
    assert_eq!(
        output.columns().collect::<Vec<_>>(),
        query
            .columns()
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
    );
    assert_eq!(
        output.column_types().last(),
        Some(&GraphSetColumnType::Scalar)
    );
    for stop in 1..=calls {
        let mut seen = 0;
        let result = query.incremental_apply_count_with_control(&mut |_| {
            seen += 1;
            if seen == stop { Err(stop) } else { Ok(()) }
        });
        assert!(matches!(result, Err(at) if at == stop));
        assert_eq!(seen, stop);
        assert_eq!(query.canonical_bytes(), original);
    }
}

#[test]
fn single_part_collected_count_reaches_native_recipe_and_maintains_zero_filled_counts() {
    for text in [
        "MATCH (a:L) RETURN a.p AS name, COUNT { MATCH (a)-[:R]->(:L) } AS c",
        "MATCH (a:L) RETURN a, COUNT { MATCH (a)-[:R]->(b) } + 1 AS c",
        "MATCH (a:L) RETURN DISTINCT a.p AS name, COUNT { MATCH (a)-[:R]->(b:L) } AS c",
        "MATCH (a:L) RETURN a.p AS name, COUNT { MATCH (a)-[:R]->(:L) } AS out_c, COUNT { MATCH (:L)-[:R]->(a) } AS in_c",
    ] {
        let query = PreparedGraphSetText::prepare(text, symbols)
            .unwrap()
            .bind_parameters(&GqlParameters::new())
            .unwrap();
        let original = query.canonical_bytes();
        let plan = query
            .incremental_collected_count_with_control(&mut allow)
            .unwrap()
            .unwrap_or_else(|| panic!("expected collected count recipe for: {text}"));
        assert_eq!(query.canonical_bytes(), original);
        assert_eq!(plan.restrict.kind(), RowJoinKind::Semi);
        assert_eq!(plan.join.kind(), RowJoinKind::Left);
        let output = plan.output.last().unwrap();
        assert_eq!(output.column_types(), query.column_types());
        assert_eq!(
            output.columns().collect::<Vec<_>>(),
            query
                .columns()
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
        );
    }

    // Raw list projection (not wrapped in COUNT/size) must return Ok(None).
    let raw_collect = PreparedGraphSetText::prepare(
        "MATCH (a:L) RETURN a.p AS name, [(a)-[:R]->(b:L) | b] AS xs",
        symbols,
    )
    .unwrap()
    .bind_parameters(&GqlParameters::new())
    .unwrap();
    assert!(
        raw_collect
            .incremental_collected_count_with_control(&mut allow)
            .unwrap()
            .is_none()
    );

    // Verify incremental Z-set ticks over the decorrelated circuit, including 0-fill.
    let single = PreparedGraphSetText::prepare(
        "MATCH (a:L) RETURN a.p AS name, COUNT { MATCH (a)-[:R]->(:L) } AS c",
        symbols,
    )
    .unwrap()
    .bind_parameters(&GqlParameters::new())
    .unwrap();
    let plan = single
        .incremental_collected_count_with_control(&mut allow)
        .unwrap()
        .unwrap();
    let schema = plan.group.input().column_types().to_vec();
    let mut circuit = Circuit {
        restrict: IncrementalRowJoin::new(plan.restrict),
        group: IncrementalGroupAggregate::new(plan.group, &schema).unwrap(),
        join: IncrementalRowJoin::new(plan.join),
        output: plan
            .output
            .into_iter()
            .map(IncrementalRowProjection::new)
            .collect(),
    };

    // Outer left has (name, hidden_vertex_a): vertex 1 ("alice"=10) and vertex 2 ("bob"=20).
    let d_left = bag([
        (row(vec![scalar(Some(10)), vertex(1)]), 1),
        (row(vec![scalar(Some(20)), vertex(2)]), 1),
    ]);
    // Inner right has correlated source vertex 1 with weight 2 (two outgoing :R edges),
    // and an uncorrelated vertex 99 that must be filtered out by Semi restriction.
    let d_right = bag([(row(vec![vertex(1)]), 2), (row(vec![vertex(99)]), 5)]);
    circuit.tick(&d_left, &d_right).unwrap();
    assert_eq!(
        plain(circuit.rows()),
        BTreeMap::from([
            (row(vec![scalar(Some(10)), scalar(Some(2))]), 1),
            (row(vec![scalar(Some(20)), scalar(Some(0))]), 1),
        ])
    );

    // Retract one outgoing edge from vertex 1 and add one outgoing edge to vertex 2.
    let d_right_2 = bag([(row(vec![vertex(1)]), -1), (row(vec![vertex(2)]), 1)]);
    circuit.tick(&bag([]), &d_right_2).unwrap();
    assert_eq!(
        plain(circuit.rows()),
        BTreeMap::from([
            (row(vec![scalar(Some(10)), scalar(Some(1))]), 1),
            (row(vec![scalar(Some(20)), scalar(Some(1))]), 1),
        ])
    );
}

struct FilteredCircuit {
    outer: IncrementalRowProjection,
    arguments: IncrementalRowProjection,
    join: IncrementalRowJoin,
    body: Vec<IncrementalRowProjection>,
    group: IncrementalGroupAggregate<PreparedGraphSetAggregate>,
    invocations: IncrementalRowProjection,
    complete: IncrementalRowJoin,
    summary: Vec<IncrementalRowProjection>,
    window: Option<IncrementalRowWindow>,
    restore: IncrementalRowJoin,
    output: Vec<IncrementalRowProjection>,
}

impl FilteredCircuit {
    fn new(query: &PreparedGraphSet) -> Self {
        let recipe = query
            .incremental_filtered_aggregate_with_control(&mut allow)
            .unwrap()
            .unwrap();
        let schema = recipe.group.input().column_types().to_vec();
        Self {
            outer: IncrementalRowProjection::new(recipe.outer),
            arguments: IncrementalRowProjection::new(recipe.arguments),
            join: IncrementalRowJoin::new(recipe.join),
            body: recipe
                .body
                .into_iter()
                .map(IncrementalRowProjection::new)
                .collect(),
            group: IncrementalGroupAggregate::new(recipe.group, &schema).unwrap(),
            invocations: IncrementalRowProjection::new(recipe.invocations),
            complete: IncrementalRowJoin::new(recipe.complete),
            summary: recipe
                .summary
                .into_iter()
                .map(IncrementalRowProjection::new)
                .collect(),
            window: recipe.window.map(IncrementalRowWindow::new),
            restore: IncrementalRowJoin::new(recipe.restore),
            output: recipe
                .output
                .into_iter()
                .map(IncrementalRowProjection::new)
                .collect(),
        }
    }

    // This kernel harness checks successful algebraic ticks, not host atomic
    // publication. Native integration tests own failed-stage publication laws.
    fn tick(
        &mut self,
        left: &ZSet<GraphValueRow>,
        right: &ZSet<GraphValueRow>,
    ) -> Result<ZSet<GraphValueRow>, ()> {
        let outer = self
            .outer
            .prepare(left, LIMBS, None, &mut |_| Ok::<_, ()>(()))
            .map_err(|_| ())?
            .commit();
        let arguments = self
            .arguments
            .prepare(&outer, LIMBS, None, &mut |_| Ok::<_, ()>(()))
            .map_err(|_| ())?
            .commit();
        let mut delta = self
            .join
            .prepare(&arguments, right, LIMBS, None, &mut |_| Ok::<_, ()>(()))
            .map_err(|_| ())?
            .commit();
        for stage in &mut self.body {
            delta = stage
                .prepare(&delta, LIMBS, None, &mut |_| Ok::<_, ()>(()))
                .map_err(|_| ())?
                .commit();
        }
        let grouped = self
            .group
            .prepare(&delta, LIMBS, None, &mut |_| Ok::<_, ()>(()))
            .map_err(|_| ())?;
        let mut values = Vec::new();
        for (row, weight) in grouped.delta().iter() {
            values.push((
                row.to_value_row_with_control(&mut allow).map_err(|_| ())?,
                weight.checked_clone(LIMBS).unwrap(),
            ));
        }
        let values = ZSet::from_updates(values, LIMBS, &mut |_| Ok::<_, ()>(())).unwrap();
        let _ = grouped.commit();
        let invocations = self
            .invocations
            .prepare(&arguments, LIMBS, None, &mut |_| Ok::<_, ()>(()))
            .map_err(|_| ())?
            .commit();
        delta = self
            .complete
            .prepare(&invocations, &values, LIMBS, None, &mut |_| Ok::<_, ()>(()))
            .map_err(|_| ())?
            .commit();
        for stage in &mut self.summary {
            delta = stage
                .prepare(&delta, LIMBS, None, &mut |_| Ok::<_, ()>(()))
                .map_err(|_| ())?
                .commit();
        }
        if let Some(window) = &mut self.window {
            delta = window
                .prepare(&delta, LIMBS, None, &mut |_| Ok::<_, ()>(()))
                .map_err(|_| ())?
                .commit();
        }
        delta = self
            .restore
            .prepare(&outer, &delta, LIMBS, None, &mut |_| Ok::<_, ()>(()))
            .map_err(|_| ())?
            .commit();
        for stage in &mut self.output {
            delta = stage
                .prepare(&delta, LIMBS, None, &mut |_| Ok::<_, ()>(()))
                .map_err(|_| ())?
                .commit();
        }
        Ok(delta)
    }

    fn rows(&self) -> &ZSet<GraphValueRow> {
        self.output.last().unwrap().rows()
    }
}

fn filtered_count(profile: usize) -> PreparedGraphSet {
    let arguments = arguments(vec![
        GraphSetColumnType::Vertex,
        GraphSetColumnType::Stored,
        GraphSetColumnType::Vertex,
    ])
    .project(
        vec![
            GraphSetProjection::new("threshold", GraphSetValue::Column(1)),
            GraphSetProjection::new("other", GraphSetValue::Column(0)),
            GraphSetProjection::new("again", GraphSetValue::Column(2)),
        ],
        GraphSetQuantifier::All,
    )
    .unwrap();
    let right = right();
    let on = RowJoinSpec::cross(arguments.column_types(), right.column_types())
        .unwrap()
        .with_predicate(&[
            GraphSetPredicateOp::Compare {
                left: GraphSetOperand::Column(4),
                comparison: IntegerComparison::NotEqual,
                right: GraphSetOperand::Column(1),
            },
            GraphSetPredicateOp::IsNull {
                operand: GraphSetOperand::Column(1),
                is_null: true,
            },
            GraphSetPredicateOp::Or,
        ])
        .unwrap();
    let body = arguments
        .join(right, on)
        .unwrap()
        .project(
            vec![
                GraphSetProjection::new("candidate", GraphSetValue::Column(4)),
                GraphSetProjection::new("threshold", GraphSetValue::Column(0)),
                GraphSetProjection::new("bound", GraphSetValue::Value(scalar(Some(2)))),
            ],
            GraphSetQuantifier::All,
        )
        .unwrap()
        .filter(&[
            GraphSetPredicateOp::Compare {
                left: GraphSetOperand::Column(1),
                comparison: IntegerComparison::Greater,
                right: GraphSetOperand::Column(2),
            },
            GraphSetPredicateOp::IsNull {
                operand: GraphSetOperand::Column(1),
                is_null: true,
            },
            GraphSetPredicateOp::Or,
        ])
        .unwrap();
    let count = match profile {
        0 => GraphAggregate::count_rows("degree"),
        1 => GraphAggregate::count("degree", 0),
        2 => GraphAggregate::count_distinct("degree", 0),
        3 => GraphAggregate::count("degree", 1),
        4 => GraphAggregate::count_distinct("degree", 1),
        _ => unreachable!(),
    };
    outer()
        .apply(body.group_by(&[], &[count]).unwrap(), vec![2, 0, 2])
        .unwrap()
}

fn filtered_left(mask: usize) -> ZSet<GraphValueRow> {
    bag([
        (0, row(vec![scalar(Some(3)), vertex(1), vertex(8)]), 2),
        (0, row(vec![scalar(Some(3)), vertex(2), vertex(8)]), 3),
        (0, row(vec![scalar(None), vertex(3), scalar(None)]), 4),
        (
            0,
            row(vec![
                GraphValue::Scalar(CanonicalScalar::Bool(false)),
                vertex(4),
                vertex(0),
            ]),
            5,
        ),
        (1, row(vec![scalar(Some(1)), vertex(5), scalar(None)]), 6),
        (1, row(vec![scalar(None), vertex(6), vertex(0)]), 7),
        (1, row(vec![scalar(Some(3)), vertex(7), scalar(None)]), 8),
    ]
    .into_iter()
    .filter_map(|(bit, row, weight)| (mask & (1 << bit) != 0).then_some((row, weight))))
}

fn filtered_right(mask: usize) -> ZSet<GraphValueRow> {
    bag([
        (0, row(vec![vertex(100), vertex(8)]), 2),
        (0, row(vec![vertex(101), vertex(9)]), 3),
        (1, row(vec![vertex(102), vertex(9)]), 4),
        (1, row(vec![vertex(103), scalar(None)]), 5),
        (1, row(vec![vertex(104), vertex(0)]), 7),
    ]
    .into_iter()
    .filter_map(|(bit, row, weight)| (mask & (1 << bit) != 0).then_some((row, weight))))
}

fn filtered_oracle(left: &ZSet<GraphValueRow>, right: &ZSet<GraphValueRow>, profile: usize) -> Bag {
    let mut output = Bag::new();
    for (outer, multiplicity) in left.iter() {
        let threshold = &outer.values()[0];
        let other = &outer.values()[2];
        let mut count = 0_i128;
        let mut values = BTreeSet::new();
        if threshold.is_null() || threshold == &scalar(Some(3)) {
            for (inner, weight) in right.iter() {
                let candidate = &inner.values()[1];
                if other.is_null() || (!candidate.is_null() && candidate != other) {
                    let value = if profile <= 2 { candidate } else { threshold };
                    if profile == 0 || !value.is_null() {
                        count += weight.to_i128().unwrap();
                        values.insert(value.clone());
                    }
                }
            }
        }
        if matches!(profile, 2 | 4) {
            count = values.len() as i128;
        }
        let mut cells = outer.values().to_vec();
        cells.push(scalar(Some(i64::try_from(count).unwrap())));
        output.insert(row(cells), multiplicity.to_i128().unwrap());
    }
    output
}

#[test]
fn filtered_counts_match_weighted_null_and_duplicate_invocations_across_every_transition() {
    for profile in 0..5 {
        let query = filtered_count(profile);
        for before in 0..16 {
            for after in 0..16 {
                let old_left = filtered_left(before & 3);
                let old_right = filtered_right(before >> 2);
                let next_left = filtered_left(after & 3);
                let next_right = filtered_right(after >> 2);
                let old = filtered_oracle(&old_left, &old_right, profile);
                let next = filtered_oracle(&next_left, &next_right, profile);
                let mut circuit = FilteredCircuit::new(&query);
                circuit.tick(&old_left, &old_right).unwrap();
                assert_eq!(
                    plain(circuit.rows()),
                    old,
                    "profile {profile}, before {before}"
                );
                let delta = circuit
                    .tick(
                        &difference(&next_left, &old_left),
                        &difference(&next_right, &old_right),
                    )
                    .unwrap();
                assert_eq!(
                    plain(circuit.rows()),
                    next,
                    "profile {profile}, after {after}"
                );
                assert_eq!(
                    delta,
                    bag(next
                        .into_iter()
                        .chain(old.into_iter().map(|(row, weight)| (row, -weight))))
                );
            }
        }
    }
}

#[test]
fn filtered_count_recipe_retains_the_complete_input_and_all_cancellation_points() {
    let query = filtered_count(2);
    let original = query.canonical_bytes();
    let mut calls = 0;
    let recipe = query
        .incremental_filtered_aggregate_with_control(&mut |_| {
            calls += 1;
            Ok::<_, usize>(())
        })
        .unwrap()
        .unwrap();
    assert_eq!(recipe.left, &outer());
    assert_eq!(recipe.right.as_ref(), &right());
    assert_eq!(recipe.arguments.quantifier(), GraphSetQuantifier::Distinct);
    assert_eq!(recipe.complete.kind(), RowJoinKind::Left);
    assert_eq!(recipe.restore.kind(), RowJoinKind::Inner);
    let mut input = recipe
        .left
        .clone()
        .project_incremental_spec(&recipe.outer)
        .unwrap()
        .project_incremental_spec(&recipe.arguments)
        .unwrap()
        .join(recipe.right.as_ref().clone(), recipe.join.clone())
        .unwrap();
    for step in &recipe.body {
        input = input.project_incremental_spec(step).unwrap();
    }
    assert_eq!(recipe.group.input(), &input);
    assert_eq!(
        recipe.group.aggregate_specs().collect::<Vec<_>>(),
        [(GraphAggregateFunction::CountDistinct, Some(0))]
    );
    let output = recipe.output.last().unwrap();
    assert_eq!(output.column_types(), query.column_types());
    assert_eq!(
        output.columns().collect::<Vec<_>>(),
        query
            .columns()
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
    );
    assert!(right().project_incremental_spec(&recipe.outer).is_none());
    let expanded = RowProjectionSpec::unwind(
        outer().column_types().to_vec(),
        outer().columns().to_vec(),
        "expanded".into(),
        GraphSetValue::List(vec![]),
    )
    .unwrap();
    assert_eq!(
        outer().project_incremental_spec(&expanded).unwrap(),
        outer()
            .unwind("expanded".into(), GraphSetValue::List(vec![]))
            .unwrap()
    );
    assert!(
        outer()
            .project_incremental_spec(
                &expanded
                    .with_filter(&[GraphSetPredicateOp::Truth(Some(false))])
                    .unwrap()
            )
            .is_none(),
        "filtered expansions need their own logical selection stage"
    );
    for stop in 1..=calls {
        let mut seen = 0;
        let result = query.incremental_filtered_aggregate_with_control(&mut |_| {
            seen += 1;
            if seen == stop { Err(stop) } else { Ok(()) }
        });
        assert!(matches!(result, Err(at) if at == stop));
        assert_eq!(seen, stop);
        assert_eq!(query.canonical_bytes(), original);
    }
}

fn filtered_aggregate_recipe(query: &PreparedGraphSet) -> Option<MaintainedFilteredAggregate<'_>> {
    if let Some(recipe) = query
        .incremental_filtered_aggregate_with_control(&mut allow)
        .unwrap()
    {
        return Some(recipe);
    }
    match &query.node {
        SetNode::Scope(input)
        | SetNode::Project { input, .. }
        | SetNode::Filter { input, .. }
        | SetNode::Unwind { input, .. } => filtered_aggregate_recipe(input),
        _ => None,
    }
}

fn has_filtered_aggregate(query: &PreparedGraphSet) -> bool {
    filtered_aggregate_recipe(query).is_some()
}

#[test]
fn native_filtered_counts_accept_imported_thresholds_and_static_property_sources() {
    for text in [
        "MATCH (a:L) WITH a RETURN a,COUNT { MATCH (b:L) WHERE b <> a RETURN b } AS degree",
        "MATCH (a:L) WITH a,a.p AS threshold CALL (a,threshold) { MATCH (a)-[:R]->(b:L) WHERE threshold > 1 AND b <> a RETURN count(*) AS degree } RETURN a,degree",
        "MATCH (a:L) WITH a,a.p AS threshold CALL (a,threshold) { MATCH (a)-[:R]->(b:L {p:1}) WHERE threshold > 1 AND b <> a RETURN count(*) AS degree } RETURN a,degree",
        "MATCH (a:L) WITH a,a.p AS threshold CALL (a,threshold) { MATCH (b:L {p:1}) WHERE threshold > 1 AND b <> a RETURN count(DISTINCT b) AS degree } RETURN a,degree",
        "MATCH (a:L) WITH a,a.p AS threshold CALL (threshold) { MATCH (b:L) WHERE threshold IS NULL OR threshold > 1 RETURN count(threshold) AS degree } RETURN a,degree",
        "MATCH (a:L) WITH a CALL (a) { MATCH (b:L) WHERE b <> a WITH DISTINCT b RETURN count(*) AS degree } RETURN a,degree",
        "MATCH (a:L) WITH a RETURN a,COUNT { MATCH (a)-[:R]->(b) RETURN DISTINCT b } AS degree",
    ] {
        let query = PreparedGraphSetText::prepare(text, symbols)
            .unwrap()
            .bind_parameters(&GqlParameters::new())
            .unwrap();
        assert!(has_filtered_aggregate(&query), "{text}");
    }
}

#[test]
fn restored_call_imports_keep_nulls_and_outer_multiplicity_across_distinct_and_group() {
    let frame = || arguments(vec![GraphSetColumnType::Vertex, GraphSetColumnType::Stored]);
    let token = || {
        frame()
            .project_ordered_output(vec![GraphSetProjection::new(
                "restored",
                GraphSetValue::Column(1),
            )])
            .unwrap()
    };
    let owner = frame()
        .project_ordered_output(vec![GraphSetProjection::new(
            "owner",
            GraphSetValue::Column(0),
        )])
        .unwrap();
    let source = right();
    let on = RowJoinSpec::new(owner.column_types(), source.column_types(), &[(0, 0)]).unwrap();
    let body = owner
        .join(source, on)
        .unwrap()
        .project(
            vec![GraphSetProjection::new("target", GraphSetValue::Column(2))],
            GraphSetQuantifier::Distinct,
        )
        .unwrap()
        .cross_join(token())
        .unwrap()
        .group_by(&[], &[GraphAggregate::count("n", 1)])
        .unwrap()
        .cross_join(token())
        .unwrap();
    let query = outer().apply(body, vec![1, 0]).unwrap();
    let mut circuit = FilteredCircuit::new(&query);
    let literal_false = GraphValue::Scalar(CanonicalScalar::Bool(false));
    let outer_row = |token: GraphValue| row(vec![token, vertex(1), vertex(8)]);
    let left = bag([
        (outer_row(scalar(None)), 2),
        (outer_row(literal_false.clone()), 3),
    ]);
    let edge = row(vec![vertex(1), vertex(9)]);
    let expected = |nonnull_count| {
        plain(&bag([
            (
                row(vec![
                    scalar(None),
                    vertex(1),
                    vertex(8),
                    scalar(Some(0)),
                    scalar(None),
                ]),
                2,
            ),
            (
                row(vec![
                    literal_false.clone(),
                    vertex(1),
                    vertex(8),
                    scalar(Some(nonnull_count)),
                    literal_false.clone(),
                ]),
                3,
            ),
        ]))
    };
    circuit.tick(&left, &bag([(edge.clone(), 2)])).unwrap();
    assert_eq!(plain(circuit.rows()), expected(1));
    circuit
        .tick(&ZSet::new(), &bag([(edge.clone(), -1)]))
        .unwrap();
    assert_eq!(plain(circuit.rows()), expected(1));
    circuit.tick(&ZSet::new(), &bag([(edge, -1)])).unwrap();
    assert_eq!(plain(circuit.rows()), expected(0));
}

#[test]
fn filtered_count_keeps_dead_expression_order_and_empty_outer_semantics() {
    let divide = GraphSetValue::Integer(
        GraphIntegerExpression::prepare(&[
            GraphIntegerOp::Literal(Some(12)),
            GraphIntegerOp::Literal(Some(0)),
            GraphIntegerOp::Binary(GraphIntegerBinary::Divide),
        ])
        .unwrap(),
    );
    for filter_first in [false, true] {
        let mut body = arguments(vec![]).cross_join(right()).unwrap();
        if filter_first {
            body = body
                .filter(&[GraphSetPredicateOp::Truth(Some(false))])
                .unwrap();
        }
        body = body
            .project(
                vec![GraphSetProjection::new("discarded", divide.clone())],
                GraphSetQuantifier::All,
            )
            .unwrap();
        if !filter_first {
            body = body
                .filter(&[GraphSetPredicateOp::Truth(Some(false))])
                .unwrap();
        }
        let query = outer().apply(counted(body), vec![]).unwrap();
        let mut circuit = FilteredCircuit::new(&query);
        circuit.tick(&ZSet::new(), &right_rows(3)).unwrap();
        assert!(circuit.rows().is_empty());
        let result = circuit.tick(&left_rows(3), &ZSet::new());
        if filter_first {
            result.unwrap();
            assert_eq!(
                plain(circuit.rows()),
                oracle(&left_rows(3), &ZSet::new(), &[])
            );
        } else {
            assert!(
                result.is_err(),
                "discarded division must execute before the later false filter"
            );
        }
    }
}

#[test]
fn filtered_counts_keep_conditional_source_effects_and_real_pages_refused() {
    for text in [
        "MATCH (a:L) WITH a,a.p AS threshold CALL (a,threshold) { MATCH (b:L) WHERE b.p > threshold RETURN count(*) AS degree } RETURN a,degree",
        "MATCH (a:L) WITH a CALL (a) { MATCH (b:L) WHERE b <> a RETURN count(b.p) AS degree } RETURN a,degree",
        "MATCH (a:L) WITH a CALL (a) { MATCH (b:L) WHERE b <> a WITH b LIMIT 1 RETURN count(*) AS degree } RETURN a,degree",
        "MATCH (a:L) WITH a CALL (a) { MATCH (b:L) WHERE b <> a WITH DISTINCT b LIMIT 1 RETURN count(*) AS degree } RETURN a,degree",
    ] {
        let query = PreparedGraphSetText::prepare(text, symbols)
            .unwrap()
            .bind_parameters(&GqlParameters::new())
            .unwrap();
        assert!(!has_filtered_aggregate(&query), "{text}");
    }
}

const SCALAR_FUNCTIONS: [GraphAggregateFunction; 6] = [
    GraphAggregateFunction::SumInt,
    GraphAggregateFunction::SumIntDistinct,
    GraphAggregateFunction::AverageInt,
    GraphAggregateFunction::AverageIntDistinct,
    GraphAggregateFunction::Min,
    GraphAggregateFunction::Max,
];

fn filtered_aggregate(profile: usize) -> PreparedGraphSet {
    let SetNode::Apply {
        left,
        right,
        imports,
        ..
    } = filtered_count(0).node
    else {
        unreachable!()
    };
    let SetNode::Aggregate(summary) = right.node else {
        unreachable!()
    };
    // Keep the actual filtered relation, then compute a variable argument
    // from both the invocation's nullable score and the RHS identity.
    let value = GraphSetValue::Integer(
        GraphIntegerExpression::prepare(&[
            GraphIntegerOp::IdentityColumn {
                column: 0,
                text: false,
            },
            GraphIntegerOp::Column(1),
            GraphIntegerOp::Binary(GraphIntegerBinary::Add),
        ])
        .unwrap(),
    );
    let body = summary
        .input()
        .clone()
        .project(
            vec![GraphSetProjection::new("value", value)],
            GraphSetQuantifier::All,
        )
        .unwrap();
    let aggregate = match profile {
        0 => GraphAggregate::sum_int("summary", 0),
        1 => GraphAggregate::sum_int_distinct("summary", 0),
        2 => GraphAggregate::average_int("summary", 0),
        3 => GraphAggregate::average_int_distinct("summary", 0),
        4 => GraphAggregate::min("summary", 0),
        5 => GraphAggregate::max("summary", 0),
        _ => unreachable!(),
    };
    (*left)
        .apply(body.group_by(&[], &[aggregate]).unwrap(), imports)
        .unwrap()
}

fn scalar_oracle(left: &ZSet<GraphValueRow>, right: &ZSet<GraphValueRow>, profile: usize) -> Bag {
    let mut output = Bag::new();
    for (outer, multiplicity) in left.iter() {
        let score = &outer.values()[0];
        let other = &outer.values()[2];
        let mut support = BTreeMap::<i64, i128>::new();
        // The fixture's WHERE admits score=3 and NULL. NULL arithmetic
        // contributes no nonnull argument; other scores fail that filter.
        if score == &scalar(Some(3)) {
            for (inner, weight) in right.iter() {
                let candidate = &inner.values()[1];
                if (other.is_null() || candidate != other)
                    && let GraphValue::Vertex(VId(id)) = candidate
                {
                    *support.entry(i64::try_from(*id).unwrap() + 3).or_default() +=
                        weight.to_i128().unwrap();
                }
            }
        }
        let value = if support.is_empty() {
            scalar(None)
        } else {
            let mut sum = 0_i128;
            let mut count = 0_i128;
            for (&value, &weight) in &support {
                let weight = if matches!(profile, 1 | 3) { 1 } else { weight };
                sum += i128::from(value) * weight;
                count += weight;
            }
            match profile {
                0 | 1 => scalar(Some(i64::try_from(sum).unwrap())),
                2 | 3 => GraphValue::Scalar(CanonicalScalar::Float(fgdb_types::CanonicalF64::new(
                    sum as f64 / count as f64,
                ))),
                4 => scalar(Some(*support.first_key_value().unwrap().0)),
                5 => scalar(Some(*support.last_key_value().unwrap().0)),
                _ => unreachable!(),
            }
        };
        let mut cells = outer.values().to_vec();
        cells.push(value);
        output.insert(row(cells), multiplicity.to_i128().unwrap());
    }
    output
}

#[test]
fn scalar_aggregates_match_1536_weighted_transitions_with_null_and_duplicate_invocations() {
    for profile in 0..SCALAR_FUNCTIONS.len() {
        let query = filtered_aggregate(profile);
        for before in 0..16 {
            for after in 0..16 {
                let old_left = filtered_left(before & 3);
                let old_right = filtered_right(before >> 2);
                let next_left = filtered_left(after & 3);
                let next_right = filtered_right(after >> 2);
                let old = scalar_oracle(&old_left, &old_right, profile);
                let next = scalar_oracle(&next_left, &next_right, profile);
                let mut circuit = FilteredCircuit::new(&query);
                circuit.tick(&old_left, &old_right).unwrap();
                assert_eq!(
                    plain(circuit.rows()),
                    old,
                    "profile {profile}, before {before}"
                );
                let delta = circuit
                    .tick(
                        &difference(&next_left, &old_left),
                        &difference(&next_right, &old_right),
                    )
                    .unwrap();
                assert_eq!(
                    plain(circuit.rows()),
                    next,
                    "profile {profile}, after {after}"
                );
                assert_eq!(
                    delta,
                    bag(next
                        .into_iter()
                        .chain(old.into_iter().map(|(row, weight)| (row, -weight))))
                );
            }
        }
    }
}

#[test]
fn scalar_extrema_restore_vertex_and_stored_domains_without_integer_coercion() {
    let left = bag([
        (
            row(vec![
                GraphValue::Scalar(CanonicalScalar::ucs_basic_text("zeta").unwrap()),
                vertex(1),
                vertex(8),
            ]),
            2,
        ),
        (
            row(vec![
                GraphValue::Scalar(CanonicalScalar::ucs_basic_text("alpha").unwrap()),
                vertex(2),
                vertex(9),
            ]),
            3,
        ),
        (row(vec![scalar(None), vertex(3), scalar(None)]), 4),
        (
            row(vec![
                GraphValue::Scalar(CanonicalScalar::Float(fgdb_types::CanonicalF64::new(1.5))),
                vertex(4),
                vertex(7),
            ]),
            5,
        ),
    ]);
    let right = right_rows(3);
    for (kind, imported) in [
        (GraphSetColumnType::Stored, 0),
        (GraphSetColumnType::Vertex, 2),
    ] {
        for maximum in [false, true] {
            let aggregate = if maximum {
                GraphAggregate::max("summary", 0)
            } else {
                GraphAggregate::min("summary", 0)
            };
            let body = arguments(vec![kind]).cross_join(self::right()).unwrap();
            let query = outer()
                .apply(body.group_by(&[], &[aggregate]).unwrap(), vec![imported])
                .unwrap();
            let expected = |present: bool| -> Bag {
                left.iter()
                    .map(|(outer, weight)| {
                        let mut cells = outer.values().to_vec();
                        cells.push(if present {
                            cells[imported].clone()
                        } else {
                            scalar(None)
                        });
                        (row(cells), weight.to_i128().unwrap())
                    })
                    .collect()
            };
            let mut circuit = FilteredCircuit::new(&query);
            circuit.tick(&left, &ZSet::new()).unwrap();
            assert_eq!(plain(circuit.rows()), expected(false));
            circuit.tick(&ZSet::new(), &right).unwrap();
            assert_eq!(plain(circuit.rows()), expected(true));
            circuit
                .tick(&ZSet::new(), &difference(&ZSet::new(), &right))
                .unwrap();
            assert_eq!(plain(circuit.rows()), expected(false));
        }
    }
}

#[test]
fn scalar_recipe_retains_complete_input_and_cancellation_while_count_fast_path_stays_narrow() {
    for (profile, function) in SCALAR_FUNCTIONS.into_iter().enumerate() {
        let query = filtered_aggregate(profile);
        let original = query.canonical_bytes();
        assert!(
            query
                .incremental_apply_count_with_control(&mut allow)
                .unwrap()
                .is_none()
        );
        let mut calls = 0;
        let recipe = query
            .incremental_filtered_aggregate_with_control(&mut |_| {
                calls += 1;
                Ok::<_, usize>(())
            })
            .unwrap()
            .unwrap();
        assert_eq!(
            recipe.group.aggregate_specs().collect::<Vec<_>>(),
            [(function, Some(0))]
        );
        let mut input = recipe
            .left
            .clone()
            .project_incremental_spec(&recipe.outer)
            .unwrap()
            .project_incremental_spec(&recipe.arguments)
            .unwrap()
            .join(recipe.right.as_ref().clone(), recipe.join.clone())
            .unwrap();
        for step in &recipe.body {
            input = input.project_incremental_spec(step).unwrap();
        }
        assert_eq!(recipe.group.input(), &input);
        assert_eq!(
            recipe.output.last().unwrap().column_types(),
            query.column_types()
        );
        for stop in 1..=calls {
            let mut seen = 0;
            let result = query.incremental_filtered_aggregate_with_control(&mut |_| {
                seen += 1;
                if seen == stop { Err(stop) } else { Ok(()) }
            });
            assert!(matches!(result, Err(at) if at == stop));
            assert_eq!(seen, stop);
            assert_eq!(query.canonical_bytes(), original);
        }
    }
}

#[test]
fn scalar_recipe_accepts_native_calls_and_keeps_conditional_effect_refusals() {
    let bind = |text: &str| {
        PreparedGraphSetText::prepare(text, symbols)
            .unwrap()
            .bind_parameters(&GqlParameters::new())
            .unwrap()
    };
    for (function, distinct) in [
        ("sum", false),
        ("sum", true),
        ("avg", false),
        ("avg", true),
        ("min", false),
        ("max", false),
    ] {
        let qualifier = if distinct { "DISTINCT " } else { "" };
        let call = format!("{function}({qualifier}id(b)+score)");
        let text = format!(
            "MATCH (a:L) WITH a,a.p AS score CALL (a,score) {{ MATCH (a)-[:R]->(b:L) WHERE b <> a RETURN {call} AS total }} RETURN a,total"
        );
        assert!(has_filtered_aggregate(&bind(&text)), "{text}");
    }
    let grouped = "MATCH (a:L) WITH a CALL (a) { MATCH (b:L) WHERE b <> a RETURN b,sum(1) AS total } RETURN a,total";
    let query = bind(grouped);
    let recipe = filtered_aggregate_recipe(&query).expect(grouped);
    // The two invocation slots precede the visible b key; the scalar 1
    // occupies its own aggregate input slot.
    assert_eq!(recipe.complete.keys(), &[(0, 0), (1, 1)]);
    assert_eq!(recipe.group.group_key_columns(), &[2, 3, 0]);
    assert_eq!(
        recipe.group.aggregate_specs().collect::<Vec<_>>(),
        [(GraphAggregateFunction::SumInt, Some(1))]
    );
    assert_eq!(
        recipe.group.input().column_types()[..2],
        [GraphSetColumnType::Vertex, GraphSetColumnType::Scalar]
    );
    for text in [
        "MATCH (a:L) WITH a,a.p AS threshold CALL (a,threshold) { MATCH (b:L) WHERE b.p > threshold RETURN sum(id(b)) AS total } RETURN a,total",
        "MATCH (a:L) WITH a CALL (a) { MATCH (b:L) WHERE b <> a RETURN avg(b.p) AS total } RETURN a,total",
        "MATCH (a:L) WITH a CALL (a) { MATCH (b:L) WHERE b <> a WITH b LIMIT 1 RETURN min(b) AS total } RETURN a,total",
        "MATCH (a:L) WITH a CALL (a) { MATCH (b:L) WHERE b <> a WITH DISTINCT b LIMIT 1 RETURN max(b) AS total } RETURN a,total",
    ] {
        assert!(!has_filtered_aggregate(&bind(text)), "{text}");
    }
}

fn mixed_summary(profile: usize) -> PreparedGraphSet {
    let arguments = arguments(vec![
        GraphSetColumnType::Stored,
        GraphSetColumnType::Vertex,
        GraphSetColumnType::Vertex,
    ]);
    let right = right();
    let on = RowJoinSpec::cross(arguments.column_types(), right.column_types())
        .unwrap()
        .with_predicate(&[
            GraphSetPredicateOp::Compare {
                left: GraphSetOperand::Column(4),
                comparison: IntegerComparison::NotEqual,
                right: GraphSetOperand::Column(1),
            },
            GraphSetPredicateOp::IsNull {
                operand: GraphSetOperand::Column(1),
                is_null: true,
            },
            GraphSetPredicateOp::Or,
        ])
        .unwrap();
    let value = GraphSetValue::Integer(
        GraphIntegerExpression::prepare(&[
            GraphIntegerOp::IdentityColumn {
                column: 4,
                text: false,
            },
            GraphIntegerOp::Column(0),
            GraphIntegerOp::Binary(GraphIntegerBinary::Add),
        ])
        .unwrap(),
    );
    let mut body = arguments
        .join(right, on)
        .unwrap()
        .filter(&[
            GraphSetPredicateOp::Compare {
                left: GraphSetOperand::Column(0),
                comparison: IntegerComparison::Greater,
                right: GraphSetOperand::Literal(
                    crate::GqlScalarParameter::new(CanonicalScalar::Int(2)).unwrap(),
                ),
            },
            GraphSetPredicateOp::IsNull {
                operand: GraphSetOperand::Column(0),
                is_null: true,
            },
            GraphSetPredicateOp::Or,
        ])
        .unwrap()
        .project(
            vec![
                GraphSetProjection::new("candidate", GraphSetValue::Column(4)),
                GraphSetProjection::new("value", value),
                GraphSetProjection::new("witness", GraphSetValue::Column(3)),
            ],
            if profile == 0 {
                GraphSetQuantifier::All
            } else {
                GraphSetQuantifier::Distinct
            },
        )
        .unwrap()
        .project(
            vec![
                GraphSetProjection::new("candidate", GraphSetValue::Column(0)),
                GraphSetProjection::new("value", GraphSetValue::Column(1)),
            ],
            if profile == 2 {
                GraphSetQuantifier::Distinct
            } else {
                GraphSetQuantifier::All
            },
        )
        .unwrap();
    if profile == 2 {
        body = body
            .filter(&[
                GraphSetPredicateOp::Compare {
                    left: GraphSetOperand::Column(1),
                    comparison: IntegerComparison::NotEqual,
                    right: GraphSetOperand::Literal(
                        crate::GqlScalarParameter::new(CanonicalScalar::Int(11)).unwrap(),
                    ),
                },
                GraphSetPredicateOp::IsNull {
                    operand: GraphSetOperand::Column(1),
                    is_null: true,
                },
                GraphSetPredicateOp::Or,
            ])
            .unwrap();
    }
    let summary = body
        .group_by(
            &[],
            &[
                GraphAggregate::count_rows("occurrences"),
                GraphAggregate::count("nonnull", 1),
                GraphAggregate::count_distinct("unique_values", 1),
                GraphAggregate::sum_int("total", 1),
                GraphAggregate::sum_int_distinct("unique_total", 1),
                GraphAggregate::average_int("mean", 1),
                GraphAggregate::average_int_distinct("unique_mean", 1),
                GraphAggregate::min("least", 0),
                GraphAggregate::max("greatest", 0),
            ],
        )
        .unwrap();
    // Complete multi-column aliases are transparent; every summary remains
    // in order and must be converted even if a later public stage hides it.
    let aliases = (0..summary.column_types().len())
        .map(|column| {
            GraphSetProjection::new(format!("metric{column}"), GraphSetValue::Column(column))
        })
        .collect();
    outer()
        .apply(
            summary.project(aliases, GraphSetQuantifier::All).unwrap(),
            vec![0, 2, 2],
        )
        .unwrap()
}

fn mixed_oracle(left: &ZSet<GraphValueRow>, right: &ZSet<GraphValueRow>, profile: usize) -> Bag {
    let mut output = Bag::new();
    for (outer, multiplicity) in left.iter() {
        let score = &outer.values()[0];
        let other = &outer.values()[2];
        let mut body = BTreeMap::<(GraphValue, GraphValue), i128>::new();
        if score.is_null() || score == &scalar(Some(3)) {
            for (inner, weight) in right.iter() {
                let candidate = &inner.values()[1];
                if other.is_null() || (!candidate.is_null() && candidate != other) {
                    let value = if score == &scalar(Some(3))
                        && let GraphValue::Vertex(VId(id)) = candidate
                    {
                        scalar(Some(i64::try_from(*id).unwrap() + 3))
                    } else {
                        scalar(None)
                    };
                    // The first DISTINCT keeps the independent source witness,
                    // so each original source tuple contributes exactly once.
                    let weight = if profile == 0 {
                        weight.to_i128().unwrap()
                    } else {
                        1
                    };
                    *body.entry((candidate.clone(), value)).or_default() += weight;
                }
            }
        }
        if profile == 2 {
            for weight in body.values_mut() {
                *weight = 1;
            }
            body.retain(|(_, value), _| value != &scalar(Some(11)));
        }
        let count: i128 = body.values().sum();
        let mut support = BTreeMap::<i64, i128>::new();
        let mut extrema = BTreeSet::new();
        for ((candidate, value), weight) in &body {
            if let GraphValue::Scalar(CanonicalScalar::Int(value)) = value {
                *support.entry(*value).or_default() += *weight;
            }
            if !candidate.is_null() {
                extrema.insert(candidate.clone());
            }
        }
        let nonnull: i128 = support.values().sum();
        let mut cells = outer.values().to_vec();
        cells.extend([
            scalar(Some(i64::try_from(count).unwrap())),
            scalar(Some(i64::try_from(nonnull).unwrap())),
            scalar(Some(i64::try_from(support.len()).unwrap())),
        ]);
        if support.is_empty() {
            cells.extend((0..4).map(|_| scalar(None)));
        } else {
            let sum: i128 = support
                .iter()
                .map(|(&value, &weight)| i128::from(value) * weight)
                .sum();
            let unique_sum: i128 = support.keys().map(|&value| i128::from(value)).sum();
            cells.extend([
                scalar(Some(i64::try_from(sum).unwrap())),
                scalar(Some(i64::try_from(unique_sum).unwrap())),
                GraphValue::Scalar(CanonicalScalar::Float(fgdb_types::CanonicalF64::new(
                    sum as f64 / nonnull as f64,
                ))),
                GraphValue::Scalar(CanonicalScalar::Float(fgdb_types::CanonicalF64::new(
                    unique_sum as f64 / support.len() as f64,
                ))),
            ]);
        }
        cells.push(extrema.first().cloned().unwrap_or_else(|| scalar(None)));
        cells.push(extrema.last().cloned().unwrap_or_else(|| scalar(None)));
        output.insert(row(cells), multiplicity.to_i128().unwrap());
    }
    output
}

#[test]
fn mixed_summary_tuples_match_768_weighted_transitions_through_distinct_barriers() {
    for profile in 0..3 {
        let query = mixed_summary(profile);
        let recipe = query
            .incremental_filtered_aggregate_with_control(&mut allow)
            .unwrap()
            .unwrap();
        assert_eq!(recipe.group.aggregate_specs().len(), 9);
        assert_eq!(
            recipe
                .body
                .iter()
                .filter(|stage| stage.quantifier() == GraphSetQuantifier::Distinct)
                .count(),
            profile
        );
        let SetNode::Apply { right, .. } = &query.node else {
            unreachable!()
        };
        assert!(count_summary(right, &mut allow).unwrap().is_none());
        assert!(
            query
                .incremental_apply_count_with_control(&mut allow)
                .unwrap()
                .is_none()
        );
        for before in 0..16 {
            for after in 0..16 {
                let old_left = filtered_left(before & 3);
                let old_right = filtered_right(before >> 2);
                let next_left = filtered_left(after & 3);
                let next_right = filtered_right(after >> 2);
                let old = mixed_oracle(&old_left, &old_right, profile);
                let next = mixed_oracle(&next_left, &next_right, profile);
                let mut circuit = FilteredCircuit::new(&query);
                circuit.tick(&old_left, &old_right).unwrap();
                assert_eq!(
                    plain(circuit.rows()),
                    old,
                    "profile {profile}, before {before}"
                );
                let delta = circuit
                    .tick(
                        &difference(&next_left, &old_left),
                        &difference(&next_right, &old_right),
                    )
                    .unwrap();
                assert_eq!(
                    plain(circuit.rows()),
                    next,
                    "profile {profile}, after {after}"
                );
                assert_eq!(
                    delta,
                    bag(next
                        .into_iter()
                        .chain(old.into_iter().map(|(row, weight)| (row, -weight))))
                );
            }
        }
    }
}

#[test]
fn mixed_summary_recipe_keeps_complete_input_schema_and_each_cancellation_point() {
    let query = mixed_summary(2);
    let original = query.canonical_bytes();
    let mut calls = 0;
    let recipe = query
        .incremental_filtered_aggregate_with_control(&mut |_| {
            calls += 1;
            Ok::<_, usize>(())
        })
        .unwrap()
        .unwrap();
    let mut input = recipe
        .left
        .clone()
        .project_incremental_spec(&recipe.outer)
        .unwrap()
        .project_incremental_spec(&recipe.arguments)
        .unwrap()
        .join(recipe.right.as_ref().clone(), recipe.join.clone())
        .unwrap();
    for stage in &recipe.body {
        input = input.project_incremental_spec(stage).unwrap();
    }
    assert_eq!(recipe.group.input(), &input);
    assert_eq!(
        recipe.output.last().unwrap().column_types(),
        query.column_types()
    );
    assert_eq!(
        recipe.output.last().unwrap().columns().collect::<Vec<_>>(),
        query
            .columns()
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
    );
    for stop in 1..=calls {
        let mut seen = 0;
        let result = query.incremental_filtered_aggregate_with_control(&mut |_| {
            seen += 1;
            if seen == stop { Err(stop) } else { Ok(()) }
        });
        assert!(matches!(result, Err(at) if at == stop));
        assert_eq!(seen, stop);
        assert_eq!(query.canonical_bytes(), original);
    }
}

#[test]
fn mixed_summary_calls_keep_reordered_and_selected_outputs_after_complete_conversion() {
    for text in [
        "MATCH (a:L) WITH a CALL (a) { MATCH (a)-[:R]->(b) RETURN count(*) AS n,sum(id(b)) AS total,avg(id(b)) AS mean,min(b) AS least } RETURN a,n,total,mean,least",
        "MATCH (a:L) WITH a CALL (a) { MATCH (b:L) WHERE b <> a WITH DISTINCT b RETURN count(*) AS n,max(b) AS greatest } RETURN a,n,greatest",
        "MATCH (a:L) WITH a,a.p AS score CALL (a,score) { MATCH (a)-[:R]->(b) WITH DISTINCT b,score WITH DISTINCT id(b)+score AS value WHERE value IS NULL OR value > 0 RETURN count(*) AS n,sum(value) AS total,avg(value) AS mean } RETURN a,n,total,mean",
    ] {
        let query = PreparedGraphSetText::prepare(text, symbols)
            .unwrap()
            .bind_parameters(&GqlParameters::new())
            .unwrap();
        assert!(has_filtered_aggregate(&query), "{text}");
    }
    for positions in [vec![0], (0..9).rev().collect()] {
        let SetNode::Apply {
            left,
            right,
            imports,
            ..
        } = mixed_summary(0).node
        else {
            unreachable!()
        };
        let projection = positions
            .iter()
            .copied()
            .enumerate()
            .map(|(out, column)| {
                GraphSetProjection::new(format!("export{out}"), GraphSetValue::Column(column))
            })
            .collect();
        let body = (*right)
            .project(projection, GraphSetQuantifier::All)
            .unwrap();
        let query = (*left).apply(body, imports).unwrap();
        let mut circuit = FilteredCircuit::new(&query);
        let left = filtered_left(3);
        let right = filtered_right(3);
        circuit.tick(&left, &right).unwrap();
        let expected: Bag = mixed_oracle(&left, &right, 0)
            .into_iter()
            .map(|(source, weight)| {
                let mut cells = source.values()[..3].to_vec();
                cells.extend(
                    positions
                        .iter()
                        .map(|column| source.values()[3 + column].clone()),
                );
                (row(cells), weight)
            })
            .collect();
        assert_eq!(plain(circuit.rows()), expected);
    }
}

fn transformed_summary(profile: usize) -> PreparedGraphSet {
    let SetNode::Apply {
        left,
        right,
        imports,
        ..
    } = filtered_count(0).node
    else {
        unreachable!()
    };
    let plus_one = GraphSetValue::Integer(
        GraphIntegerExpression::prepare(&[
            GraphIntegerOp::Column(0),
            GraphIntegerOp::Literal(Some(1)),
            GraphIntegerOp::Binary(GraphIntegerBinary::Add),
        ])
        .unwrap(),
    );
    let body = match profile {
        0 => (*right)
            .project(
                vec![GraphSetProjection::new("adjusted", plus_one)],
                GraphSetQuantifier::All,
            )
            .unwrap(),
        1 | 3 => {
            let (comparison, threshold) = if profile == 1 {
                (IntegerComparison::Greater, 3)
            } else {
                (IntegerComparison::Equal, 0)
            };
            (*right)
                .filter(&[GraphSetPredicateOp::Compare {
                    left: GraphSetOperand::Column(0),
                    comparison,
                    right: GraphSetOperand::Literal(
                        crate::GqlScalarParameter::new(CanonicalScalar::Int(threshold)).unwrap(),
                    ),
                }])
                .unwrap()
                .project(
                    vec![GraphSetProjection::new("adjusted", plus_one)],
                    GraphSetQuantifier::Distinct,
                )
                .unwrap()
        }
        2 => (*right)
            .project(
                vec![GraphSetProjection::new(
                    "absent",
                    GraphSetValue::Value(scalar(None)),
                )],
                GraphSetQuantifier::Distinct,
            )
            .unwrap()
            .project(
                vec![GraphSetProjection::new("again", GraphSetValue::Column(0))],
                GraphSetQuantifier::Distinct,
            )
            .unwrap(),
        _ => unreachable!(),
    };
    (*left).apply(body, imports).unwrap()
}

fn transformed_oracle(
    left: &ZSet<GraphValueRow>,
    right: &ZSet<GraphValueRow>,
    profile: usize,
) -> Bag {
    filtered_oracle(left, right, 0)
        .into_iter()
        .filter_map(|(source, weight)| {
            let GraphValue::Scalar(CanonicalScalar::Int(count)) = &source.values()[3] else {
                unreachable!()
            };
            if (profile == 1 && *count <= 3) || (profile == 3 && *count != 0) {
                return None;
            }
            let mut cells = source.values()[..3].to_vec();
            cells.push(if profile == 2 {
                scalar(None)
            } else {
                scalar(Some(count + 1))
            });
            Some((row(cells), weight))
        })
        .collect()
}

#[test]
fn post_summary_stages_preserve_all_1024_weighted_transitions_and_empty_global_groups() {
    for profile in 0..4 {
        let query = transformed_summary(profile);
        for before in 0..16 {
            for after in 0..16 {
                let old_left = filtered_left(before & 3);
                let old_right = filtered_right(before >> 2);
                let next_left = filtered_left(after & 3);
                let next_right = filtered_right(after >> 2);
                let old = transformed_oracle(&old_left, &old_right, profile);
                let next = transformed_oracle(&next_left, &next_right, profile);
                let mut circuit = FilteredCircuit::new(&query);
                circuit.tick(&old_left, &old_right).unwrap();
                assert_eq!(
                    plain(circuit.rows()),
                    old,
                    "profile {profile}, before {before}"
                );
                let delta = circuit
                    .tick(
                        &difference(&next_left, &old_left),
                        &difference(&next_right, &old_right),
                    )
                    .unwrap();
                assert_eq!(
                    plain(circuit.rows()),
                    next,
                    "profile {profile}, after {after}"
                );
                assert_eq!(
                    delta,
                    bag(next
                        .into_iter()
                        .chain(old.into_iter().map(|(row, weight)| (row, -weight))))
                );
            }
        }
    }
}

#[test]
fn post_summary_recipes_keep_schema_identity_and_every_construction_checkpoint() {
    for profile in 0..4 {
        let query = transformed_summary(profile);
        let bytes = query.canonical_bytes();
        let mut calls = 0;
        let recipe = query
            .incremental_filtered_aggregate_with_control(&mut |_| {
                calls += 1;
                Ok::<_, usize>(())
            })
            .unwrap()
            .unwrap();
        assert_eq!(recipe.complete.kind(), RowJoinKind::Left);
        assert_eq!(recipe.restore.kind(), RowJoinKind::Inner);
        assert!(
            recipe
                .output
                .iter()
                .all(|stage| stage.quantifier() == GraphSetQuantifier::All)
        );
        let output = recipe.output.last().unwrap();
        assert_eq!(output.column_types(), query.column_types());
        assert_eq!(
            output.columns().collect::<Vec<_>>(),
            query
                .columns()
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
        );
        for stop in 1..=calls {
            let mut seen = 0;
            let result = query.incremental_filtered_aggregate_with_control(&mut |_| {
                seen += 1;
                if seen == stop { Err(stop) } else { Ok(()) }
            });
            assert!(matches!(result, Err(at) if at == stop));
            assert_eq!(seen, stop);
            assert_eq!(query.canonical_bytes(), bytes);
        }
    }
}

#[test]
fn post_summary_errors_keep_stage_order_and_cannot_hide_checked_count_overflow() {
    let divide = GraphSetValue::Integer(
        GraphIntegerExpression::prepare(&[
            GraphIntegerOp::Literal(Some(12)),
            GraphIntegerOp::Literal(Some(0)),
            GraphIntegerOp::Binary(GraphIntegerBinary::Divide),
        ])
        .unwrap(),
    );
    for filter_first in [false, true] {
        let SetNode::Apply {
            left,
            right,
            imports,
            ..
        } = filtered_count(0).node
        else {
            unreachable!()
        };
        let mut body = *right;
        if filter_first {
            body = body
                .filter(&[GraphSetPredicateOp::Truth(Some(false))])
                .unwrap();
        }
        body = body
            .project(
                vec![
                    GraphSetProjection::new("n", GraphSetValue::Column(0)),
                    GraphSetProjection::new("discarded", divide.clone()),
                ],
                GraphSetQuantifier::Distinct,
            )
            .unwrap();
        if !filter_first {
            body = body
                .filter(&[GraphSetPredicateOp::Truth(Some(false))])
                .unwrap();
        }
        body = body
            .project(
                vec![GraphSetProjection::new("n", GraphSetValue::Column(0))],
                GraphSetQuantifier::All,
            )
            .unwrap();
        let query = (*left).apply(body, imports).unwrap();
        let mut circuit = FilteredCircuit::new(&query);
        circuit.tick(&ZSet::new(), &filtered_right(3)).unwrap();
        assert!(circuit.rows().is_empty());
        let activated = circuit.tick(&filtered_left(3), &ZSet::new());
        if filter_first {
            activated.unwrap();
            assert!(circuit.rows().is_empty());
        } else {
            assert!(
                activated.is_err(),
                "discarded expression executes before its later filter"
            );
        }
    }
    // Replacing a complete global count with NULL still evaluates and converts
    // the original count. A projection must not conceal a wide-domain failure.
    let mut circuit = FilteredCircuit::new(&transformed_summary(2));
    let left = bag([(row(vec![scalar(Some(3)), vertex(1), vertex(8)]), 1)]);
    let right = bag([(row(vec![vertex(100), vertex(9)]), i128::from(i64::MAX) + 1)]);
    assert!(circuit.tick(&left, &right).is_err());
}

#[test]
fn native_post_summary_expressions_and_filters_keep_reader_and_page_boundaries() {
    for text in [
        "MATCH (a:L) WITH a CALL (a) { MATCH (b:L) WHERE b <> a RETURN sum(1)+1 AS total } RETURN a,total",
        "MATCH (a:L) WITH a CALL (a) { MATCH (a)-[:R]->(b) WITH count(*) AS n WHERE n > 1 RETURN n+1 AS next } RETURN a,next",
        "MATCH (a:L) WITH a CALL (a) { MATCH (a)-[:R]->(b) WITH count(*) AS n,sum(id(b)) AS s WHERE n = 0 OR s > 1 RETURN coalesce(s,7) AS total } RETURN a,total",
        "MATCH (a:L) WITH a CALL (a) { MATCH (a)-[:R]->(b) WITH count(*) AS n WITH DISTINCT NULL AS gone RETURN gone } RETURN a,gone",
    ] {
        let query = PreparedGraphSetText::prepare(text, symbols)
            .unwrap()
            .bind_parameters(&GqlParameters::new())
            .unwrap();
        assert!(has_filtered_aggregate(&query), "{text}");
    }
    for text in [
        "MATCH (a:L) WITH a CALL (a) { MATCH (a)-[:R]->(b) WITH count(*) AS n LIMIT 1 RETURN n+1 AS next } RETURN a,next",
        "MATCH (a:L) WITH a CALL (a) { MATCH (a)-[:R]->(b) WITH min(b) AS selected RETURN selected.p AS p } RETURN a,p",
        "MATCH (a:L) WITH a CALL (a) { MATCH (a)-[:R]->(b) WITH count(*) AS n RETURN rand()+n AS random } RETURN a,random",
    ] {
        let query = PreparedGraphSetText::prepare(text, symbols)
            .unwrap()
            .bind_parameters(&GqlParameters::new())
            .unwrap();
        assert!(!has_filtered_aggregate(&query), "{text}");
    }
}

fn optional_transformed_summary(profile: usize) -> PreparedGraphSet {
    let SetNode::Apply {
        left,
        right,
        imports,
        ..
    } = transformed_summary(profile).node
    else {
        unreachable!()
    };
    (*left).optional_apply(*right, imports).unwrap()
}

fn optional_transformed_oracle(
    left: &ZSet<GraphValueRow>,
    right: &ZSet<GraphValueRow>,
    profile: usize,
) -> Bag {
    filtered_oracle(left, right, 0)
        .into_iter()
        .map(|(source, weight)| {
            let GraphValue::Scalar(CanonicalScalar::Int(count)) = source.values()[3] else {
                unreachable!()
            };
            let retained = match profile {
                1 => count > 3,
                3 => count == 0,
                _ => true,
            };
            let mut cells = source.values()[..3].to_vec();
            cells.push(if !retained || profile == 2 {
                scalar(None)
            } else {
                scalar(Some(count + 1))
            });
            (row(cells), weight)
        })
        .collect()
}

#[test]
fn optional_post_summary_null_extension_matches_1024_weighted_transitions() {
    for profile in 0..4 {
        let query = optional_transformed_summary(profile);
        let recipe = query
            .incremental_filtered_aggregate_with_control(&mut allow)
            .unwrap()
            .unwrap();
        assert_eq!(recipe.complete.kind(), RowJoinKind::Left);
        assert_eq!(recipe.restore.kind(), RowJoinKind::Left);
        for before in 0..16 {
            for after in 0..16 {
                let old_left = filtered_left(before & 3);
                let old_right = filtered_right(before >> 2);
                let next_left = filtered_left(after & 3);
                let next_right = filtered_right(after >> 2);
                let old = optional_transformed_oracle(&old_left, &old_right, profile);
                let next = optional_transformed_oracle(&next_left, &next_right, profile);
                let mut circuit = FilteredCircuit::new(&query);
                circuit.tick(&old_left, &old_right).unwrap();
                assert_eq!(
                    plain(circuit.rows()),
                    old,
                    "optional profile {profile}, before {before}"
                );
                let delta = circuit
                    .tick(
                        &difference(&next_left, &old_left),
                        &difference(&next_right, &old_right),
                    )
                    .unwrap();
                assert_eq!(
                    plain(circuit.rows()),
                    next,
                    "optional profile {profile}, after {after}"
                );
                assert_eq!(
                    delta,
                    bag(next
                        .into_iter()
                        .chain(old.into_iter().map(|(row, weight)| (row, -weight))))
                );
            }
        }
    }
}

#[test]
fn post_summary_work_domain_is_distinct_invocations_before_outer_restoration() {
    let left = bag([
        (row(vec![scalar(Some(3)), vertex(1), vertex(8)]), 1000),
        (row(vec![scalar(Some(3)), vertex(2), vertex(8)]), 2000),
    ]);
    let mut circuit = FilteredCircuit::new(&optional_transformed_summary(2));
    circuit.tick(&left, &filtered_right(3)).unwrap();
    // Owner is not imported. Both distinct outer rows therefore invoke one
    // summary, and their 3000 occurrences are restored only AFTER that tail.
    for stage in &circuit.summary {
        assert_eq!(stage.rows().len(), 1);
        assert_eq!(stage.rows().iter().next().unwrap().1.to_i128(), Some(1));
    }
    assert_eq!(
        plain(circuit.rows()),
        optional_transformed_oracle(&left, &filtered_right(3), 2)
    );
    circuit
        .tick(
            &bag([(row(vec![scalar(Some(3)), vertex(1), vertex(8)]), -999)]),
            &ZSet::new(),
        )
        .unwrap();
    for stage in &circuit.summary {
        assert_eq!(stage.rows().len(), 1);
        assert_eq!(stage.rows().iter().next().unwrap().1.to_i128(), Some(1));
    }
}

#[test]
fn optional_no_import_summary_completes_only_live_invocations_and_keeps_errors() {
    let body = counted(arguments(Vec::new()).cross_join(right()).unwrap());
    for filtered in [false, true] {
        let mut right = body.clone();
        if filtered {
            right = right
                .filter(&[GraphSetPredicateOp::Compare {
                    left: GraphSetOperand::Column(0),
                    comparison: IntegerComparison::Greater,
                    right: GraphSetOperand::Literal(
                        crate::GqlScalarParameter::new(CanonicalScalar::Int(0)).unwrap(),
                    ),
                }])
                .unwrap();
        }
        let query = outer().optional_apply(right, Vec::new()).unwrap();
        let mut circuit = FilteredCircuit::new(&query);
        circuit.tick(&ZSet::new(), &ZSet::new()).unwrap();
        assert!(circuit.rows().is_empty());
        let left = bag([(row(vec![scalar(None), vertex(1), vertex(8)]), 2)]);
        circuit.tick(&left, &ZSet::new()).unwrap();
        assert_eq!(
            plain(circuit.rows()),
            BTreeMap::from([(
                row(vec![
                    scalar(None),
                    vertex(1),
                    vertex(8),
                    scalar(if filtered { None } else { Some(0) })
                ]),
                2,
            )])
        );
        let right = bag([(row(vec![vertex(5), vertex(9)]), 1)]);
        circuit.tick(&ZSet::new(), &right).unwrap();
        assert_eq!(
            plain(circuit.rows()),
            BTreeMap::from([(
                row(vec![scalar(None), vertex(1), vertex(8), scalar(Some(1))]),
                2,
            )])
        );
    }
    let failure = GraphSetValue::Integer(
        GraphIntegerExpression::prepare(&[
            GraphIntegerOp::Literal(Some(12)),
            GraphIntegerOp::Column(0),
            GraphIntegerOp::Binary(GraphIntegerBinary::Divide),
        ])
        .unwrap(),
    );
    let body = body
        .project(
            vec![GraphSetProjection::new("bad", failure)],
            GraphSetQuantifier::All,
        )
        .unwrap();
    let mut circuit = FilteredCircuit::new(&outer().optional_apply(body, Vec::new()).unwrap());
    assert!(
        circuit.tick(&filtered_left(3), &ZSet::new()).is_err(),
        "optional NULL extension cannot swallow an invoked expression error"
    );
}

#[test]
fn repeated_imports_keep_the_existing_complete_summary_width_boundary() {
    let arguments = arguments(vec![GraphSetColumnType::Vertex; 12]);
    let right = right();
    let join = RowJoinSpec::new(arguments.column_types(), right.column_types(), &[(0, 0)]).unwrap();
    let names: Vec<_> = (0..8).map(|index| format!("count{index}")).collect();
    let summaries: Vec<_> = names
        .iter()
        .map(|name| GraphAggregate::count_rows(name))
        .collect();
    let body = arguments
        .join(right, join)
        .unwrap()
        .group_by(&[], &summaries)
        .unwrap();
    let query = outer().optional_apply(body, vec![1; 12]).unwrap();
    let recipe = query
        .incremental_filtered_aggregate_with_control(&mut allow)
        .unwrap()
        .unwrap();
    // The old outer restoration fits: 3 outer + 24 key + 24 group key + 8
    // summaries = 59 columns. Carrying the twelve imported argument copies
    // into completion instead would need 68 and incorrectly refuse this plan.
    assert_eq!(recipe.arguments.column_types().len(), 36);
    assert_eq!(recipe.invocations.column_types().len(), 24);
    assert_eq!(recipe.complete.width(), 56);
    assert_eq!(recipe.restore.width(), 59);
    let mut circuit = FilteredCircuit::new(&query);
    circuit
        .tick(
            &bag([(row(vec![scalar(Some(7)), vertex(5), vertex(8)]), 2)]),
            &bag([(row(vec![vertex(5), vertex(9)]), 3)]),
        )
        .unwrap();
    let mut expected = vec![scalar(Some(7)), vertex(5), vertex(8)];
    expected.extend((0..8).map(|_| scalar(Some(3))));
    assert_eq!(plain(circuit.rows()), BTreeMap::from([(row(expected), 2)]));
}

fn grouped_filtered_summary(optional: bool, profile: usize) -> PreparedGraphSet {
    let SetNode::Apply {
        left,
        right,
        imports,
        ..
    } = filtered_count(0).node
    else {
        unreachable!()
    };
    let SetNode::Aggregate(summary) = right.node else {
        unreachable!()
    };
    let mut body = summary.input().clone();
    if profile == 1 {
        body = body
            .project(
                (0..3)
                    .map(|column| {
                        GraphSetProjection::new(
                            format!("body{column}"),
                            GraphSetValue::Column(column),
                        )
                    })
                    .collect(),
                GraphSetQuantifier::Distinct,
            )
            .unwrap();
    }
    // Both user keys can be NULL. Neither is the private, non-null invocation
    // key; grouping and invocation restoration have distinct equality owners.
    let mut body = body
        .group_by(
            &[1, 0],
            &[
                GraphAggregate::count_rows("rows"),
                GraphAggregate::count("nonnull", 1),
                GraphAggregate::count_distinct("unique", 0),
                GraphAggregate::sum_int("total", 1),
                GraphAggregate::min("first", 0),
            ],
        )
        .unwrap();
    if profile >= 2 {
        body = body
            .project(
                vec![
                    GraphSetProjection::new("nonnull", GraphSetValue::Column(3)),
                    GraphSetProjection::new("total", GraphSetValue::Column(5)),
                ],
                if profile == 3 {
                    GraphSetQuantifier::Distinct
                } else {
                    GraphSetQuantifier::All
                },
            )
            .unwrap();
    }
    if optional {
        (*left).optional_apply(body, imports).unwrap()
    } else {
        (*left).apply(body, imports).unwrap()
    }
}

// Independent scalar/bag arithmetic. No compiler recipe, join kernel, group
// owner or batch executor participates in these expected values.
fn grouped_summary_oracle(
    left: &ZSet<GraphValueRow>,
    right: &ZSet<GraphValueRow>,
    optional: bool,
    profile: usize,
) -> Bag {
    let mut output = Bag::new();
    for (outer, multiplicity) in left.iter() {
        let threshold = &outer.values()[0];
        let other = &outer.values()[2];
        let mut groups = BTreeMap::<GraphValue, i128>::new();
        if threshold.is_null() || threshold == &scalar(Some(3)) {
            for (inner, weight) in right.iter() {
                let candidate = &inner.values()[1];
                if other.is_null() || (!candidate.is_null() && candidate != other) {
                    *groups.entry(candidate.clone()).or_default() += weight.to_i128().unwrap();
                }
            }
        }
        let mut selected = BTreeMap::<Vec<GraphValue>, i128>::new();
        for (key, count) in groups {
            let count = if profile == 1 { 1 } else { count };
            let nonnull = if threshold.is_null() { 0 } else { count };
            let total = if threshold.is_null() {
                scalar(None)
            } else {
                scalar(Some(i64::try_from(3 * count).unwrap()))
            };
            let cells = if profile >= 2 {
                vec![scalar(Some(i64::try_from(nonnull).unwrap())), total]
            } else {
                vec![
                    threshold.clone(),
                    key.clone(),
                    scalar(Some(i64::try_from(count).unwrap())),
                    scalar(Some(i64::try_from(nonnull).unwrap())),
                    scalar(Some(i64::from(!key.is_null()))),
                    total,
                    key,
                ]
            };
            let weight = selected.entry(cells).or_default();
            if profile == 3 {
                *weight = 1;
            } else {
                *weight += 1;
            }
        }
        if optional && selected.is_empty() {
            selected.insert(vec![scalar(None); if profile >= 2 { 2 } else { 7 }], 1);
        }
        for (values, weight) in selected {
            let mut cells = outer.values().to_vec();
            cells.extend(values);
            output.insert(row(cells), weight * multiplicity.to_i128().unwrap());
        }
    }
    output
}

#[test]
fn grouped_summaries_match_2048_weighted_transitions_with_null_keys_and_optional_calls() {
    for optional in [false, true] {
        for profile in 0..4 {
            let query = grouped_filtered_summary(optional, profile);
            let recipe = query
                .incremental_filtered_aggregate_with_control(&mut allow)
                .unwrap()
                .unwrap();
            assert_eq!(recipe.complete.kind(), RowJoinKind::Inner);
            assert_eq!(
                recipe.restore.kind(),
                if optional {
                    RowJoinKind::Left
                } else {
                    RowJoinKind::Inner
                }
            );
            for before in 0..16 {
                for after in 0..16 {
                    let old_left = filtered_left(before & 3);
                    let old_right = filtered_right(before >> 2);
                    let next_left = filtered_left(after & 3);
                    let next_right = filtered_right(after >> 2);
                    let old = grouped_summary_oracle(&old_left, &old_right, optional, profile);
                    let next = grouped_summary_oracle(&next_left, &next_right, optional, profile);
                    let mut circuit = FilteredCircuit::new(&query);
                    circuit.tick(&old_left, &old_right).unwrap();
                    assert_eq!(plain(circuit.rows()), old, "{optional}/{profile}/{before}");
                    let delta = circuit
                        .tick(
                            &difference(&next_left, &old_left),
                            &difference(&next_right, &old_right),
                        )
                        .unwrap();
                    assert_eq!(plain(circuit.rows()), next, "{optional}/{profile}/{after}");
                    assert_eq!(
                        delta,
                        bag(next
                            .into_iter()
                            .chain(old.into_iter().map(|(row, weight)| (row, -weight)))),
                    );
                }
            }
        }
    }
}

#[test]
fn grouped_no_import_calls_distinguish_real_null_groups_from_absent_groups() {
    let body = arguments(Vec::new())
        .cross_join(right())
        .unwrap()
        .group_by(&[1], &[GraphAggregate::count_rows("n")])
        .unwrap();
    let query = outer().optional_apply(body, Vec::new()).unwrap();
    // A grouped COUNT must not enter the scalar count fast path.
    assert!(
        query
            .incremental_apply_count_with_control(&mut allow)
            .unwrap()
            .is_none()
    );
    let mut circuit = FilteredCircuit::new(&query);
    let left = bag([(row(vec![scalar(None), vertex(1), vertex(8)]), 2)]);
    circuit.tick(&left, &ZSet::new()).unwrap();
    let absent = BTreeMap::from([(
        row(vec![
            scalar(None),
            vertex(1),
            vertex(8),
            scalar(None),
            scalar(None),
        ]),
        2,
    )]);
    assert_eq!(plain(circuit.rows()), absent);
    let right = bag([(row(vec![vertex(7), scalar(None)]), 5)]);
    circuit.tick(&ZSet::new(), &right).unwrap();
    assert_eq!(
        plain(circuit.rows()),
        BTreeMap::from([(
            row(vec![
                scalar(None),
                vertex(1),
                vertex(8),
                scalar(None),
                scalar(Some(5))
            ]),
            2,
        )]),
    );
    circuit
        .tick(&ZSet::new(), &difference(&ZSet::new(), &right))
        .unwrap();
    assert_eq!(plain(circuit.rows()), absent);
    circuit
        .tick(&difference(&ZSet::new(), &left), &right)
        .unwrap();
    assert!(circuit.rows().is_empty());
}

#[test]
fn native_grouped_recipes_preserve_definition_cancellation_and_source_boundaries() {
    for text in [
        "MATCH (a:L) WITH a CALL (a) { MATCH (a)-[:R]->(b:L) RETURN b AS bucket,count(*) AS degree } RETURN a,bucket,degree",
        "MATCH (a:L) WITH a CALL (a) { MATCH (a)-[:R]->(b) RETURN b AS bucket,count(*) AS degree LIMIT 1 } RETURN a,bucket,degree",
        "MATCH (a:L) WITH a OPTIONAL CALL (a) { MATCH (a)-[:R]->(b:L {p:1}) RETURN id(b)%2 AS bucket,count(*) AS degree,sum(id(b)) AS total } RETURN a,bucket,degree,total",
        "MATCH (a:L) WITH a,a.p AS threshold CALL (a,threshold) { MATCH (a)-[:R]->(b) WITH b,threshold,count(*) AS n WHERE n>1 RETURN DISTINCT threshold AS category,n+1 AS degree } RETURN a,category,degree",
    ] {
        let query = PreparedGraphSetText::prepare(text, symbols)
            .unwrap()
            .bind_parameters(&GqlParameters::new())
            .unwrap();
        assert!(has_filtered_aggregate(&query), "{text}");
    }
    for text in [
        "MATCH (a:L) WITH a OPTIONAL CALL (a) { MATCH (a)-[:R]->(b) RETURN b.p AS bucket,count(*) AS degree } RETURN a,bucket,degree",
    ] {
        let query = PreparedGraphSetText::prepare(text, symbols)
            .unwrap()
            .bind_parameters(&GqlParameters::new())
            .unwrap();
        assert!(!has_filtered_aggregate(&query), "{text}");
    }
    let query = grouped_filtered_summary(true, 3);
    let original = query.canonical_bytes();
    let mut calls = 0;
    query
        .incremental_filtered_aggregate_with_control(&mut |_| {
            calls += 1;
            Ok::<_, usize>(())
        })
        .unwrap()
        .unwrap();
    assert!(calls > 0);
    for stop in 1..=calls {
        let mut seen = 0;
        assert!(matches!(
            query.incremental_filtered_aggregate_with_control(&mut |_| {
                seen += 1;
                if seen == stop { Err(stop) } else { Ok(()) }
            }),
            Err(at) if at == stop
        ));
        assert_eq!(seen, stop);
        assert_eq!(query.canonical_bytes(), original);
    }
}

fn unwind_summary(profile: usize, optional: bool) -> PreparedGraphSet {
    let is_null = GraphSetValue::Integer(
        GraphIntegerExpression::prepare_scalar(&[
            GraphIntegerOp::PresenceColumn(0),
            GraphIntegerOp::IsNull(true),
        ])
        .unwrap(),
    );
    let next = GraphSetValue::Integer(
        GraphIntegerExpression::prepare(&[
            GraphIntegerOp::Column(0),
            GraphIntegerOp::Literal(Some(1)),
            GraphIntegerOp::Binary(GraphIntegerBinary::Add),
        ])
        .unwrap(),
    );
    let members = GraphSetValue::Case {
        arms: vec![(is_null, GraphSetValue::List(vec![]))],
        otherwise: Some(Box::new(GraphSetValue::List(vec![
            GraphSetValue::Column(0),
            GraphSetValue::Column(0),
            next,
        ]))),
    };
    let bucket = GraphSetValue::Integer(
        GraphIntegerExpression::prepare(&[
            GraphIntegerOp::Column(1),
            GraphIntegerOp::Literal(Some(2)),
            GraphIntegerOp::Binary(GraphIntegerBinary::Remainder),
        ])
        .unwrap(),
    );
    let body = arguments(vec![GraphSetColumnType::Stored])
        .unwind("member".into(), members)
        .unwrap()
        .project(
            vec![
                GraphSetProjection::new("bucket", bucket),
                GraphSetProjection::new("value", GraphSetValue::Column(1)),
            ],
            if profile & 2 != 0 {
                GraphSetQuantifier::Distinct
            } else {
                GraphSetQuantifier::All
            },
        )
        .unwrap();
    let grouped = profile & 1 != 0;
    let body = body
        .group_by(
            if grouped { &[0] } else { &[] },
            &[
                GraphAggregate::count_rows("n"),
                GraphAggregate::sum_int("total", 1),
            ],
        )
        .unwrap();
    let count_column = usize::from(grouped);
    let expanded = body.column_types().len();
    let body = body
        .unwind(
            "again".into(),
            GraphSetValue::List(vec![
                GraphSetValue::Column(count_column),
                GraphSetValue::Column(count_column),
            ]),
        )
        .unwrap()
        .project(
            vec![GraphSetProjection::new(
                "answer",
                GraphSetValue::Column(expanded),
            )],
            if profile & 4 != 0 {
                GraphSetQuantifier::Distinct
            } else {
                GraphSetQuantifier::All
            },
        )
        .unwrap();
    if optional {
        outer().optional_apply(body, vec![0]).unwrap()
    } else {
        outer().apply(body, vec![0]).unwrap()
    }
}

fn unwind_outer(mask: usize) -> ZSet<GraphValueRow> {
    bag([
        (0, row(vec![scalar(Some(2)), vertex(1), vertex(8)]), 2),
        (1, row(vec![scalar(Some(2)), vertex(2), vertex(8)]), 3),
        (2, row(vec![scalar(Some(3)), vertex(3), vertex(9)]), 4),
        (3, row(vec![scalar(None), vertex(4), vertex(0)]), 5),
    ]
    .into_iter()
    .filter_map(|(bit, row, weight)| (mask & (1 << bit) != 0).then_some((row, weight))))
}

// Expand literal occurrences, count each user's bucket, expand its tail list,
// then deduplicate exported values only when requested. This oracle has no
// invocation-key encoding, incremental operator or recipe-derived layout.
fn unwind_summary_oracle(left: &ZSet<GraphValueRow>, profile: usize, optional: bool) -> Bag {
    let mut output = Bag::new();
    for (outer, multiplicity) in left.iter() {
        let mut members = match outer.values()[0] {
            GraphValue::Scalar(CanonicalScalar::Int(score)) => vec![score, score, score + 1],
            _ => vec![],
        };
        if profile & 2 != 0 {
            members.sort_unstable();
            members.dedup();
        }
        let mut counts = BTreeMap::new();
        if profile & 1 == 0 {
            counts.insert(0, 0_i64);
        }
        for value in members {
            *counts
                .entry(if profile & 1 != 0 { value % 2 } else { 0 })
                .or_default() += 1;
        }
        let mut answers: Vec<_> = counts
            .into_values()
            .flat_map(|count| [scalar(Some(count)), scalar(Some(count))])
            .collect();
        if profile & 4 != 0 {
            answers.sort();
            answers.dedup();
        }
        if optional && answers.is_empty() {
            answers.push(scalar(None));
        }
        for answer in answers {
            let mut cells = outer.values().to_vec();
            cells.push(answer);
            *output.entry(row(cells)).or_default() += multiplicity.to_i128().unwrap();
        }
    }
    output
}

#[test]
fn correlated_unwind_body_and_summary_match_2048_weighted_transitions() {
    let unit = bag([(row(vec![]), 1)]);
    for profile in 0..8 {
        let query = unwind_summary(profile, true);
        let recipe = query
            .incremental_filtered_aggregate_with_control(&mut allow)
            .unwrap()
            .unwrap();
        assert!(matches!(&recipe.right, Cow::Owned(_)));
        assert_eq!(recipe.right.as_ref(), &PreparedGraphSet::singleton());
        for before in 0..16 {
            for after in 0..16 {
                let old = unwind_outer(before);
                let next = unwind_outer(after);
                let mut circuit = FilteredCircuit::new(&query);
                circuit.tick(&old, &unit).unwrap();
                assert_eq!(
                    plain(circuit.rows()),
                    unwind_summary_oracle(&old, profile, true)
                );
                let delta = circuit
                    .tick(&difference(&next, &old), &ZSet::new())
                    .unwrap();
                let expected = unwind_summary_oracle(&next, profile, true);
                assert_eq!(
                    plain(circuit.rows()),
                    expected,
                    "profile {profile}: {before}->{after}"
                );
                let mut integrated = bag(unwind_summary_oracle(&old, profile, true));
                integrated
                    .integrate(&delta, LIMBS, &mut |_| Ok::<_, ()>(()))
                    .unwrap();
                assert_eq!(plain(&integrated), expected, "exact derivative");
            }
        }
        let mut inner = FilteredCircuit::new(&unwind_summary(profile, false));
        inner.tick(&unwind_outer(15), &unit).unwrap();
        assert_eq!(
            plain(inner.rows()),
            unwind_summary_oracle(&unwind_outer(15), profile, false)
        );
    }
}

#[test]
fn source_free_ranges_keep_unit_activation_and_reconstruct_the_original_members() {
    for (start, end, step, count, sum) in [
        (3, -1, Some(-2), 3, 3),
        (0, 4, None, 5, 10),
        (2, 0, None, 0, 0),
    ] {
        let range = GraphSetValue::Range {
            start: Box::new(GraphSetValue::Value(scalar(Some(start)))),
            end: Box::new(GraphSetValue::Value(scalar(Some(end)))),
            step: step.map(|value| Box::new(GraphSetValue::Value(scalar(Some(value))))),
        };
        let source = PreparedGraphSet::singleton();
        let expanded = source
            .clone()
            .unwind("member".into(), range.clone())
            .unwrap();
        let spec = RowProjectionSpec::unwind(vec![], vec![], "member".into(), range).unwrap();
        assert_eq!(source.project_incremental_spec(&spec).unwrap(), expanded);
        let body = expanded
            .group_by(
                &[],
                &[
                    GraphAggregate::count_rows("n"),
                    GraphAggregate::sum_int("total", 0),
                ],
            )
            .unwrap();
        let query = outer().apply(body, vec![]).unwrap();
        let recipe = query
            .incremental_filtered_aggregate_with_control(&mut allow)
            .unwrap()
            .unwrap();
        assert!(matches!(&recipe.right, Cow::Borrowed(_)));
        let mut active = recipe
            .left
            .clone()
            .project_incremental_spec(&recipe.outer)
            .unwrap()
            .project_incremental_spec(&recipe.arguments)
            .unwrap()
            .join(recipe.right.as_ref().clone(), recipe.join.clone())
            .unwrap();
        for stage in &recipe.body {
            active = active.project_incremental_spec(stage).unwrap();
        }
        assert_eq!(
            recipe.group.input(),
            &active,
            "the actual expanded logical input is retained"
        );
        let mut circuit = FilteredCircuit::new(&query);
        circuit
            .tick(&ZSet::new(), &bag([(row(vec![]), 1)]))
            .unwrap();
        assert!(
            circuit.rows().is_empty(),
            "a unit does not invent an invocation"
        );
        let left = unwind_outer(7);
        circuit.tick(&left, &ZSet::new()).unwrap();
        let expected = left
            .iter()
            .map(|(outer, weight)| {
                let mut cells = outer.values().to_vec();
                cells.extend([
                    scalar(Some(count)),
                    if count == 0 {
                        scalar(None)
                    } else {
                        scalar(Some(sum))
                    },
                ]);
                (row(cells), weight.to_i128().unwrap())
            })
            .collect::<Bag>();
        assert_eq!(plain(circuit.rows()), expected);
        circuit
            .tick(&difference(&ZSet::new(), &left), &ZSet::new())
            .unwrap();
        assert!(
            circuit.rows().is_empty(),
            "the last outer retraction deactivates the unit body"
        );
    }
}

#[test]
fn correlated_unwind_preserves_lazy_and_discarded_effect_boundaries() {
    let divide = GraphSetValue::Integer(
        GraphIntegerExpression::prepare(&[
            GraphIntegerOp::Literal(Some(1)),
            GraphIntegerOp::Literal(Some(0)),
            GraphIntegerOp::Binary(GraphIntegerBinary::Divide),
        ])
        .unwrap(),
    );
    for arguments_only in [false, true] {
        for after_summary in [false, true] {
            for filter_first in [false, true] {
                let mut body = if arguments_only {
                    arguments(vec![GraphSetColumnType::Stored])
                } else {
                    PreparedGraphSet::singleton()
                };
                if after_summary {
                    body = counted(body);
                }
                if filter_first {
                    body = body
                        .filter(&[GraphSetPredicateOp::Truth(Some(false))])
                        .unwrap();
                }
                body = body
                    .unwind("failed".into(), GraphSetValue::List(vec![divide.clone()]))
                    .unwrap();
                if !filter_first {
                    body = body
                        .filter(&[GraphSetPredicateOp::Truth(Some(false))])
                        .unwrap();
                }
                if !after_summary {
                    body = counted(body);
                }
                let query = outer()
                    .apply(body, if arguments_only { vec![0] } else { vec![] })
                    .unwrap();
                let mut circuit = FilteredCircuit::new(&query);
                circuit
                    .tick(&ZSet::new(), &bag([(row(vec![]), 1)]))
                    .unwrap();
                assert!(
                    circuit.rows().is_empty(),
                    "no eager evaluation for inactive invocations"
                );
                let result = circuit.tick(&unwind_outer(1), &ZSet::new());
                assert_eq!(
                    result.is_ok(),
                    filter_first,
                    "ordered body/tail effect: {arguments_only}/{after_summary}/{filter_first}"
                );
            }
        }
    }
    for value in [
        GraphSetValue::Value(scalar(Some(1))),
        GraphSetValue::Range {
            start: Box::new(GraphSetValue::Value(scalar(Some(0)))),
            end: Box::new(GraphSetValue::Value(scalar(Some(2)))),
            step: Some(Box::new(GraphSetValue::Value(scalar(Some(0))))),
        },
    ] {
        let body = counted(
            PreparedGraphSet::singleton()
                .unwind("invalid".into(), value)
                .unwrap(),
        );
        let query = outer().apply(body, vec![]).unwrap();
        let mut circuit = FilteredCircuit::new(&query);
        circuit
            .tick(&ZSet::new(), &bag([(row(vec![]), 1)]))
            .unwrap();
        assert!(circuit.tick(&unwind_outer(1), &ZSet::new()).is_err());
    }
    // The unused CASE branch remains lazy inside an active body, rather than
    // merely being suppressed by an empty outer input.
    let value = GraphSetValue::Case {
        arms: vec![(
            GraphSetValue::Value(GraphValue::Scalar(CanonicalScalar::Bool(true))),
            GraphSetValue::List(vec![]),
        )],
        otherwise: Some(Box::new(GraphSetValue::List(vec![divide]))),
    };
    let query = outer()
        .apply(
            counted(
                PreparedGraphSet::singleton()
                    .unwind("safe".into(), value)
                    .unwrap(),
            ),
            vec![],
        )
        .unwrap();
    let mut circuit = FilteredCircuit::new(&query);
    circuit
        .tick(&unwind_outer(1), &bag([(row(vec![]), 1)]))
        .unwrap();
    assert_eq!(
        plain(circuit.rows()),
        BTreeMap::from([(
            row(vec![scalar(Some(2)), vertex(1), vertex(8), scalar(Some(0))]),
            2,
        )])
    );
}

#[test]
fn empty_post_summary_unwind_null_extends_optional_calls_without_repeating_the_tail() {
    for empty_body in [false, true] {
        let mut body = arguments(vec![GraphSetColumnType::Stored]);
        if empty_body {
            body = body
                .filter(&[GraphSetPredicateOp::Truth(Some(false))])
                .unwrap();
        }
        let divide = GraphSetValue::Integer(
            GraphIntegerExpression::prepare(&[
                GraphIntegerOp::Literal(Some(1)),
                GraphIntegerOp::Literal(Some(0)),
                GraphIntegerOp::Binary(GraphIntegerBinary::Divide),
            ])
            .unwrap(),
        );
        let body = counted(body)
            .unwind("absent".into(), GraphSetValue::List(vec![]))
            .unwrap()
            .project(
                vec![GraphSetProjection::new("unexecuted", divide)],
                GraphSetQuantifier::All,
            )
            .unwrap();
        let query = outer().optional_apply(body, vec![0]).unwrap();
        let mut circuit = FilteredCircuit::new(&query);
        let left = unwind_outer(15);
        circuit.tick(&left, &bag([(row(vec![]), 1)])).unwrap();
        let expected = left
            .iter()
            .map(|(outer, weight)| {
                let mut cells = outer.values().to_vec();
                cells.push(scalar(None));
                (row(cells), weight.to_i128().unwrap())
            })
            .collect::<Bag>();
        assert_eq!(
            plain(circuit.rows()),
            expected,
            "the optional row is NULL, not a zero-filled or re-evaluated summary"
        );
    }
}

#[test]
fn native_correlated_unwind_profiles_keep_reader_page_and_cancellation_boundaries() {
    for text in [
        "MATCH (a:L) WITH a,a.p AS value CALL(value) { RETURN count(*) AS degree,count(value) AS present,sum(value) AS total } RETURN a,degree,present,total",
        "MATCH (a:L) WITH a CALL() { UNWIND [1,1,NULL] AS value RETURN count(*) AS degree,count(value) AS present,sum(value) AS total } RETURN a,degree,present,total",
        "MATCH (a:L) WITH a,a.p AS score OPTIONAL CALL(score) { UNWIND CASE WHEN score IS NULL THEN [] ELSE [score,score,score+1] END AS value RETURN value%2 AS bucket,count(*) AS degree,sum(value) AS total } RETURN a,bucket,degree,total",
        "MATCH (a:L) WITH a,a.p AS score CALL(a,score) { MATCH (a)-[:R]->(b:L) UNWIND [score,score] AS value WITH DISTINCT b,value RETURN value AS bucket,count(*) AS degree } RETURN a,bucket,degree",
        "MATCH (a:L) WITH a,a.p AS score CALL(a,score) { UNWIND [score,score] AS value WITH count(value) AS degree UNWIND [degree,degree] AS copy RETURN a AS owner,copy } RETURN a,owner,copy",
        "MATCH (a:L) WITH a CALL() { UNWIND range(3,-1,-2) AS value WITH value%2 AS bucket,count(*) AS degree UNWIND [degree,degree] AS copy RETURN DISTINCT copy } RETURN a,copy",
    ] {
        let query = PreparedGraphSetText::prepare(text, symbols)
            .unwrap_or_else(|error| panic!("{text}: {error:?}"))
            .bind_parameters(&GqlParameters::new())
            .unwrap();
        assert!(has_filtered_aggregate(&query), "{text}");
    }
    for text in [
        "MATCH (a:L) WITH a CALL(a) { UNWIND a.p AS value RETURN count(*) AS degree } RETURN a,degree",
        "MATCH (a:L) WITH a CALL(a) { UNWIND [rand()] AS value RETURN count(*) AS degree } RETURN a,degree",
        "MATCH (a:L) WITH a CALL() { UNWIND [1,2] AS value WITH value LIMIT 0 RETURN count(*) AS degree } RETURN a,degree",
        "MATCH (a:L) WITH a CALL() { WITH count(*) AS degree UNWIND [degree,degree] AS copy RETURN copy LIMIT 0 } RETURN a,copy",
    ] {
        let query = PreparedGraphSetText::prepare(text, symbols)
            .unwrap()
            .bind_parameters(&GqlParameters::new())
            .unwrap();
        assert!(!has_filtered_aggregate(&query), "{text}");
    }
    let query = unwind_summary(7, true);
    let original = query.canonical_bytes();
    let mut calls = 0;
    query
        .incremental_filtered_aggregate_with_control(&mut |_| {
            calls += 1;
            Ok::<_, usize>(())
        })
        .unwrap()
        .unwrap();
    for stop in 1..=calls {
        let mut seen = 0;
        assert!(
            matches!(query.incremental_filtered_aggregate_with_control(&mut |_| {
            seen += 1;
            if seen == stop { Err(stop) } else { Ok(()) }
        }), Err(at) if at == stop)
        );
        assert_eq!(seen, stop);
        assert_eq!(query.canonical_bytes(), original);
    }
}

fn grouped_page(optional: bool, profile: usize, offset: u64, count: u64) -> PreparedGraphSet {
    let SetNode::Apply {
        left,
        right,
        imports,
        ..
    } = grouped_filtered_summary(optional, profile).node
    else {
        unreachable!()
    };
    let order = if profile < 2 {
        [
            GraphValueOrder::descending(2),
            GraphValueOrder::ascending(1),
        ]
    } else {
        [
            GraphValueOrder::descending(0),
            GraphValueOrder::ascending(1),
        ]
    };
    let body = (*right)
        .with_order_by(&order)
        .unwrap()
        .with_page(offset, Some(count));
    if optional {
        (*left).optional_apply(body, imports).unwrap()
    } else {
        (*left).apply(body, imports).unwrap()
    }
}

// Apply literal occurrence slicing to the independent grouped-summary oracle.
// No native comparator, window, recipe or maintained operator constructs this
// expected page. Outer weights are removed before selection and restored after.
fn grouped_page_oracle(
    left: &ZSet<GraphValueRow>,
    right: &ZSet<GraphValueRow>,
    optional: bool,
    profile: usize,
    offset: u64,
    count: u64,
) -> Bag {
    let summaries = grouped_summary_oracle(left, right, false, profile);
    let mut output = Bag::new();
    for (outer, weight) in left.iter() {
        let weight = weight.to_i128().unwrap();
        let mut candidates: Vec<_> = summaries
            .iter()
            .filter_map(|(row, n)| {
                (row.values().get(..3) == Some(outer.values())).then(|| {
                    assert_eq!(n % weight, 0);
                    (row.values()[3..].to_vec(), n / weight)
                })
            })
            .collect();
        candidates.sort_by(|(a, _), (b, _)| {
            let rank = if profile < 2 { 2 } else { 0 };
            let integer = |value: &GraphValue| match value {
                GraphValue::Scalar(CanonicalScalar::Int(value)) => *value,
                _ => panic!("the fixture's ranking count is a nonnull integer"),
            };
            integer(&b[rank])
                .cmp(&integer(&a[rank]))
                .then_with(|| match (a[1].is_null(), b[1].is_null()) {
                    (true, false) => std::cmp::Ordering::Greater,
                    (false, true) => std::cmp::Ordering::Less,
                    _ => a[1].cmp(&b[1]),
                })
                .then_with(|| a.cmp(b))
        });
        let mut skip = i128::from(offset);
        let mut remaining = i128::from(count);
        let mut selected = false;
        for (cells, n) in candidates {
            let skipped = skip.min(n);
            skip -= skipped;
            let emitted = remaining.min(n - skipped);
            remaining -= emitted;
            if emitted != 0 {
                selected = true;
                let mut result = outer.values().to_vec();
                result.extend(cells);
                output.insert(row(result), weight * emitted);
            }
        }
        if optional && !selected {
            let mut result = outer.values().to_vec();
            result.extend(vec![scalar(None); if profile < 2 { 7 } else { 2 }]);
            output.insert(row(result), weight);
        }
    }
    output
}

#[test]
fn grouped_invocation_pages_match_4096_weighted_transitions() {
    for optional in [false, true] {
        for profile in 0..4 {
            for (offset, count) in [(0, 1), (1, 2)] {
                let query = grouped_page(optional, profile, offset, count);
                let recipe = query
                    .incremental_filtered_aggregate_with_control(&mut allow)
                    .unwrap()
                    .unwrap();
                let window = recipe.window.as_ref().unwrap();
                assert!(!window.partition_columns().is_empty());
                assert!(query.incremental_window_sequence_compatible());
                for before in 0..16 {
                    for after in 0..16 {
                        let old_left = filtered_left(before & 3);
                        let old_right = filtered_right(before >> 2);
                        let next_left = filtered_left(after & 3);
                        let next_right = filtered_right(after >> 2);
                        let old = grouped_page_oracle(
                            &old_left, &old_right, optional, profile, offset, count,
                        );
                        let next = grouped_page_oracle(
                            &next_left,
                            &next_right,
                            optional,
                            profile,
                            offset,
                            count,
                        );
                        let mut circuit = FilteredCircuit::new(&query);
                        circuit.tick(&old_left, &old_right).unwrap();
                        assert_eq!(plain(circuit.rows()), old);
                        let delta = circuit
                            .tick(
                                &difference(&next_left, &old_left),
                                &difference(&next_right, &old_right),
                            )
                            .unwrap();
                        assert_eq!(
                            plain(circuit.rows()),
                            next,
                            "{optional}/{profile}/{offset}: {before}->{after}"
                        );
                        assert_eq!(
                            delta,
                            bag(next
                                .into_iter()
                                .chain(old.into_iter().map(|(row, n)| (row, -n))))
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn empty_global_counts_are_completed_before_pages_and_optional_restoration() {
    let left = bag([
        (row(vec![scalar(None), vertex(1), vertex(8)]), 2),
        (
            row(vec![
                GraphValue::Scalar(CanonicalScalar::Bool(false)),
                vertex(2),
                vertex(9),
            ]),
            3,
        ),
    ]);
    let unit = bag([(row(vec![]), 1)]);
    for imported in [false, true] {
        for optional in [false, true] {
            for (offset, count) in [(0, 1), (1, 1), (0, 0)] {
                let body = arguments(if imported {
                    vec![GraphSetColumnType::Stored]
                } else {
                    vec![]
                })
                .filter(&[GraphSetPredicateOp::Truth(Some(false))])
                .unwrap()
                .group_by(&[], &[GraphAggregate::count_rows("n")])
                .unwrap()
                .with_order_by(&[GraphValueOrder::descending(0)])
                .unwrap()
                .with_page(offset, Some(count));
                let imports = if imported { vec![0] } else { vec![] };
                let query = if optional {
                    outer().optional_apply(body, imports).unwrap()
                } else {
                    outer().apply(body, imports).unwrap()
                };
                let mut circuit = FilteredCircuit::new(&query);
                circuit.tick(&ZSet::new(), &unit).unwrap();
                assert!(
                    circuit.rows().is_empty(),
                    "a unit never creates an invocation"
                );
                circuit.tick(&left, &ZSet::new()).unwrap();
                let emits_zero = offset == 0 && count == 1;
                let expected = if emits_zero || optional {
                    left.iter()
                        .map(|(r, n)| {
                            let mut cells = r.values().to_vec();
                            cells.push(scalar(emits_zero.then_some(0)));
                            (row(cells), n.to_i128().unwrap())
                        })
                        .collect()
                } else {
                    Bag::new()
                };
                assert_eq!(plain(circuit.rows()), expected);
                circuit
                    .tick(&difference(&ZSet::new(), &left), &ZSet::new())
                    .unwrap();
                assert!(circuit.rows().is_empty());
            }
        }
    }
}

#[test]
fn grouped_pages_preserve_tail_errors_and_exact_sequence_boundaries() {
    let divide = GraphSetValue::Integer(
        GraphIntegerExpression::prepare(&[
            GraphIntegerOp::Literal(Some(1)),
            GraphIntegerOp::Literal(Some(0)),
            GraphIntegerOp::Binary(GraphIntegerBinary::Divide),
        ])
        .unwrap(),
    );
    for filter_first in [false, true] {
        let mut body = arguments(vec![])
            .group_by(&[], &[GraphAggregate::count_rows("n")])
            .unwrap();
        if filter_first {
            body = body
                .filter(&[GraphSetPredicateOp::Truth(Some(false))])
                .unwrap();
        }
        body = body
            .project(
                vec![GraphSetProjection::new("discarded", divide.clone())],
                GraphSetQuantifier::All,
            )
            .unwrap();
        if !filter_first {
            body = body
                .filter(&[GraphSetPredicateOp::Truth(Some(false))])
                .unwrap();
        }
        let body = body
            .with_order_by(&[GraphValueOrder::ascending(0)])
            .unwrap()
            .with_page(0, Some(0));
        let query = outer().optional_apply(body, vec![]).unwrap();
        let mut circuit = FilteredCircuit::new(&query);
        circuit
            .tick(&ZSet::new(), &bag([(row(vec![]), 1)]))
            .unwrap();
        let result = circuit.tick(&left_rows(1), &ZSet::new());
        assert_eq!(
            result.is_ok(),
            filter_first,
            "LIMIT zero cannot suppress a reached expression"
        );
    }
    for text in [
        "MATCH (a:L) WITH a CALL(a) { MATCH (a)-[:R]->(b) RETURN b AS bucket,count(*) AS degree ORDER BY degree DESC,bucket LIMIT 2 } RETURN a,bucket,degree",
        "MATCH (a:L) WITH a OPTIONAL CALL(a) { MATCH (a)-[:R]->(b) WITH b,count(*) AS n WHERE n>1 RETURN DISTINCT n AS degree ORDER BY degree DESC SKIP 1 LIMIT 2 } RETURN a,degree",
        "MATCH (a:L) WITH a CALL() { UNWIND [1,1,2] AS item WITH item,count(*) AS n UNWIND [n,n] AS copy RETURN copy ORDER BY copy DESC SKIP 1 LIMIT 2 } RETURN a,copy",
    ] {
        let query = PreparedGraphSetText::prepare(text, symbols)
            .unwrap()
            .bind_parameters(&GqlParameters::new())
            .unwrap();
        let recipe = filtered_aggregate_recipe(&query).unwrap();
        assert!(recipe.window.is_some(), "{text}");
        assert!(query.incremental_window_sequence_compatible(), "{text}");
        assert!(
            !query
                .clone()
                .with_page(0, Some(1))
                .incremental_window_sequence_compatible(),
            "an unrelated outer positional page is not consumed"
        );
    }
    for text in [
        "MATCH (a:L) WITH a CALL(a) { MATCH (a)-[:R]->(b) RETURN b AS bucket,count(*) AS degree ORDER BY degree DESC } RETURN a,bucket,degree",
        "MATCH (a:L) WITH a CALL(a) { MATCH (a)-[:R]->(b) WITH b LIMIT 1 RETURN b AS bucket,count(*) AS degree ORDER BY degree DESC LIMIT 2 } RETURN a,bucket,degree",
        "MATCH (a:L) WITH a CALL(a) { MATCH (a)-[:R]->(b) WITH b,count(*) AS n ORDER BY n DESC LIMIT 1 RETURN n ORDER BY n DESC LIMIT 2 } RETURN a,n",
        "MATCH (a:L) WITH a CALL(a) { MATCH (a)-[:R]->(b) RETURN b.p AS bucket,count(*) AS degree ORDER BY degree DESC LIMIT 2 } RETURN a,bucket,degree",
    ] {
        let query = PreparedGraphSetText::prepare(text, symbols)
            .unwrap()
            .bind_parameters(&GqlParameters::new())
            .unwrap();
        assert!(!has_filtered_aggregate(&query), "{text}");
    }
    let query = grouped_page(true, 3, 1, 2);
    let original = query.canonical_bytes();
    let mut calls = 0;
    query
        .incremental_filtered_aggregate_with_control(&mut |_| {
            calls += 1;
            Ok::<_, usize>(())
        })
        .unwrap()
        .unwrap();
    for stop in 1..=calls {
        let mut seen = 0;
        assert!(
            matches!(query.incremental_filtered_aggregate_with_control(&mut |_| {
            seen += 1; if seen == stop { Err(stop) } else { Ok(()) }
        }), Err(at) if at == stop)
        );
        assert_eq!(seen, stop);
        assert_eq!(query.canonical_bytes(), original);
    }
}

fn collected_summary(optional: bool, grouped: bool, distinct_rows: bool) -> PreparedGraphSet {
    let args = arguments(vec![GraphSetColumnType::Stored, GraphSetColumnType::Vertex]);
    let source = right();
    let on = RowJoinSpec::cross(args.column_types(), source.column_types())
        .unwrap()
        .with_predicate(&[
            GraphSetPredicateOp::Compare {
                left: GraphSetOperand::Column(3),
                comparison: IntegerComparison::NotEqual,
                right: GraphSetOperand::Column(1),
            },
            GraphSetPredicateOp::IsNull {
                operand: GraphSetOperand::Column(1),
                is_null: true,
            },
            GraphSetPredicateOp::Or,
        ])
        .unwrap();
    let body = args
        .join(source, on)
        .unwrap()
        .project(
            vec![
                // Retain the source identity ahead of the payload. Sorting
                // collection values instead of full input rows is observably
                // wrong when these identities carry reverse-valued payloads.
                GraphSetProjection::new("ordinal", GraphSetValue::Column(2)),
                GraphSetProjection::new("member", GraphSetValue::Column(3)),
                GraphSetProjection::new("threshold", GraphSetValue::Column(0)),
                GraphSetProjection::new(
                    "nested",
                    GraphSetValue::List(vec![GraphSetValue::Column(3), GraphSetValue::Column(0)]),
                ),
            ],
            if distinct_rows {
                GraphSetQuantifier::Distinct
            } else {
                GraphSetQuantifier::All
            },
        )
        .unwrap()
        .filter(&[
            GraphSetPredicateOp::Compare {
                left: GraphSetOperand::Column(2),
                comparison: IntegerComparison::Greater,
                right: GraphSetOperand::Literal(
                    crate::GqlScalarParameter::new(CanonicalScalar::Int(2)).unwrap(),
                ),
            },
            GraphSetPredicateOp::IsNull {
                operand: GraphSetOperand::Column(2),
                is_null: true,
            },
            GraphSetPredicateOp::Or,
        ])
        .unwrap()
        .group_by(
            if grouped { &[1] } else { &[] },
            &[
                GraphAggregate::count_rows("rows"),
                GraphAggregate::collect("members", 1),
                GraphAggregate::collect_distinct("unique", 1),
                GraphAggregate::collect("nested", 3),
                GraphAggregate::collect_distinct("unique_nested", 3),
                GraphAggregate::collect("imports", 2),
            ],
        )
        .unwrap();
    if optional {
        outer().optional_apply(body, vec![0, 2]).unwrap()
    } else {
        outer().apply(body, vec![0, 2]).unwrap()
    }
}

fn collected_right(mask: usize) -> ZSet<GraphValueRow> {
    bag([
        (mask & 1 != 0, row(vec![vertex(100), vertex(9)]), 2),
        (mask != 0, row(vec![vertex(101), vertex(8)]), 3),
        (mask & 2 != 0, row(vec![vertex(102), vertex(9)]), 4),
        (mask != 0, row(vec![vertex(103), scalar(None)]), 1),
        (mask & 2 != 0, row(vec![vertex(104), vertex(7)]), 2),
    ]
    .into_iter()
    .filter_map(|(present, row, weight)| present.then_some((row, weight))))
}

fn collected_list(values: Vec<GraphValue>) -> GraphValue {
    GraphValue::List(values.into_boxed_slice())
}

// Independent sequence arithmetic over the fixture's complete source order.
// No native group, row comparator or compiled recipe supplies expected lists.
fn collected_oracle(
    left: &ZSet<GraphValueRow>,
    right: &ZSet<GraphValueRow>,
    optional: bool,
    grouped: bool,
    distinct_rows: bool,
) -> Bag {
    let mut output = Bag::new();
    for (outer, multiplicity) in left.iter() {
        let threshold = &outer.values()[0];
        let other = &outer.values()[2];
        let mut groups = BTreeMap::<GraphValue, Vec<(GraphValue, i128)>>::new();
        if !grouped {
            groups.insert(scalar(None), Vec::new());
        }
        if threshold.is_null() || threshold == &scalar(Some(3)) {
            for (inner, weight) in right.iter() {
                let member = &inner.values()[1];
                if other.is_null() || (!member.is_null() && member != other) {
                    groups
                        .entry(if grouped {
                            member.clone()
                        } else {
                            scalar(None)
                        })
                        .or_default()
                        .push((
                            member.clone(),
                            if distinct_rows {
                                1
                            } else {
                                weight.to_i128().unwrap()
                            },
                        ));
                }
            }
        }
        if groups.is_empty() && optional {
            let mut cells = outer.values().to_vec();
            cells.extend((0..7).map(|_| scalar(None)));
            output.insert(row(cells), multiplicity.to_i128().unwrap());
        }
        for (key, occurrences) in groups {
            let mut count = 0_i64;
            let mut members = Vec::new();
            let mut unique = Vec::new();
            let mut nested = Vec::new();
            let mut unique_nested = Vec::new();
            let mut imports = Vec::new();
            let mut seen = BTreeSet::new();
            let mut seen_nested = BTreeSet::new();
            for (member, weight) in occurrences {
                let item = collected_list(vec![member.clone(), threshold.clone()]);
                if !member.is_null() && seen.insert(member.clone()) {
                    unique.push(member.clone());
                }
                if seen_nested.insert(item.clone()) {
                    unique_nested.push(item.clone());
                }
                for _ in 0..weight {
                    count += 1;
                    if !member.is_null() {
                        members.push(member.clone());
                    }
                    nested.push(item.clone());
                    if !threshold.is_null() {
                        imports.push(threshold.clone());
                    }
                }
            }
            let mut cells = outer.values().to_vec();
            if grouped {
                cells.push(key);
            }
            cells.extend([
                scalar(Some(count)),
                collected_list(members),
                collected_list(unique),
                collected_list(nested),
                collected_list(unique_nested),
                collected_list(imports),
            ]);
            output.insert(row(cells), multiplicity.to_i128().unwrap());
        }
    }
    output
}

#[test]
fn correlated_collections_match_weighted_transitions_and_first_occurrence_retractions() {
    for optional in [false, true] {
        for grouped in [false, true] {
            for distinct_rows in [false, true] {
                let query = collected_summary(optional, grouped, distinct_rows);
                for before in 0..16 {
                    for after in 0..16 {
                        let old_left = filtered_left(before & 3);
                        let old_right = collected_right(before >> 2);
                        let next_left = filtered_left(after & 3);
                        let next_right = collected_right(after >> 2);
                        let old = collected_oracle(
                            &old_left,
                            &old_right,
                            optional,
                            grouped,
                            distinct_rows,
                        );
                        let next = collected_oracle(
                            &next_left,
                            &next_right,
                            optional,
                            grouped,
                            distinct_rows,
                        );
                        let mut circuit = FilteredCircuit::new(&query);
                        circuit.tick(&old_left, &old_right).unwrap();
                        assert_eq!(plain(circuit.rows()), old, "seed {before}");
                        let delta = circuit
                            .tick(
                                &difference(&next_left, &old_left),
                                &difference(&next_right, &old_right),
                            )
                            .unwrap();
                        assert_eq!(
                            plain(circuit.rows()),
                            next,
                            "optional {optional}, grouped {grouped}, distinct {distinct_rows}, {before}->{after}"
                        );
                        assert_eq!(
                            delta,
                            bag(next
                                .into_iter()
                                .chain(old.into_iter().map(|(row, n)| (row, -n))))
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn collection_completion_keeps_list_types_and_runs_before_the_summary_tail() {
    let SetNode::Apply {
        left,
        right,
        imports,
        ..
    } = collected_summary(false, false, false).node
    else {
        unreachable!()
    };
    let body = right
        .project(
            vec![
                GraphSetProjection::new("members", GraphSetValue::Column(1)),
                GraphSetProjection::new(
                    "size",
                    GraphSetValue::Size(Box::new(GraphSetValue::Column(1))),
                ),
            ],
            GraphSetQuantifier::All,
        )
        .unwrap();
    let query = (*left).optional_apply(body, imports).unwrap();
    let recipe = query
        .incremental_filtered_aggregate_with_control(&mut allow)
        .unwrap()
        .unwrap();
    assert_eq!(
        recipe.summary[0].column_types()[1],
        GraphSetColumnType::List
    );
    assert_eq!(query.column_types()[3], GraphSetColumnType::List);
    let left = filtered_left(3);
    let mut circuit = FilteredCircuit::new(&query);
    circuit.tick(&left, &ZSet::new()).unwrap();
    let expected: Bag = plain(&left)
        .into_iter()
        .map(|(outer, weight)| {
            let mut cells = outer.values().to_vec();
            cells.extend([collected_list(vec![]), scalar(Some(0))]);
            (row(cells), weight)
        })
        .collect();
    assert_eq!(plain(circuit.rows()), expected);
}

#[test]
fn collection_bodies_remain_lazy_and_keep_discarded_expression_errors() {
    let divide = GraphSetValue::Integer(
        GraphIntegerExpression::prepare(&[
            GraphIntegerOp::Literal(Some(1)),
            GraphIntegerOp::Literal(Some(0)),
            GraphIntegerOp::Binary(GraphIntegerBinary::Divide),
        ])
        .unwrap(),
    );
    for filter_first in [false, true] {
        let mut body = base();
        if filter_first {
            body = body
                .filter(&[GraphSetPredicateOp::Truth(Some(false))])
                .unwrap();
        }
        body = body
            .project(
                vec![GraphSetProjection::new("failed", divide.clone())],
                GraphSetQuantifier::All,
            )
            .unwrap();
        if !filter_first {
            body = body
                .filter(&[GraphSetPredicateOp::Truth(Some(false))])
                .unwrap();
        }
        let query = outer()
            .apply(
                body.group_by(&[], &[GraphAggregate::collect("members", 0)])
                    .unwrap(),
                vec![1, 1],
            )
            .unwrap();
        let mut circuit = FilteredCircuit::new(&query);
        circuit.tick(&ZSet::new(), &right_rows(3)).unwrap();
        assert!(circuit.rows().is_empty());
        assert_eq!(
            circuit.tick(&left_rows(1), &ZSet::new()).is_ok(),
            filter_first
        );
        if filter_first {
            let mut cells = left_rows(1).iter().next().unwrap().0.values().to_vec();
            cells.push(collected_list(vec![]));
            assert_eq!(plain(circuit.rows()), BTreeMap::from([(row(cells), 2)]));
        }
    }
}

#[test]
fn collections_require_proven_input_order_and_distinct_can_restore_that_proof() {
    let unwind = || {
        base()
            .unwind(
                "member".into(),
                GraphSetValue::List(vec![
                    GraphSetValue::Value(scalar(Some(2))),
                    GraphSetValue::Value(scalar(Some(1))),
                    GraphSetValue::Value(scalar(Some(2))),
                ]),
            )
            .unwrap()
    };
    for body in [
        unwind(),
        unwind()
            .project(
                (0..5)
                    .map(|column| {
                        GraphSetProjection::new(format!("c{column}"), GraphSetValue::Column(column))
                    })
                    .collect(),
                GraphSetQuantifier::All,
            )
            .unwrap(),
        base()
            .with_order_by(&[GraphValueOrder::descending(3)])
            .unwrap()
            .with_page(0, Some(1)),
    ] {
        let column = body.column_types().len() - 1;
        let summary = body
            .group_by(&[], &[GraphAggregate::collect("members", column)])
            .unwrap();
        for terminal_page in [false, true] {
            let body = if terminal_page {
                summary
                    .clone()
                    .with_order_by(&[GraphValueOrder::ascending(0)])
                    .unwrap()
                    .with_page(0, Some(1))
            } else {
                summary.clone()
            };
            let query = outer().apply(body, vec![1, 1]).unwrap();
            assert!(
                query
                    .incremental_filtered_aggregate_with_control(&mut allow)
                    .unwrap()
                    .is_none(),
                "ordering a completed summary cannot prove its collection input sequence"
            );
        }
    }
    let body = unwind()
        .project(
            vec![GraphSetProjection::new("member", GraphSetValue::Column(4))],
            GraphSetQuantifier::Distinct,
        )
        .unwrap()
        .group_by(&[], &[GraphAggregate::collect("members", 0)])
        .unwrap();
    let query = outer().apply(body, vec![1, 1]).unwrap();
    let mut circuit = FilteredCircuit::new(&query);
    let left = left_rows(3);
    let right = right_rows(3);
    circuit.tick(&left, &right).unwrap();
    let expected = |members: Vec<GraphValue>| -> Bag {
        plain(&left)
            .into_iter()
            .map(|(outer, weight)| {
                let mut cells = outer.values().to_vec();
                cells.push(collected_list(members.clone()));
                (row(cells), weight)
            })
            .collect()
    };
    assert_eq!(
        plain(circuit.rows()),
        expected(vec![scalar(Some(1)), scalar(Some(2))])
    );
    circuit
        .tick(&ZSet::new(), &difference(&ZSet::new(), &right))
        .unwrap();
    assert_eq!(plain(circuit.rows()), expected(vec![]));
}

#[test]
fn native_correlated_collections_keep_schema_identity_and_cancellation() {
    for text in [
        "MATCH (a:L) WITH a CALL (a) { MATCH (b:L) WHERE b <> a RETURN sum(1) AS total,collect(b) AS values } RETURN a,total,values",
        "MATCH (a:L) WITH a CALL (a) { MATCH (b:L) WHERE b <> a RETURN collect(b) AS total } RETURN a,total",
        "MATCH (a:L) WITH a CALL(a) { MATCH (a)-[:R]->(b:L) RETURN collect(b) AS members } RETURN a,members",
        "MATCH (a:L) WITH a OPTIONAL CALL(a) { MATCH (a)-[:R]->(b:L) RETURN collect(DISTINCT b) AS members,count(*) AS degree } RETURN a,members,degree",
        "MATCH (a:L) WITH a CALL(a) { MATCH (a)-[:R]->(b:L) WITH DISTINCT b RETURN collect(b) AS members } RETURN a,members",
        "MATCH (a:L) WITH a CALL(a) { MATCH (a)-[r:R]->(b) WITH b,id(r)%7 AS rank ORDER BY rank DESC RETURN collect(b) AS members } RETURN a,members",
        "MATCH (a:L) WITH a OPTIONAL CALL(a) { MATCH (a)-[r:R]->(b) WITH b,id(r)%7 AS rank ORDER BY rank RETURN collect(DISTINCT b) AS members,count(*) AS degree } RETURN a,members,degree",
    ] {
        let query = PreparedGraphSetText::prepare(text, symbols)
            .unwrap()
            .bind_parameters(&GqlParameters::new())
            .unwrap();
        assert!(has_filtered_aggregate(&query), "{text}");
    }
    let query = collected_summary(true, false, false);
    let original = query.canonical_bytes();
    let mut calls = 0;
    query
        .incremental_filtered_aggregate_with_control(&mut |_| {
            calls += 1;
            Ok::<_, usize>(())
        })
        .unwrap()
        .unwrap();
    for stop in 1..=calls {
        let mut seen = 0;
        assert!(
            matches!(query.incremental_filtered_aggregate_with_control(&mut |_| {
            seen += 1;
            if seen == stop { Err(stop) } else { Ok(()) }
        }), Err(at) if at == stop)
        );
        assert_eq!(seen, stop);
        assert_eq!(query.canonical_bytes(), original);
    }
}

// Rank completed summaries independently of the collection's input sequence.
// Each original group has one occurrence per invocation; outer weights are
// restored only after literal page slicing of these expected summaries.
fn collected_page_oracle(
    left: &ZSet<GraphValueRow>,
    right: &ZSet<GraphValueRow>,
    optional: bool,
    grouped: bool,
    distinct_rows: bool,
    offset: u64,
    count: u64,
) -> Bag {
    let summaries = collected_oracle(left, right, false, grouped, distinct_rows);
    let mut output = Bag::new();
    for (outer, weight) in left.iter() {
        let weight = weight.to_i128().unwrap();
        let mut candidates: Vec<_> = summaries
            .iter()
            .filter_map(|(row, n)| {
                (row.values().get(..3) == Some(outer.values())).then(|| {
                    assert_eq!(*n, weight);
                    row.values()[3..].to_vec()
                })
            })
            .collect();
        let rank = usize::from(grouped);
        candidates.sort_by(|a, b| {
            let integer = |value: &GraphValue| match value {
                GraphValue::Scalar(CanonicalScalar::Int(value)) => *value,
                _ => panic!("the fixture ranks a nonnull COUNT result"),
            };
            integer(&b[rank])
                .cmp(&integer(&a[rank]))
                .then_with(|| {
                    if grouped {
                        match (a[0].is_null(), b[0].is_null()) {
                            (true, false) => std::cmp::Ordering::Greater,
                            (false, true) => std::cmp::Ordering::Less,
                            _ => a[0].cmp(&b[0]),
                        }
                    } else {
                        std::cmp::Ordering::Equal
                    }
                })
                .then_with(|| a.cmp(b))
        });
        let mut selected = false;
        for cells in candidates
            .into_iter()
            .skip(usize::try_from(offset).unwrap())
            .take(usize::try_from(count).unwrap())
        {
            selected = true;
            let mut result = outer.values().to_vec();
            result.extend(cells);
            output.insert(row(result), weight);
        }
        if optional && !selected {
            let mut result = outer.values().to_vec();
            result.extend(vec![scalar(None); if grouped { 7 } else { 6 }]);
            output.insert(row(result), weight);
        }
    }
    output
}

#[test]
fn ranked_collection_pages_preserve_lists_completion_and_optional_absence() {
    for optional in [false, true] {
        for grouped in [false, true] {
            for distinct_rows in [false, true] {
                for (offset, count) in [(0, 1), (1, 1), (0, 0)] {
                    let SetNode::Apply {
                        left,
                        right,
                        imports,
                        ..
                    } = collected_summary(optional, grouped, distinct_rows).node
                    else {
                        unreachable!()
                    };
                    let order = if grouped {
                        vec![
                            GraphValueOrder::descending(1),
                            GraphValueOrder::ascending(0),
                        ]
                    } else {
                        vec![GraphValueOrder::descending(0)]
                    };
                    let body = (*right)
                        .with_order_by(&order)
                        .unwrap()
                        .with_page(offset, Some(count));
                    let query = if optional {
                        (*left).optional_apply(body, imports).unwrap()
                    } else {
                        (*left).apply(body, imports).unwrap()
                    };
                    let recipe = query
                        .incremental_filtered_aggregate_with_control(&mut allow)
                        .unwrap()
                        .unwrap();
                    assert!(recipe.window.is_some());
                    assert!(query.incremental_window_sequence_compatible());
                    let mut circuit = FilteredCircuit::new(&query);
                    let mut old_left = ZSet::new();
                    let mut old_right = ZSet::new();
                    let mut old = Bag::new();
                    // Delete first DISTINCT support, remove every group, refill
                    // off-page groups and reactivate NULL/duplicate invocations.
                    for (left_mask, right_mask) in
                        [(3, 3), (3, 2), (1, 0), (3, 1), (2, 3), (0, 3), (3, 3)]
                    {
                        let left = filtered_left(left_mask);
                        let right = collected_right(right_mask);
                        let next = collected_page_oracle(
                            &left,
                            &right,
                            optional,
                            grouped,
                            distinct_rows,
                            offset,
                            count,
                        );
                        let delta = circuit
                            .tick(
                                &difference(&left, &old_left),
                                &difference(&right, &old_right),
                            )
                            .unwrap();
                        assert_eq!(plain(circuit.rows()), next);
                        assert_eq!(
                            delta,
                            bag(next
                                .iter()
                                .map(|(row, n)| (row.clone(), *n))
                                .chain(old.into_iter().map(|(row, n)| (row, -n))),)
                        );
                        old = next;
                        old_left = left;
                        old_right = right;
                    }
                }
            }
        }
    }
}


fn ordered_collected_summary(
    optional: bool,
    grouped: bool,
    descending: bool,
) -> PreparedGraphSet {
    let frame = || arguments(vec![GraphSetColumnType::Stored, GraphSetColumnType::Vertex]);
    let args = frame();
    let source = right();
    let on = RowJoinSpec::cross(args.column_types(), source.column_types())
        .unwrap()
        .with_predicate(&[
            GraphSetPredicateOp::Compare {
                left: GraphSetOperand::Column(3),
                comparison: IntegerComparison::NotEqual,
                right: GraphSetOperand::Column(1),
            },
            GraphSetPredicateOp::IsNull {
                operand: GraphSetOperand::Column(1),
                is_null: true,
            },
            GraphSetPredicateOp::Or,
        ])
        .unwrap();
    let rank = GraphSetValue::Integer(
        GraphIntegerExpression::prepare(&[
            GraphIntegerOp::IdentityColumn {
                column: 2,
                text: false,
            },
            GraphIntegerOp::Literal(Some(2)),
            GraphIntegerOp::Binary(GraphIntegerBinary::Remainder),
        ])
        .unwrap(),
    );
    let order = if descending {
        GraphValueOrder::descending(2).with_nulls_first(true)
    } else {
        GraphValueOrder::ascending(2)
    };
    let body = args
        .join(source, on)
        .unwrap()
        .project(
            vec![
                GraphSetProjection::new("ordinal", GraphSetValue::Column(2)),
                GraphSetProjection::new("member", GraphSetValue::Column(3)),
                GraphSetProjection::new("rank", rank),
            ],
            GraphSetQuantifier::All,
        )
        .unwrap()
        .with_order_by(&[order])
        .unwrap()
        // Both the explicit sort key and the complete-row tie breaker
        // disappear before grouping. Sorting only the retained member would
        // reverse the rank-zero tie between ordinals 100 and 102.
        .project_ordered_output(vec![GraphSetProjection::new(
            "member",
            GraphSetValue::Column(1),
        )])
        .unwrap()
        .cross_join(
            frame()
                .project_ordered_output(vec![GraphSetProjection::new(
                    "threshold",
                    GraphSetValue::Column(0),
                )])
                .unwrap(),
        )
        .unwrap()
        .project_ordered_output(vec![
            GraphSetProjection::new("threshold", GraphSetValue::Column(1)),
            GraphSetProjection::new("member", GraphSetValue::Column(0)),
        ])
        .unwrap()
        .group_by(
            if grouped { &[1] } else { &[] },
            &[
                GraphAggregate::count_rows("rows"),
                GraphAggregate::collect("members", 1),
                GraphAggregate::collect_distinct("unique", 1),
                GraphAggregate::collect("imports", 0),
            ],
        )
        .unwrap();
    if optional {
        outer().optional_apply(body, vec![0, 2]).unwrap()
    } else {
        outer().apply(body, vec![0, 2]).unwrap()
    }
}

fn ordered_collected_right(mask: usize) -> ZSet<GraphValueRow> {
    bag([
        (mask & 1 != 0, row(vec![vertex(100), vertex(9)]), 2),
        (mask & 2 != 0, row(vec![vertex(101), vertex(8)]), 3),
        (mask != 0, row(vec![vertex(102), vertex(8)]), 4),
        (mask != 0, row(vec![vertex(103), vertex(9)]), 1),
        (mask != 0, row(vec![vertex(104), scalar(None)]), 2),
        (mask != 0, row(vec![scalar(None), vertex(6)]), 1),
    ]
    .into_iter()
    .filter_map(|(present, row, weight)| present.then_some((row, weight))))
}

// Literal source-identity order, independent of native comparators, recipes
// and aggregates. NULL ranks lead DESC and follow ASC; equal ranks retain
// the ascending complete-row tie breaker even in the descending profile.
fn ordered_collected_oracle(
    left: &ZSet<GraphValueRow>,
    right: &ZSet<GraphValueRow>,
    optional: bool,
    grouped: bool,
    descending: bool,
) -> Bag {
    let ordinals = if descending {
        [None, Some(101), Some(103), Some(100), Some(102), Some(104)]
    } else {
        [Some(100), Some(102), Some(104), Some(101), Some(103), None]
    };
    let mut output = Bag::new();
    for (outer, multiplicity) in left.iter() {
        let threshold = &outer.values()[0];
        let other = &outer.values()[2];
        let mut groups = BTreeMap::<GraphValue, Vec<(GraphValue, i128)>>::new();
        if !grouped {
            groups.insert(scalar(None), Vec::new());
        }
        for ordinal in ordinals {
            let identity = ordinal.map_or_else(|| scalar(None), vertex);
            for (inner, weight) in right.iter() {
                let member = &inner.values()[1];
                if inner.values()[0] == identity
                    && (other.is_null() || (!member.is_null() && member != other))
                {
                    groups
                        .entry(if grouped {
                            member.clone()
                        } else {
                            scalar(None)
                        })
                        .or_default()
                        .push((member.clone(), weight.to_i128().unwrap()));
                }
            }
        }
        if groups.is_empty() && optional {
            let mut cells = outer.values().to_vec();
            cells.extend(vec![scalar(None); 5]);
            output.insert(row(cells), multiplicity.to_i128().unwrap());
        }
        for (key, occurrences) in groups {
            let mut count = 0_i64;
            let mut members = Vec::new();
            let mut unique = Vec::new();
            let mut imports = Vec::new();
            let mut seen = BTreeSet::new();
            for (member, weight) in occurrences {
                if !member.is_null() && seen.insert(member.clone()) {
                    unique.push(member.clone());
                }
                for _ in 0..weight {
                    count += 1;
                    if !member.is_null() {
                        members.push(member.clone());
                    }
                    if !threshold.is_null() {
                        imports.push(threshold.clone());
                    }
                }
            }
            let mut cells = outer.values().to_vec();
            if grouped {
                cells.push(key);
            }
            cells.extend([
                scalar(Some(count)),
                collected_list(members),
                collected_list(unique),
                collected_list(imports),
            ]);
            output.insert(row(cells), multiplicity.to_i128().unwrap());
        }
    }
    output
}

#[test]
fn ordered_collections_keep_hidden_rank_ties_and_restored_imports() {
    for descending in [false, true] {
        for optional in [false, true] {
            for grouped in [false, true] {
                let query = ordered_collected_summary(optional, grouped, descending);
                for before in 0..16 {
                    for after in 0..16 {
                        let old_left = filtered_left(before & 3);
                        let old_right = ordered_collected_right(before >> 2);
                        let next_left = filtered_left(after & 3);
                        let next_right = ordered_collected_right(after >> 2);
                        let old = ordered_collected_oracle(
                            &old_left,
                            &old_right,
                            optional,
                            grouped,
                            descending,
                        );
                        let next = ordered_collected_oracle(
                            &next_left,
                            &next_right,
                            optional,
                            grouped,
                            descending,
                        );
                        let mut circuit = FilteredCircuit::new(&query);
                        circuit.tick(&old_left, &old_right).unwrap();
                        assert_eq!(plain(circuit.rows()), old, "seed {before}");
                        let delta = circuit
                            .tick(
                                &difference(&next_left, &old_left),
                                &difference(&next_right, &old_right),
                            )
                            .unwrap();
                        assert_eq!(
                            plain(circuit.rows()),
                            next,
                            "descending {descending}, optional {optional}, grouped {grouped}, {before}->{after}"
                        );
                        assert_eq!(
                            delta,
                            bag(next
                                .into_iter()
                                .chain(old.into_iter().map(|(row, n)| (row, -n))))
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn ordered_collection_recipes_preserve_identity_and_every_cancellation_checkpoint() {
    for query in [
        ordered_collected_summary(true, false, false),
        ordered_collected_summary(false, true, true),
    ] {
        let original = query.canonical_bytes();
        let mut calls = 0;
        let recipe = query
            .incremental_filtered_aggregate_with_control(&mut |_| {
                calls += 1;
                Ok::<_, usize>(())
            })
            .unwrap()
            .unwrap();
        assert_eq!(
            recipe.group.input().incremental_result_order().unwrap().0.len(),
            1
        );
        for stop in 1..=calls {
            let mut seen = 0;
            assert!(
                matches!(query.incremental_filtered_aggregate_with_control(&mut |_| {
                    seen += 1;
                    if seen == stop { Err(stop) } else { Ok(()) }
                }), Err(at) if at == stop)
            );
            assert_eq!(seen, stop);
            assert_eq!(query.canonical_bytes(), original);
        }
    }
}

#[test]
fn ordered_collections_keep_input_pages_and_unproved_later_stages_refused() {
    let ordered = || {
        base()
            .with_order_by(&[GraphValueOrder::descending(3)])
            .unwrap()
    };
    let divide = GraphSetValue::Integer(
        GraphIntegerExpression::prepare(&[
            GraphIntegerOp::Literal(Some(1)),
            GraphIntegerOp::Literal(Some(0)),
            GraphIntegerOp::Binary(GraphIntegerBinary::Divide),
        ])
        .unwrap(),
    );
    for body in [
        ordered().with_page(0, Some(1)),
        ordered()
            .filter(&[GraphSetPredicateOp::Truth(Some(true))])
            .unwrap(),
        ordered()
            .unwind(
                "member".into(),
                GraphSetValue::List(vec![GraphSetValue::Column(3)]),
            )
            .unwrap(),
        ordered()
            .project_ordered_output(vec![GraphSetProjection::new("failed", divide)])
            .unwrap(),
        ordered()
            .project(
                vec![GraphSetProjection::new("member", GraphSetValue::Column(3))],
                GraphSetQuantifier::Distinct,
            )
            .unwrap(),
        ordered()
            .nested()
            .unwrap()
            .with_order_by(&[GraphValueOrder::ascending(3)])
            .unwrap(),
    ] {
        let column = body.column_types().len() - 1;
        let body = body
            .group_by(&[], &[GraphAggregate::collect("members", column)])
            .unwrap();
        let query = outer().apply(body, vec![1, 1]).unwrap();
        assert!(
            query
                .incremental_filtered_aggregate_with_control(&mut allow)
                .unwrap()
                .is_none()
        );
    }
}
