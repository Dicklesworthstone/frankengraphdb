use super::*;
use crate::algebra::{GraphColumn, GraphPatternBuilder, GraphValueOrder, IntegerComparison};
use crate::row_join::IncrementalRowJoin;
use crate::row_projection::IncrementalRowProjection;
use crate::{
    GqlParameters, GraphAggregate, GraphIntegerBinary, GraphIntegerExpression, GraphIntegerOp,
    GraphSymbol, GraphSymbolKind, PreparedGraphSetText, PreparedGraphText,
};
use fgdb_delta_types::{LabelId, LimbLimit, PropertyKeyId, RelationId, ZSet, ZWeight};
use fgdb_types::{CanonicalF64, VId};
use std::collections::BTreeMap;

const LIMBS: LimbLimit = LimbLimit::new(4);
type Bag = BTreeMap<GraphValueRow, i128>;

fn allow(_: GlaExecutionEvent) -> Result<(), usize> {
    Ok(())
}

fn leaf() -> PreparedGraphSet {
    PreparedGraphText::prepare("MATCH (n) RETURN n AS id", |_, _: &str| None)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
        .with_duplicates()
        .into()
}

fn outer() -> PreparedGraphSet {
    PreparedGraphText::prepare(
        "MATCH (n) RETURN n.p AS token,n AS owner,n.q AS wanted",
        |kind, name| match (kind, name) {
            (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
            (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(PropertyKeyId(2))),
            _ => None,
        },
    )
    .unwrap()
    .bind_parameters(&GqlParameters::new())
    .unwrap()
    .with_duplicates()
    .into()
}

fn int(value: i64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(value))
}

fn null() -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Null)
}

fn row(values: Vec<GraphValue>) -> GraphValueRow {
    GraphValueRow::from_owned_values(values)
}

fn bag(rows: impl IntoIterator<Item = (GraphValueRow, i128)>) -> ZSet<GraphValueRow> {
    ZSet::from_updates(
        rows.into_iter()
            .map(|(row, weight)| (row, ZWeight::from_i128(weight))),
        LIMBS,
        &mut |_| Ok::<_, ()>(()),
    )
    .unwrap()
}

fn plain(rows: &ZSet<GraphValueRow>) -> Bag {
    rows.iter()
        .map(|(row, weight)| (row.clone(), weight.to_i128().unwrap()))
        .collect()
}

fn difference(after: &ZSet<GraphValueRow>, before: &ZSet<GraphValueRow>) -> ZSet<GraphValueRow> {
    bag(plain(after).into_iter().chain(
        plain(before)
            .into_iter()
            .map(|(row, weight)| (row, -weight)),
    ))
}

fn equal(left: usize, right: usize) -> GraphSetPredicateOp {
    GraphSetPredicateOp::Compare {
        left: GraphSetOperand::Column(left),
        comparison: IntegerComparison::Equal,
        right: GraphSetOperand::Column(right),
    }
}

fn arguments(types: Vec<GraphSetColumnType>) -> PreparedGraphSet {
    PreparedGraphSet::arguments(
        (0..types.len())
            .map(|column| format!("a{column}"))
            .collect(),
        types,
    )
    .unwrap()
}

fn right() -> PreparedGraphSet {
    leaf()
        .project(
            vec![
                GraphSetProjection::new("child", GraphSetValue::Column(0)),
                GraphSetProjection::new("score", GraphSetValue::Value(int(10))),
                GraphSetProjection::new("missing", GraphSetValue::Value(null())),
            ],
            GraphSetQuantifier::All,
        )
        .unwrap()
}

fn division(column: usize) -> GraphSetValue {
    GraphSetValue::Integer(
        GraphIntegerExpression::prepare(&[
            GraphIntegerOp::Literal(Some(12)),
            GraphIntegerOp::Column(column),
            GraphIntegerOp::Binary(GraphIntegerBinary::Divide),
        ])
        .unwrap(),
    )
}

fn selected_body() -> PreparedGraphSet {
    arguments(vec![GraphSetColumnType::Stored; 3])
        .cross_join(right())
        .unwrap()
        .filter(&[equal(0, 4)])
        .unwrap()
        .project(
            vec![
                GraphSetProjection::new("child", GraphSetValue::Column(3)),
                GraphSetProjection::new("copied", GraphSetValue::Column(2)),
                GraphSetProjection::new("literal", GraphSetValue::Value(int(7))),
                GraphSetProjection::new("missing", GraphSetValue::Column(5)),
                GraphSetProjection::new("computed", division(0)),
            ],
            GraphSetQuantifier::All,
        )
        .unwrap()
}

