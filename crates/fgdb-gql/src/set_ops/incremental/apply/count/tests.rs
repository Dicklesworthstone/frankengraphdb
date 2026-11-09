//! Correlated COUNT(*) laws over full weighted bags.
//! Expected counts are summed independently; the circuit uses native operators.

use super::*;
use crate::algebra::{GraphValueRow, GraphValueOrder, IntegerComparison};
use crate::row_aggregate::definition::operator::IncrementalGroupAggregate;
use crate::row_join::IncrementalRowJoin;
use crate::row_projection::IncrementalRowProjection;
use crate::spill_set::SpillSetAggregateRowError;
use crate::{
    GqlParameters, GraphAggregateError, GraphIntegerBinary, GraphIntegerError,
    GraphIntegerErrorKind, GraphSymbol, GraphSymbolKind, PreparedGraphSetText,
    PreparedGraphText,
};
use fgdb_delta_types::{LabelId, LimbLimit, PropertyKeyId, RelationId, ZSet, ZWeight};
use fgdb_types::VId;
use std::collections::BTreeMap;

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
        rows.into_iter().map(|(row, count)| (row, ZWeight::from_i128(count))),
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
    bag(plain(after).into_iter().chain(
        plain(before).into_iter().map(|(row, count)| (row, -count)),
    ))
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
        (0..types.len()).map(|column| format!("arg{column}")).collect(),
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
    body.group_by(&[], &[GraphAggregate::count_rows("token")]).unwrap()
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
            output: plan.output.into_iter().map(IncrementalRowProjection::new).collect(),
        }
    }

    fn tick(
        &mut self,
        left: &ZSet<GraphValueRow>,
        right: &ZSet<GraphValueRow>,
    ) -> Result<ZSet<GraphValueRow>, SpillSetAggregateRowError<usize>> {
        let selected = self.restrict
            .prepare(right, left, LIMBS, None, &mut |_| Ok::<_, ()>(()))
            .unwrap();
        let grouped = self.group
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
        let joined = self.join
            .prepare(left, &values, LIMBS, None, &mut |_| Ok::<_, ()>(()))
            .unwrap();
        let mut delta = joined.delta()
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
    ].into_iter().enumerate().filter_map(|(index, item)| {
        (mask & (1 << index) != 0).then_some(item)
    }))
}

fn right_rows(mask: usize) -> ZSet<GraphValueRow> {
    bag([
        (row(vec![vertex(1), vertex(8)]), 2),
        (row(vec![vertex(1), vertex(9)]), 3),
    ].into_iter().enumerate().filter_map(|(index, item)| {
        (mask & (1 << index) != 0).then_some(item)
    }))
}

fn oracle(
    left: &ZSet<GraphValueRow>,
    right: &ZSet<GraphValueRow>,
    keys: &[(usize, usize)],
) -> Bag {
    let mut output = Bag::new();
    for (outer, weight) in left.iter() {
        let count: i128 = right.iter()
            .filter(|(inner, _)| keys.iter().all(|&(a, b)| {
                !outer.values()[a].is_null()
                    && !inner.values()[b].is_null()
                    && outer.values()[a] == inner.values()[b]
            }))
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
            assert_eq!(plain(circuit.rows()), oracle(&old_left, &old_right, &[(1, 0)]));
            let next_left = left_rows(after & 3);
            let next_right = right_rows(after >> 2);
            let delta = circuit.tick(
                &difference(&next_left, &old_left),
                &difference(&next_right, &old_right),
            ).unwrap();
            assert_eq!(plain(circuit.rows()), oracle(&next_left, &next_right, &[(1, 0)]));
            let expected = bag(oracle(&next_left, &next_right, &[(1, 0)]).into_iter().chain(
                oracle(&old_left, &old_right, &[(1, 0)])
                    .into_iter()
                    .map(|(row, count)| (row, -count)),
            ));
            assert_eq!(delta, expected);
        }
    }
}

