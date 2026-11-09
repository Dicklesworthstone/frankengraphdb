use super::*;
use crate::algebra::IntegerComparison;
use crate::row_join::IncrementalRowJoin;
use crate::row_projection::IncrementalRowProjection;
use crate::{GqlParameters, GraphIntegerBinary, PreparedGraphText};
use fgdb_delta_types::{LimbLimit, ZSet, ZWeight};
use fgdb_types::CanonicalF64;
use std::collections::{BTreeMap, BTreeSet};

const LIMBS: LimbLimit = LimbLimit::new(4);
type Bag = BTreeMap<GraphValueRow, i128>;

fn allow(_: GlaExecutionEvent) -> Result<(), usize> {
    Ok(())
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

fn leaf() -> PreparedGraphSet {
    PreparedGraphText::prepare("MATCH (n) RETURN n AS id", |_, _: &str| None)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
        .with_duplicates()
        .into()
}

// The circuit receives the complete native source bags. These total source
// projections admit each fixture schema without a stored-property read that
// the conditional-effect proof deliberately excludes from a shared RHS.
fn source(prefix: &str, types: &[GraphSetColumnType]) -> PreparedGraphSet {
    leaf()
        .project(
            types
                .iter()
                .enumerate()
                .map(|(column, kind)| {
                    let value = match kind {
                        GraphSetColumnType::Scalar => GraphSetValue::Value(int(0)),
                        GraphSetColumnType::Vertex => GraphSetValue::Column(0),
                        GraphSetColumnType::Edge => GraphSetValue::Value(GraphValue::Edge(EId(1))),
                        _ => GraphSetValue::TypedNull(*kind),
                    };
                    GraphSetProjection::new(format!("{prefix}{column}"), value)
                })
                .collect(),
            GraphSetQuantifier::All,
        )
        .unwrap()
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

fn equal(left: usize, right: usize) -> GraphSetPredicateOp {
    GraphSetPredicateOp::Compare {
        left: GraphSetOperand::Column(left),
        comparison: IntegerComparison::Equal,
        right: GraphSetOperand::Column(right),
    }
}

struct Circuit {
    outer: IncrementalRowProjection,
    arguments: IncrementalRowProjection,
    join: IncrementalRowJoin,
    body: Vec<IncrementalRowProjection>,
    restore: IncrementalRowJoin,
    output: Vec<IncrementalRowProjection>,
}

impl Circuit {
    fn new(query: &PreparedGraphSet) -> Self {
        let recipe = query
            .incremental_apply_distinct_with_control(&mut allow)
            .unwrap()
            .expect("the proved DISTINCT invocation must admit");
        Self {
            outer: IncrementalRowProjection::new(recipe.outer),
            arguments: IncrementalRowProjection::new(recipe.arguments),
            join: IncrementalRowJoin::new(recipe.join),
            body: recipe
                .body
                .into_iter()
                .map(IncrementalRowProjection::new)
                .collect(),
            restore: IncrementalRowJoin::new(recipe.restore),
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
        for projection in &mut self.body {
            delta = projection
                .prepare(&delta, LIMBS, None, &mut |_| Ok::<_, ()>(()))
                .map_err(|_| ())?
                .commit();
        }
        delta = self
            .restore
            .prepare(&outer, &delta, LIMBS, None, &mut |_| Ok::<_, ()>(()))
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

fn query(optional: bool) -> PreparedGraphSet {
    let outer = source(
        "outer",
        &[
            GraphSetColumnType::Stored,
            GraphSetColumnType::Vertex,
            GraphSetColumnType::Stored,
        ],
    );
    let args = arguments(vec![GraphSetColumnType::Stored; 3])
        .project(
            vec![
                GraphSetProjection::new("token", GraphSetValue::Column(1)),
                GraphSetProjection::new("wanted", GraphSetValue::Column(2)),
                GraphSetProjection::new("again", GraphSetValue::Column(0)),
            ],
            GraphSetQuantifier::All,
        )
        .unwrap();
    let right = source(
        "right",
        &[
            GraphSetColumnType::Vertex,
            GraphSetColumnType::Stored,
            GraphSetColumnType::Edge,
        ],
    );
    let body = args
        .cross_join(right)
        .unwrap()
        .filter(&[equal(1, 4)])
        .unwrap()
        .project(
            vec![
                GraphSetProjection::new("child", GraphSetValue::Column(3)),
                GraphSetProjection::new("token", GraphSetValue::Column(0)),
                GraphSetProjection::new("copied", GraphSetValue::Column(2)),
            ],
            GraphSetQuantifier::Distinct,
        )
        .unwrap();
    if optional {
        outer.optional_apply(body, vec![2, 0, 2]).unwrap()
    } else {
        outer.apply(body, vec![2, 0, 2]).unwrap()
    }
}

fn left_rows(mask: usize) -> ZSet<GraphValueRow> {
    bag([
        (row(vec![int(3), GraphValue::Vertex(VId(17)), int(10)]), 2),
        (row(vec![int(3), GraphValue::Vertex(VId(19)), int(10)]), 3),
    ]
    .into_iter()
    .enumerate()
    .filter_map(|(at, item)| (mask & (1 << at) != 0).then_some(item)))
}

fn right_rows(mask: usize) -> ZSet<GraphValueRow> {
    bag([
        (
            row(vec![
                GraphValue::Vertex(VId(1)),
                int(10),
                GraphValue::Edge(EId(11)),
            ]),
            2,
        ),
        (
            row(vec![
                GraphValue::Vertex(VId(1)),
                int(10),
                GraphValue::Edge(EId(12)),
            ]),
            3,
        ),
        (
            row(vec![
                GraphValue::Vertex(VId(2)),
                int(10),
                GraphValue::Edge(EId(13)),
            ]),
            5,
        ),
    ]
    .into_iter()
    .enumerate()
    .filter_map(|(at, item)| (mask & (1 << at) != 0).then_some(item)))
}

// Enumerate complete source occurrences independently of the recipe's private
// keys. DISTINCT belongs to each invocation's export; the original outer
// multiplicity is restored only after deduplicating the selected children.
fn oracle(left: &ZSet<GraphValueRow>, right: &ZSet<GraphValueRow>, optional: bool) -> Bag {
    let mut output = Bag::new();
    for (a, weight) in left.iter() {
        let children: BTreeSet<_> = right
            .iter()
            .filter(|(b, _)| b.values()[1] == a.values()[2])
            .map(|(b, _)| b.values()[0].clone())
            .collect();
        if optional && children.is_empty() {
            let mut values = a.values().to_vec();
            values.extend([null(), null(), null()]);
            output.insert(row(values), weight.to_i128().unwrap());
        } else {
            for child in children {
                let mut values = a.values().to_vec();
                values.extend([child, a.values()[0].clone(), a.values()[2].clone()]);
                output.insert(row(values), weight.to_i128().unwrap());
            }
        }
    }
    output
}

#[test]
fn every_weighted_transition_preserves_outer_occurrences_and_per_invocation_distinct() {
    for optional in [false, true] {
        let query = query(optional);
        let frozen = query.canonical_bytes();
        let recipe = query
            .incremental_apply_distinct_with_control(&mut allow)
            .unwrap()
            .unwrap();
        assert_eq!(
            recipe.restore.kind(),
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
        for before in 0..32 {
            for after in 0..32 {
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
                        .map(|(row, weight)| (row, -weight)),
                ));
                assert_eq!(delta, expected);
            }
        }
        assert_eq!(query.canonical_bytes(), frozen);
    }
}

#[test]
fn null_invocation_keys_do_not_collide_with_scalar_or_element_fillers() {
    for (kind, values) in [
        (
            GraphSetColumnType::Scalar,
            vec![
                null(),
                GraphValue::Scalar(CanonicalScalar::Bool(false)),
                int(0),
                GraphValue::Scalar(CanonicalScalar::Float(CanonicalF64::new(0.0))),
            ],
        ),
        (
            GraphSetColumnType::Stored,
            vec![
                null(),
                GraphValue::Scalar(CanonicalScalar::Bool(false)),
                GraphValue::List(vec![int(0)].into_boxed_slice()),
                GraphValue::map(vec![("p".into(), int(0))]).unwrap(),
            ],
        ),
        (
            GraphSetColumnType::Vertex,
            vec![
                null(),
                GraphValue::Vertex(VId(0)),
                GraphValue::Vertex(VId(1)),
            ],
        ),
        (
            GraphSetColumnType::Edge,
            vec![null(), GraphValue::Edge(EId(0)), GraphValue::Edge(EId(1))],
        ),
    ] {
        let body = arguments(vec![kind])
            .cross_join(leaf())
            .unwrap()
            .project(
                vec![
                    GraphSetProjection::new("copied", GraphSetValue::Column(0)),
                    GraphSetProjection::new("child", GraphSetValue::Column(1)),
                ],
                GraphSetQuantifier::Distinct,
            )
            .unwrap();
        let query = source("outer", &[GraphSetColumnType::Scalar, kind])
            .apply(body, vec![1])
            .unwrap();
        let input = bag(values
            .iter()
            .enumerate()
            .map(|(at, value)| (row(vec![int(at as i64), value.clone()]), at as i128 + 2)));
        let child = GraphValue::Vertex(VId(99));
        let expected: Bag = values
            .iter()
            .enumerate()
            .map(|(at, value)| {
                (
                    row(vec![
                        int(at as i64),
                        value.clone(),
                        value.clone(),
                        child.clone(),
                    ]),
                    at as i128 + 2,
                )
            })
            .collect();
        let mut circuit = Circuit::new(&query);
        circuit
            .tick(&input, &bag([(row(vec![child.clone()]), 11)]))
            .unwrap();
        assert_eq!(plain(circuit.rows()), expected, "{kind:?}");
        // Losing all but one duplicate child cannot retract a DISTINCT row.
        assert!(
            circuit
                .tick(&ZSet::new(), &bag([(row(vec![child.clone()]), -10)]))
                .unwrap()
                .is_empty()
        );
        assert_eq!(plain(circuit.rows()), expected);
        circuit
            .tick(&ZSet::new(), &bag([(row(vec![child]), -1)]))
            .unwrap();
        assert!(circuit.rows().is_empty());
    }
}

#[test]
fn an_all_projection_after_distinct_retains_multiplicity_and_real_null_witnesses() {
    let body = arguments(vec![GraphSetColumnType::Scalar])
        .cross_join(leaf())
        .unwrap()
        .project(
            vec![GraphSetProjection::new("child", GraphSetValue::Column(1))],
            GraphSetQuantifier::Distinct,
        )
        .unwrap()
        .project(
            vec![GraphSetProjection::new(
                "value",
                GraphSetValue::Value(null()),
            )],
            GraphSetQuantifier::All,
        )
        .unwrap();
    let query = source("outer", &[GraphSetColumnType::Scalar])
        .optional_apply(body, vec![0])
        .unwrap();
    let left = bag([(row(vec![int(7)]), 3)]);
    let first = row(vec![GraphValue::Vertex(VId(1))]);
    let second = row(vec![GraphValue::Vertex(VId(2))]);
    let result = row(vec![int(7), null()]);
    let mut circuit = Circuit::new(&query);
    circuit
        .tick(&left, &bag([(first.clone(), 5), (second.clone(), 9)]))
        .unwrap();
    assert_eq!(plain(circuit.rows()), Bag::from([(result.clone(), 6)]));
    let delta = circuit
        .tick(&ZSet::new(), &bag([(first.clone(), -5)]))
        .unwrap();
    assert_eq!(plain(&delta), Bag::from([(result.clone(), -3)]));
    assert_eq!(plain(circuit.rows()), Bag::from([(result.clone(), 3)]));
    // The last real NULL row is replaced by one optional NULL row. Public
    // bytes and weight do not change, but its internal absence state must.
    assert!(
        circuit
            .tick(&ZSet::new(), &bag([(second, -9)]))
            .unwrap()
            .is_empty()
    );
    assert_eq!(plain(circuit.rows()), Bag::from([(result.clone(), 3)]));
    assert!(
        circuit
            .tick(&ZSet::new(), &bag([(first, 7)]))
            .unwrap()
            .is_empty()
    );
    assert_eq!(plain(circuit.rows()), Bag::from([(result, 3)]));
}

#[test]
fn optional_empty_and_filtered_bodies_never_evaluate_a_fabricated_export() {
    let divide = GraphSetValue::Integer(
        GraphIntegerExpression::prepare(&[
            GraphIntegerOp::Literal(Some(12)),
            GraphIntegerOp::Column(0),
            GraphIntegerOp::Binary(GraphIntegerBinary::Divide),
        ])
        .unwrap(),
    );
    let body = arguments(vec![GraphSetColumnType::Scalar])
        .cross_join(leaf())
        .unwrap()
        .project(
            vec![GraphSetProjection::new("computed", divide)],
            GraphSetQuantifier::Distinct,
        )
        .unwrap()
        .filter(&[GraphSetPredicateOp::Truth(Some(false))])
        .unwrap()
        .project(
            vec![GraphSetProjection::new(
                "literal",
                GraphSetValue::Value(int(7)),
            )],
            GraphSetQuantifier::All,
        )
        .unwrap();
    let query = source("outer", &[GraphSetColumnType::Scalar])
        .optional_apply(body, vec![0])
        .unwrap();
    let zero = bag([(row(vec![int(0)]), 3)]);
    let child = bag([(row(vec![GraphValue::Vertex(VId(1))]), 5)]);
    let mut empty = Circuit::new(&query);
    empty.tick(&zero, &ZSet::new()).unwrap();
    assert_eq!(
        plain(empty.rows()),
        Bag::from([(row(vec![int(0), null()]), 3)])
    );
    let mut filtered = Circuit::new(&query);
    filtered
        .tick(&bag([(row(vec![int(4)]), 2)]), &child)
        .unwrap();
    assert_eq!(
        plain(filtered.rows()),
        Bag::from([(row(vec![int(4), null()]), 2)])
    );
    let mut failure = Circuit::new(&query);
    assert!(
        failure.tick(&zero, &child).is_err(),
        "a later FALSE must not erase division by zero"
    );
}

#[test]
fn no_import_invocation_activates_once_and_restores_every_outer_occurrence() {
    let body = leaf()
        .project(
            vec![GraphSetProjection::new(
                "value",
                GraphSetValue::Value(int(7)),
            )],
            GraphSetQuantifier::Distinct,
        )
        .unwrap();
    let query = source("outer", &[GraphSetColumnType::Scalar])
        .apply(body, vec![])
        .unwrap();
    let right = bag([(row(vec![GraphValue::Vertex(VId(1))]), 13)]);
    let left = bag([(row(vec![int(2)]), 3), (row(vec![int(4)]), 5)]);
    let mut circuit = Circuit::new(&query);
    circuit.tick(&ZSet::new(), &right).unwrap();
    assert!(circuit.rows().is_empty());
    circuit.tick(&left, &ZSet::new()).unwrap();
    assert_eq!(
        plain(circuit.rows()),
        Bag::from([
            (row(vec![int(2), int(7)]), 3),
            (row(vec![int(4), int(7)]), 5)
        ])
    );
    circuit
        .tick(&difference(&ZSet::new(), &left), &ZSet::new())
        .unwrap();
    assert!(circuit.rows().is_empty());
    circuit
        .tick(&left, &difference(&ZSet::new(), &right))
        .unwrap();
    assert!(circuit.rows().is_empty());
    circuit.tick(&ZSet::new(), &right).unwrap();
    assert_eq!(
        plain(circuit.rows()),
        Bag::from([
            (row(vec![int(2), int(7)]), 3),
            (row(vec![int(4), int(7)]), 5)
        ])
    );
}

#[test]
fn typed_join_keys_and_residual_on_operands_keep_the_original_argument_frame() {
    let outer = source(
        "outer",
        &[
            GraphSetColumnType::Scalar,
            GraphSetColumnType::Vertex,
            GraphSetColumnType::Scalar,
        ],
    );
    let args = arguments(vec![
        GraphSetColumnType::Vertex,
        GraphSetColumnType::Scalar,
        GraphSetColumnType::Scalar,
    ])
    .project(
        vec![
            GraphSetProjection::new("token", GraphSetValue::Column(2)),
            GraphSetProjection::new("owner", GraphSetValue::Column(0)),
            GraphSetProjection::new("wanted", GraphSetValue::Column(1)),
        ],
        GraphSetQuantifier::All,
    )
    .unwrap();
    let right = source(
        "right",
        &[GraphSetColumnType::Vertex, GraphSetColumnType::Scalar],
    );
    let join = RowJoinSpec::new(args.column_types(), right.column_types(), &[(1, 0)])
        .unwrap()
        .with_predicate(&[equal(2, 4)])
        .unwrap();
    let body = args
        .join(right, join)
        .unwrap()
        .project(
            vec![
                GraphSetProjection::new("child", GraphSetValue::Column(3)),
                GraphSetProjection::new("score", GraphSetValue::Column(4)),
            ],
            GraphSetQuantifier::Distinct,
        )
        .unwrap();
    let query = outer.optional_apply(body, vec![1, 2, 0]).unwrap();
    let first = row(vec![int(2), GraphValue::Vertex(VId(7)), int(10)]);
    let second = row(vec![int(3), GraphValue::Vertex(VId(7)), int(11)]);
    let third = row(vec![int(2), GraphValue::Vertex(VId(8)), int(10)]);
    let left = bag([(first.clone(), 3), (second.clone(), 2), (third.clone(), 5)]);
    let old_right = bag([
        (row(vec![GraphValue::Vertex(VId(7)), int(10)]), 7),
        (row(vec![GraphValue::Vertex(VId(8)), int(12)]), 3),
    ]);
    let next_right = bag([
        (row(vec![GraphValue::Vertex(VId(7)), int(11)]), 13),
        (row(vec![GraphValue::Vertex(VId(8)), int(12)]), 3),
    ]);
    let output = |outer: &GraphValueRow, child, value| {
        let mut values = outer.values().to_vec();
        values.extend([child, value]);
        row(values)
    };
    let before = Bag::from([
        (output(&first, GraphValue::Vertex(VId(7)), int(10)), 3),
        (output(&second, null(), null()), 2),
        (output(&third, null(), null()), 5),
    ]);
    let after = Bag::from([
        (output(&first, null(), null()), 3),
        (output(&second, GraphValue::Vertex(VId(7)), int(11)), 2),
        (output(&third, null(), null()), 5),
    ]);
    let mut circuit = Circuit::new(&query);
    circuit.tick(&left, &old_right).unwrap();
    assert_eq!(plain(circuit.rows()), before);
    let delta = circuit
        .tick(&ZSet::new(), &difference(&next_right, &old_right))
        .unwrap();
    assert_eq!(plain(circuit.rows()), after);
    assert_eq!(delta, difference(&bag(after), &bag(before)));
}

#[test]
fn every_preparation_checkpoint_preserves_definition_identity_and_recipe() {
    let query = query(true);
    let frozen = query.canonical_bytes();
    let mut count = 0;
    let expected = query
        .incremental_apply_distinct_with_control(&mut |_| {
            count += 1;
            Ok::<_, usize>(())
        })
        .unwrap()
        .unwrap();
    assert!(count > 1);
    for stop in 1..=count {
        let mut seen = 0;
        let result = query.incremental_apply_distinct_with_control(&mut |_| {
            seen += 1;
            if seen == stop { Err(stop) } else { Ok(()) }
        });
        assert!(matches!(result, Err(at) if at == stop));
        assert_eq!(seen, stop);
        assert_eq!(query.canonical_bytes(), frozen);
    }
    assert_eq!(
        query
            .incremental_apply_distinct_with_control(&mut allow)
            .unwrap()
            .unwrap(),
        expected
    );
}

#[test]
fn unsupported_argument_domains_and_real_windows_remain_typed_refusals() {
    for kind in [
        GraphSetColumnType::Any,
        GraphSetColumnType::List,
        GraphSetColumnType::Path,
    ] {
        let body = arguments(vec![kind])
            .cross_join(leaf())
            .unwrap()
            .project(
                vec![GraphSetProjection::new("child", GraphSetValue::Column(1))],
                GraphSetQuantifier::Distinct,
            )
            .unwrap();
        let query = source("outer", &[kind]).apply(body, vec![0]).unwrap();
        assert!(
            query
                .incremental_apply_distinct_with_control(&mut allow)
                .unwrap()
                .is_none()
        );
    }
    let query = query(false);
    assert!(
        query
            .with_page(0, Some(0))
            .incremental_apply_distinct_with_control(&mut allow)
            .unwrap()
            .is_none()
    );
}

#[test]
fn null_default_proof_keeps_the_admitted_domain_and_refuses_arbitrary_type_assertions() {
    let spec = RowProjectionSpec::new(
        vec![
            GraphSetColumnType::Scalar,
            GraphSetColumnType::Vertex,
            GraphSetColumnType::Stored,
        ],
        vec![
            GraphSetProjection::new("scalar", GraphSetValue::Column(0)),
            GraphSetProjection::new("vertex", GraphSetValue::Column(1)),
            GraphSetProjection::new("stored", GraphSetValue::Column(2)),
        ],
        GraphSetQuantifier::All,
    )
    .unwrap();
    let no = GraphValue::Scalar(CanonicalScalar::Bool(false));
    let normalized = spec
        .clone()
        .with_null_default(0, no.clone())
        .unwrap()
        .with_null_default(1, GraphValue::Vertex(VId(0)))
        .unwrap()
        .with_null_default(2, no.clone())
        .unwrap();
    assert_eq!(normalized.column_types(), spec.column_types());
    let stored = GraphValue::List(vec![int(9)].into_boxed_slice());
    let mut rows = IncrementalRowProjection::new(normalized);
    rows.prepare(
        &bag([
            (row(vec![null(), null(), null()]), 2),
            (
                row(vec![int(7), GraphValue::Vertex(VId(3)), stored.clone()]),
                3,
            ),
        ]),
        LIMBS,
        None,
        &mut |_| Ok::<_, ()>(()),
    )
    .unwrap()
    .commit();
    assert_eq!(
        plain(rows.rows()),
        Bag::from([
            (
                row(vec![no.clone(), GraphValue::Vertex(VId(0)), no.clone()]),
                2
            ),
            (row(vec![int(7), GraphValue::Vertex(VId(3)), stored]), 3),
        ])
    );
    for (column, fallback) in [
        (0, null()),
        (0, GraphValue::Vertex(VId(0))),
        (1, no.clone()),
        (3, no.clone()),
    ] {
        assert!(spec.clone().with_null_default(column, fallback).is_err());
    }
    let literal = RowProjectionSpec::new(
        vec![],
        vec![GraphSetProjection::new(
            "value",
            GraphSetValue::Value(int(7)),
        )],
        GraphSetQuantifier::All,
    )
    .unwrap();
    assert!(literal.with_null_default(0, no.clone()).is_err());
    let expanded = RowProjectionSpec::unwind(
        vec![GraphSetColumnType::Scalar],
        vec!["input".into()],
        "item".into(),
        GraphSetValue::List(vec![GraphSetValue::Value(int(1))]),
    )
    .unwrap();
    assert!(expanded.with_null_default(0, no).is_err());
}