fn query(optional: bool) -> PreparedGraphSet {
    if optional {
        outer().optional_apply(selected_body(), vec![2, 0, 2])
    } else {
        outer().apply(selected_body(), vec![2, 0, 2])
    }
    .unwrap()
}

struct Circuit {
    right: Option<IncrementalRowProjection>,
    join: IncrementalRowJoin,
    output: Vec<IncrementalRowProjection>,
}

impl Circuit {
    fn new(query: &PreparedGraphSet) -> Self {
        let recipe = query
            .incremental_apply_join_with_control(&mut allow)
            .unwrap()
            .unwrap();
        Self {
            right: recipe.right_projection.map(IncrementalRowProjection::new),
            join: IncrementalRowJoin::new(recipe.join),
            output: recipe
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
    ) -> Result<ZSet<GraphValueRow>, ()> {
        let projected = if let Some(projection) = &mut self.right {
            Some(
                projection
                    .prepare(right, LIMBS, None, &mut |_| Ok::<_, ()>(()))
                    .map_err(|_| ())?
                    .commit(),
            )
        } else {
            None
        };
        let mut delta = self
            .join
            .prepare(
                left,
                projected.as_ref().unwrap_or(right),
                LIMBS,
                None,
                &mut |_| Ok::<_, ()>(()),
            )
            .map_err(|_| ())?
            .commit();
        for projection in &mut self.output {
            delta = projection
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

fn left_rows(mask: usize) -> ZSet<GraphValueRow> {
    bag([
        (
            row(vec![int(3), GraphValue::Vertex(VId(u128::MAX)), int(10)]),
            2,
        ),
        (row(vec![int(4), GraphValue::Vertex(VId(17)), int(0)]), 3),
    ]
    .into_iter()
    .enumerate()
    .filter_map(|(at, item)| (mask & (1 << at) != 0).then_some(item)))
}

fn right_rows(mask: usize) -> ZSet<GraphValueRow> {
    bag([
        (row(vec![GraphValue::Vertex(VId(1)), int(10), null()]), 2),
        (
            row(vec![GraphValue::Vertex(VId(u128::MAX)), int(10), null()]),
            3,
        ),
    ]
    .into_iter()
    .enumerate()
    .filter_map(|(at, item)| (mask & (1 << at) != 0).then_some(item)))
}

// Independent full-input integer oracle. It knows neither the recipe's
// private columns nor the row-predicate/expression virtual machines.
fn oracle(left: &ZSet<GraphValueRow>, right: &ZSet<GraphValueRow>, optional: bool) -> Bag {
    let mut output = Bag::new();
    for (a, aw) in left.iter() {
        let GraphValue::Scalar(CanonicalScalar::Int(wanted)) = a.values()[2] else {
            panic!("integer fixture");
        };
        let mut matched = false;
        for (b, bw) in right.iter() {
            let GraphValue::Scalar(CanonicalScalar::Int(score)) = b.values()[1] else {
                panic!("integer fixture");
            };
            if wanted == score {
                matched = true;
                let mut values = a.values().to_vec();
                values.extend([
                    b.values()[0].clone(),
                    int(wanted),
                    int(7),
                    null(),
                    int(12 / wanted),
                ]);
                *output.entry(row(values)).or_default() +=
                    aw.to_i128().unwrap() * bw.to_i128().unwrap();
            }
        }
        if optional && !matched {
            let mut values = a.values().to_vec();
            values.extend([null(), null(), null(), null(), null()]);
            *output.entry(row(values)).or_default() += aw.to_i128().unwrap();
        }
    }
    output
}

#[test]
fn simultaneous_changes_preserve_duplicates_nonprefix_imports_and_optional_absence() {
    for optional in [false, true] {
        let query = query(optional);
        let original = query.canonical_bytes();
        let recipe = query
            .incremental_apply_join_with_control(&mut allow)
            .unwrap()
            .unwrap();
        assert_eq!(recipe.left, &outer());
        assert_eq!(recipe.right, &right());
        assert!(recipe.join.keys().is_empty());
        assert_eq!(
            recipe.join.kind(),
            if optional {
                RowJoinKind::Left
            } else {
                RowJoinKind::Inner
            }
        );
        assert_eq!(
            recipe.output.last().unwrap().columns().collect::<Vec<_>>(),
            query
                .columns()
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
        );
        for before in 0..16 {
            for after in 0..16 {
                let mut circuit = Circuit::new(&query);
                let old_left = left_rows(before & 3);
                let old_right = right_rows(before >> 2);
                circuit.tick(&old_left, &old_right).unwrap();
                assert_eq!(
                    plain(circuit.rows()),
                    oracle(&old_left, &old_right, optional)
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
                    oracle(&next_left, &next_right, optional)
                );
                let expected = bag(oracle(&next_left, &next_right, optional).into_iter().chain(
                    oracle(&old_left, &old_right, optional)
                        .into_iter()
                        .map(|(row, n)| (row, -n)),
                ));
                assert_eq!(delta, expected);
            }
        }
        assert_eq!(query.canonical_bytes(), original);
    }
}

#[test]
fn a_real_all_null_right_row_is_not_absence_and_every_absent_export_is_null() {
    let source = leaf()
        .project(
            vec![GraphSetProjection::new(
                "empty",
                GraphSetValue::Value(null()),
            )],
            GraphSetQuantifier::All,
        )
        .unwrap();
    let body = arguments(vec![GraphSetColumnType::Stored])
        .cross_join(source)
        .unwrap()
        .project(
            vec![
                GraphSetProjection::new("copied", GraphSetValue::Column(0)),
                GraphSetProjection::new("literal", GraphSetValue::Value(int(7))),
                GraphSetProjection::new("empty", GraphSetValue::Column(1)),
            ],
            GraphSetQuantifier::All,
        )
        .unwrap();
    let query = outer().optional_apply(body, vec![2]).unwrap();
    let mut circuit = Circuit::new(&query);
    let left = left_rows(1);
    let right = bag([(row(vec![null()]), 3)]);
    circuit.tick(&left, &right).unwrap();
    let mut matched = left.iter().next().unwrap().0.values().to_vec();
    matched.extend([int(10), int(7), null()]);
    assert_eq!(plain(circuit.rows()), Bag::from([(row(matched), 6)]));
    circuit
        .tick(&ZSet::new(), &bag([(row(vec![null()]), -3)]))
        .unwrap();
    let mut absent = left.iter().next().unwrap().0.values().to_vec();
    absent.extend([null(), null(), null()]);
    assert_eq!(plain(circuit.rows()), Bag::from([(row(absent), 2)]));
}

#[test]
fn optional_lifts_filters_through_reordered_columns_without_losing_null_outer_rows() {
    let body = arguments(vec![GraphSetColumnType::Stored])
        .cross_join(right())
        .unwrap()
        .project(
            vec![
                GraphSetProjection::new("score", GraphSetValue::Column(2)),
                GraphSetProjection::new("wanted", GraphSetValue::Column(0)),
                GraphSetProjection::new("child", GraphSetValue::Column(1)),
            ],
            GraphSetQuantifier::All,
        )
        .unwrap()
        .filter(&[equal(0, 1)])
        .unwrap()
        .project(
            vec![GraphSetProjection::new("child", GraphSetValue::Column(2))],
            GraphSetQuantifier::All,
        )
        .unwrap();
    let query = outer().optional_apply(body, vec![2]).unwrap();
    let mut circuit = Circuit::new(&query);
    let a = row(vec![int(8), GraphValue::Vertex(VId(9)), null()]);
    circuit
        .tick(&bag([(a.clone(), 4)]), &right_rows(3))
        .unwrap();
    let mut expected = a.values().to_vec();
    expected.push(null());
    assert_eq!(plain(circuit.rows()), Bag::from([(row(expected), 4)]));
}

#[test]
fn typed_keys_remain_typed_and_repeated_argument_positions_are_not_deduplicated() {
    let args = arguments(vec![GraphSetColumnType::Vertex; 2]);
    let source = leaf();
    let spec = RowJoinSpec::new(
        args.column_types(),
        source.column_types(),
        &[(0, 0), (1, 0)],
    )
    .unwrap();
    let body = args.join(source, spec).unwrap();
    let query = outer().apply(body, vec![1, 1]).unwrap();
    let recipe = query
        .incremental_apply_join_with_control(&mut allow)
        .unwrap()
        .unwrap();
    assert_eq!(recipe.join.keys(), &[(1, 0), (1, 0)]);
    let mut circuit = Circuit::new(&query);
    circuit
        .tick(
            &left_rows(1),
            &bag([(row(vec![GraphValue::Vertex(VId(u128::MAX))]), 3)]),
        )
        .unwrap();
    assert_eq!(
        circuit
            .rows()
            .total_weight(LIMBS, &mut |_| Ok::<_, ()>(()))
            .unwrap(),
        ZWeight::from_i128(6)
    );
}

#[test]
fn where_numeric_equality_is_not_replaced_by_canonical_typed_identity() {
    let source = leaf()
        .project(
            vec![GraphSetProjection::new(
                "number",
                GraphSetValue::Value(GraphValue::Scalar(CanonicalScalar::Float(
                    CanonicalF64::new(10.0),
                ))),
            )],
            GraphSetQuantifier::All,
        )
        .unwrap();
    let body = arguments(vec![GraphSetColumnType::Stored])
        .cross_join(source)
        .unwrap()
        .filter(&[equal(0, 1)])
        .unwrap();
    let query = outer().apply(body, vec![2]).unwrap();
    let recipe = query
        .incremental_apply_join_with_control(&mut allow)
        .unwrap()
        .unwrap();
    assert!(recipe.join.keys().is_empty());
    let mut circuit = Circuit::new(&query);
    circuit
        .tick(
            &left_rows(1),
            &bag([(
                row(vec![GraphValue::Scalar(CanonicalScalar::Float(
                    CanonicalF64::new(10.0),
                ))]),
                3,
            )]),
        )
        .unwrap();
    assert_eq!(
        circuit
            .rows()
            .total_weight(LIMBS, &mut |_| Ok::<_, ()>(()))
            .unwrap(),
        ZWeight::from_i128(6)
    );
}

#[test]
fn inner_computed_stages_keep_their_order_and_optional_does_not_speculate_through_them() {
    let computed = arguments(vec![GraphSetColumnType::Stored])
        .cross_join(leaf())
        .unwrap()
        .project(
            vec![GraphSetProjection::new("value", division(0))],
            GraphSetQuantifier::All,
        )
        .unwrap()
        .filter(&[GraphSetPredicateOp::Truth(Some(false))])
        .unwrap();
    let inner = outer().apply(computed.clone(), vec![2]).unwrap();
    let optional = outer().optional_apply(computed, vec![2]).unwrap();
    assert!(
        inner
            .incremental_apply_join_with_control(&mut allow)
            .unwrap()
            .is_some()
    );
    assert!(
        optional
            .incremental_apply_join_with_control(&mut allow)
            .unwrap()
            .is_none()
    );
    // A later FALSE cannot erase the earlier division-by-zero failure.
    let mut circuit = Circuit::new(&inner);
    assert!(
        circuit
            .tick(
                &left_rows(2),
                &bag([(row(vec![GraphValue::Vertex(VId(1))]), 1)]),
            )
            .is_err()
    );
}

#[test]
fn conditional_source_effects_and_per_invocation_barriers_refuse_without_execution() {
    let args = || arguments(vec![GraphSetColumnType::Stored]);
    let base = || args().cross_join(leaf()).unwrap();
    let property = PreparedGraphText::prepare("MATCH (n) RETURN n.p AS p", |kind, _| {
        (kind == GraphSymbolKind::Property).then_some(GraphSymbol::Property(PropertyKeyId(1)))
    })
    .unwrap()
    .bind_parameters(&GqlParameters::new())
    .unwrap();
    let mut hidden = GraphPatternBuilder::new();
    hidden.vertex("n").unwrap();
    let hidden = hidden
        .prepare_values(
            &[
                GraphColumn::vertex("id", "n"),
                GraphColumn::property("hidden", "n", PropertyKeyId(1)),
            ],
            0,
            None,
        )
        .unwrap()
        .with_duplicates()
        .with_visible_columns(1);
    let fallible_source = leaf().project(
        vec![GraphSetProjection::new("failure", division(0))],
        GraphSetQuantifier::All,
    );
    // A graph identity is not scalar; use an admitted scalar literal column
    // before the failing computation to test effect admission, not type checks.
    assert!(fallible_source.is_err());
    let fallible_source = leaf()
        .project(
            vec![GraphSetProjection::new(
                "zero",
                GraphSetValue::Value(int(0)),
            )],
            GraphSetQuantifier::All,
        )
        .unwrap()
        .project(
            vec![GraphSetProjection::new("failure", division(0))],
            GraphSetQuantifier::All,
        )
        .unwrap();
    let random = base()
        .project(
            vec![GraphSetProjection::new(
                "random",
                GraphSetValue::Case {
                    arms: vec![(
                        GraphSetValue::Value(GraphValue::Scalar(CanonicalScalar::Bool(true))),
                        GraphSetValue::Value(int(1)),
                    )],
                    otherwise: Some(Box::new(GraphSetValue::Random { site: 0, scope: 0 })),
                },
            )],
            GraphSetQuantifier::All,
        )
        .unwrap();
    for body in [
        base().with_page(0, Some(0)),
        base().with_page(0, Some(1)),
        base()
            .with_order_by(&[GraphValueOrder::ascending(0)])
            .unwrap(),
        base()
            .project(
                vec![GraphSetProjection::new("x", GraphSetValue::Column(0))],
                GraphSetQuantifier::Distinct,
            )
            .unwrap(),
        base()
            .group_by(&[], &[GraphAggregate::count_rows("n")])
            .unwrap(),
        base()
            .apply(
                arguments(vec![GraphSetColumnType::Stored, GraphSetColumnType::Vertex]),
                vec![0, 1],
            )
            .unwrap(),
        base()
            .unwind("item".into(), GraphSetValue::List(vec![]))
            .unwrap(),
        args().cross_join(property.into()).unwrap(),
        args().cross_join(hidden.into()).unwrap(),
        args().cross_join(fallible_source).unwrap(),
        random,
    ] {
        let query = outer().apply(body, vec![2]).unwrap();
        assert!(
            query
                .incremental_apply_join_with_control(&mut allow)
                .unwrap()
                .is_none()
        );
    }
    let collected = outer()
        .collect_apply(arguments(vec![GraphSetColumnType::Stored]), vec![2], 2)
        .unwrap();
    assert!(
        collected
            .incremental_apply_join_with_control(&mut allow)
            .unwrap()
            .is_none()
    );
    let no_imports = outer().apply(leaf(), vec![]).unwrap();
    assert!(
        no_imports
            .incremental_apply_join_with_control(&mut allow)
            .unwrap()
            .is_some()
    );
    assert!(
        no_imports
            .with_page(0, Some(0))
            .incremental_apply_join_with_control(&mut allow)
            .unwrap()
            .is_none()
    );
}

#[test]
fn every_preparation_checkpoint_preserves_the_original_definition() {
    let query = query(true);
    let frozen = query.canonical_bytes();
    let mut count = 0;
    let expected = query
        .incremental_apply_join_with_control(&mut |_| {
            count += 1;
            Ok::<_, usize>(())
        })
        .unwrap()
        .unwrap();
    for stop in 1..=count {
        let mut seen = 0;
        let result = query.incremental_apply_join_with_control(&mut |_| {
            seen += 1;
            if seen == stop { Err(stop) } else { Ok(()) }
        });
        assert!(matches!(result, Err(at) if at == stop));
        assert_eq!(seen, stop);
        assert_eq!(query.canonical_bytes(), frozen);
    }
    assert_eq!(
        query
            .incremental_apply_join_with_control(&mut allow)
            .unwrap()
            .unwrap(),
        expected
    );
}

#[test]
fn unpaged_native_call_passes_the_shared_sequence_gate_but_real_pages_stay_barriers() {
    let query = PreparedGraphSetText::prepare(
        "MATCH (a:L) UNWIND [7,7] AS token CALL (a) { MATCH (a)-[:R]->(b) RETURN b AS child } RETURN token,a,child",
        |kind, name| match (kind, name) {
            (GraphSymbolKind::Label, "L") => Some(GraphSymbol::Label(LabelId(1))),
            (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(RelationId(1))),
            _ => None,
        },
    ).unwrap().bind_parameters(&GqlParameters::new()).unwrap();
    assert!(query.incremental_window_sequence_compatible());
    let mut node = &query;
    loop {
        match &node.node {
            SetNode::Scope(input)
            | SetNode::Project { input, .. }
            | SetNode::Filter { input, .. }
            | SetNode::Unwind { input, .. } => node = input,
            SetNode::Apply { .. } => {
                assert!(
                    node.incremental_apply_join_with_control(&mut allow)
                        .unwrap()
                        .is_some()
                );
                break;
            }
            _ => panic!("the returning CALL must remain a real Apply"),
        }
    }
    for paged in [
        query.clone().with_page(0, Some(0)),
        query.clone().with_page(1, None),
        outer()
            .apply(selected_body().with_page(0, Some(0)), vec![2, 0, 2])
            .unwrap(),
        outer()
            .apply(
                selected_body()
                    .with_order_by(&[GraphValueOrder::ascending(0)])
                    .unwrap(),
                vec![2, 0, 2],
            )
            .unwrap(),
    ] {
        assert!(!paged.incremental_window_sequence_compatible());
        assert!(paged.incremental_window().unwrap().is_none());
    }
    let hidden = query
        .with_order_by(&[GraphValueOrder::ascending(0)])
        .unwrap()
        .with_page(0, Some(2))
        .project_ordered_output(vec![GraphSetProjection::new(
            "child",
            GraphSetValue::Column(2),
        )])
        .unwrap();
    assert!(!hidden.incremental_window_sequence_compatible());
    assert!(
        hidden
            .with_page(0, Some(1))
            .incremental_window()
            .unwrap()
            .is_none()
    );
    // A frame is positional but is still not a standalone maintained source.
    let standalone = arguments(vec![GraphSetColumnType::Stored]);
    assert!(
        standalone
            .incremental_apply_join_with_control(&mut allow)
            .unwrap()
            .is_none()
    );
    assert!(standalone.incremental_pattern().is_none());
    assert!(
        standalone
            .with_page(0, Some(0))
            .incremental_window()
            .unwrap()
            .is_none()
    );
}

#[test]
fn typed_null_guard_keeps_proven_domains_and_does_not_touch_carried_columns() {
    let spec = RowProjectionSpec::new(
        vec![
            GraphSetColumnType::Scalar,
            GraphSetColumnType::Vertex,
            GraphSetColumnType::Stored,
        ],
        vec![
            GraphSetProjection::new("id", GraphSetValue::Column(1)),
            GraphSetProjection::new("copied", GraphSetValue::Column(2)),
            GraphSetProjection::new("literal", GraphSetValue::Value(int(7))),
            GraphSetProjection::new("quotient", division(2)),
            GraphSetProjection::new("carried", GraphSetValue::Column(2)),
        ],
        GraphSetQuantifier::All,
    )
    .unwrap();
    let guarded = spec.clone().with_null_guard(0, 0..4).unwrap();
    assert_eq!(guarded.column_types(), spec.column_types());
    assert_eq!(
        guarded.column_types(),
        &[
            GraphSetColumnType::Vertex,
            GraphSetColumnType::Stored,
            GraphSetColumnType::Scalar,
            GraphSetColumnType::Scalar,
            GraphSetColumnType::Stored,
        ]
    );
    let mut state = IncrementalRowProjection::new(guarded);
    state
        .prepare(
            &bag([
                (
                    row(vec![
                        GraphValue::Scalar(CanonicalScalar::Bool(true)),
                        GraphValue::Vertex(VId(u128::MAX)),
                        int(10),
                    ]),
                    2,
                ),
                (row(vec![null(), GraphValue::Vertex(VId(1)), int(0)]), 3),
            ]),
            LIMBS,
            None,
            &mut |_| Ok::<_, ()>(()),
        )
        .unwrap()
        .commit();
    assert_eq!(
        plain(state.rows()),
        Bag::from([
            (
                row(vec![
                    GraphValue::Vertex(VId(u128::MAX)),
                    int(10),
                    int(7),
                    int(1),
                    int(10)
                ]),
                2
            ),
            (row(vec![null(), null(), null(), null(), int(0)]), 3),
        ])
    );
    assert!(spec.clone().with_null_guard(1, 0..4).is_err());
    assert!(spec.clone().with_null_guard(9, 0..4).is_err());
    for range in [core::ops::Range { start: 3, end: 2 }, 0..6] {
        assert!(spec.clone().with_null_guard(0, range).is_err());
    }
    let expanded = RowProjectionSpec::unwind(
        vec![GraphSetColumnType::Scalar],
        vec!["guard".into()],
        "item".into(),
        GraphSetValue::List(vec![GraphSetValue::Value(int(1))]),
    )
    .unwrap();
    assert!(expanded.with_null_guard(0, 0..1).is_err());
    let open = RowProjectionSpec::new(
        vec![GraphSetColumnType::Scalar],
        vec![GraphSetProjection::new(
            "open",
            GraphSetValue::TypedNull(GraphSetColumnType::Any),
        )],
        GraphSetQuantifier::All,
    )
    .unwrap()
    .with_null_guard(0, 0..1)
    .unwrap();
    assert_eq!(open.column_types(), &[GraphSetColumnType::Any]);
}