#[test]
fn repeated_rhs_keys_keep_every_outer_constraint_and_nulls_receive_zero() {
    let query = outer().apply(counted(base()), vec![1, 2]).unwrap();
    let plan = query.incremental_apply_count_with_control(&mut allow).unwrap().unwrap();
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
    assert_eq!(plain(circuit.rows()), oracle(&left, &right, &[(1, 0), (2, 0)]));
    assert_eq!(plain(circuit.restrict.rows()), Bag::from([
        (row(vec![vertex(1), vertex(8)]), 3),
        (row(vec![vertex(2), vertex(9)]), 7),
    ]));
    circuit.tick(&ZSet::new(), &difference(&ZSet::new(), &right)).unwrap();
    assert_eq!(plain(circuit.rows()), oracle(&left, &ZSet::new(), &[(1, 0), (2, 0)]));
}

#[test]
fn keyless_counts_include_null_rows_and_keep_one_zero_per_outer_occurrence() {
    let source = right().project(
        vec![GraphSetProjection::new("missing", GraphSetValue::Value(scalar(None)))],
        GraphSetQuantifier::All,
    ).unwrap();
    let body = arguments(vec![]).cross_join(source).unwrap();
    let query = outer().apply(counted(body), vec![]).unwrap();
    let mut circuit = Circuit::new(&query);
    let left = left_rows(3);
    circuit.tick(&left, &ZSet::new()).unwrap();
    assert_eq!(plain(circuit.rows()), oracle(&left, &ZSet::new(), &[]));
    let right = bag([(row(vec![scalar(None)]), 7)]);
    circuit.tick(&ZSet::new(), &right).unwrap();
    assert_eq!(plain(circuit.rows()), oracle(&left, &right, &[]));
    circuit.tick(&difference(&ZSet::new(), &left), &ZSet::new()).unwrap();
    assert!(circuit.rows().is_empty());
    circuit.tick(&left, &ZSet::new()).unwrap();
    assert_eq!(plain(circuit.rows()), oracle(&left, &right, &[]));
    circuit.tick(&ZSet::new(), &difference(&ZSet::new(), &right)).unwrap();
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
        let old_groups = circuit.group.rows()
            .checked_clone(LIMBS, &mut |_| Ok::<_, ()>(())).unwrap();
        let error = circuit.tick(&left, &ZSet::new()).unwrap_err();
        assert!(matches!(
            error,
            SpillSetAggregateRowError::Value(GraphAggregateError::OutputExpression {
                error: GraphIntegerError { kind: GraphIntegerErrorKind::Overflow, .. },
                ..
            })
        ));
        assert!(circuit.rows().is_empty());
        assert!(circuit.restrict.rows().is_empty());
        assert_eq!(circuit.group.rows(), &old_groups);
        // The failed outer insertion was not published. Simultaneously bring
        // the previously stored huge RHS down to two and retry that insertion.
        circuit.tick(
            &left,
            &bag([(row(vec![vertex(1), vertex(8)]), 2 - huge)]),
        ).unwrap();
        assert_eq!(plain(circuit.rows()), oracle(
            &left, &bag([(row(vec![vertex(1), vertex(8)]), 2)]), &[],
        ));

        let mut leaving = Circuit::new(&query);
        let one = bag([(row(vec![vertex(1), vertex(8)]), 1)]);
        leaving.tick(&left, &one).unwrap();
        // Final active keys are empty: an intermediate count of huge must
        // never be converted before the simultaneous outer retraction.
        leaving.tick(
            &difference(&ZSet::new(), &left),
            &bag([(row(vec![vertex(1), vertex(8)]), huge - 1)]),
        ).unwrap();
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
    let literal = base().project(
        vec![GraphSetProjection::new("unused", GraphSetValue::Value(scalar(Some(9))))],
        GraphSetQuantifier::All,
    ).unwrap();
    let admitted = outer().apply(
        counted(literal).project(
            vec![GraphSetProjection::new("renamed", GraphSetValue::Column(0))],
            GraphSetQuantifier::All,
        ).unwrap(),
        vec![1, 1],
    ).unwrap();
    assert!(admitted.incremental_apply_count_with_control(&mut allow).unwrap().is_some());

    let fallible = GraphSetValue::Integer(GraphIntegerExpression::prepare(&[
        GraphIntegerOp::Literal(Some(12)),
        GraphIntegerOp::Literal(Some(0)),
        GraphIntegerOp::Binary(GraphIntegerBinary::Divide),
    ]).unwrap());
    let args = arguments(vec![GraphSetColumnType::Vertex; 2]);
    let source = right();
    let on = RowJoinSpec::new(args.column_types(), source.column_types(), &[(0, 0)])
        .unwrap()
        .with_predicate(&[equal(0, 2)])
        .unwrap();
    let property = PreparedGraphText::prepare("MATCH (n) RETURN n.p AS payload", symbols)
        .unwrap().bind_parameters(&GqlParameters::new()).unwrap().with_duplicates();
    for body in [
        counted(base().with_page(0, Some(1))),
        counted(base().with_order_by(&[GraphValueOrder::ascending(0)]).unwrap()),
        counted(base().filter(&[GraphSetPredicateOp::Truth(Some(false))]).unwrap()),
        counted(args.join(source, on).unwrap()),
        counted(base().project(
            vec![GraphSetProjection::new("unused", fallible.clone())],
            GraphSetQuantifier::All,
        ).unwrap()),
        counted(base().project(
            vec![GraphSetProjection::new("id", GraphSetValue::Column(2))],
            GraphSetQuantifier::Distinct,
        ).unwrap()),
        base().group_by(&[], &[GraphAggregate::count("nonnull", 2)]).unwrap(),
        base().group_by(&[2], &[GraphAggregate::count_rows("rows")]).unwrap(),
        counted(base()).with_page(0, Some(0)),
        counted(base()).project(
            vec![GraphSetProjection::new("changed", fallible)],
            GraphSetQuantifier::All,
        ).unwrap(),
        counted(arguments(vec![GraphSetColumnType::Vertex; 2])
            .cross_join(property.into()).unwrap()),
    ] {
        let query = outer().apply(body, vec![1, 1]).unwrap();
        assert!(query.incremental_apply_count_with_control(&mut allow).unwrap().is_none());
    }
    let score = right().project(
        vec![GraphSetProjection::new("score", GraphSetValue::Value(scalar(Some(10))))],
        GraphSetQuantifier::All,
    ).unwrap();
    let numeric = arguments(vec![GraphSetColumnType::Stored])
        .cross_join(score).unwrap()
        .filter(&[equal(0, 1)]).unwrap();
    let query = outer().apply(counted(numeric), vec![0]).unwrap();
    assert!(query.incremental_apply_count_with_control(&mut allow).unwrap().is_none());
    let optional = outer().optional_apply(counted(base()), vec![1, 1]).unwrap();
    assert!(optional.incremental_apply_count_with_control(&mut allow).unwrap().is_none());
}

fn admitted_apply(query: &PreparedGraphSet) -> bool {
    if query.incremental_apply_count_with_control(&mut allow).unwrap().is_some() {
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
        "MATCH (a:L) WITH a CALL (a) { MATCH (a)-[:R]->(b) RETURN count(*) AS c } RETURN a,c",
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
    let query = query();
    let original = query.canonical_bytes();
    let mut calls = 0;
    let plan = query.incremental_apply_count_with_control(&mut |_| {
        calls += 1;
        Ok::<_, usize>(())
    }).unwrap().unwrap();
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
        query.columns().iter().map(String::as_str).collect::<Vec<_>>(),
    );
    assert_eq!(output.column_types().last(), Some(&GraphSetColumnType::Scalar));
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
