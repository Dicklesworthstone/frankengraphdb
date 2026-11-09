//! Presence laws for maintained correlated EXISTS.
//! The oracle below uses full-input scalar membership, independent of the recipe.

use super::*;
use crate::algebra::{GraphValueOrder, GraphValueRow, IntegerComparison};
use crate::row_join::IncrementalRowJoin;
use crate::row_projection::IncrementalRowProjection;
use crate::{
    GqlParameters, GqlScalarParameter, GraphAggregate, GraphIntegerBinary, GraphIntegerExpression,
    GraphIntegerOp, GraphSymbol, GraphSymbolKind, PreparedGraphSetText, PreparedGraphText,
};
use fgdb_delta_types::zset::set::{IncrementalSet, SetOperation};
use fgdb_delta_types::{LabelId, LimbLimit, PropertyKeyId, RelationId, ZSet, ZWeight};
use fgdb_types::{CanonicalF64, VId};
use std::collections::BTreeMap;

const LIMBS: LimbLimit = LimbLimit::new(4);
type Bag = BTreeMap<GraphValueRow, i128>;

fn allow(_: GlaExecutionEvent) -> Result<(), usize> {
    Ok(())
}

fn scalar(value: Option<i64>) -> GraphValue {
    GraphValue::Scalar(value.map_or(CanonicalScalar::Null, CanonicalScalar::Int))
}

fn truth() -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Bool(true))
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

fn arguments(types: Vec<GraphSetColumnType>) -> PreparedGraphSet {
    PreparedGraphSet::arguments(
        (0..types.len())
            .map(|column| format!("arg{column}"))
            .collect(),
        types,
    )
    .unwrap()
}

fn witness(body: PreparedGraphSet) -> PreparedGraphSet {
    body.project_ordered_output(vec![GraphSetProjection::new(
        "owner",
        GraphSetValue::Literal(GqlScalarParameter::new(CanonicalScalar::Bool(true)).unwrap()),
    )])
    .unwrap()
    .with_page(0, Some(1))
}

fn equal(left: usize, right: usize) -> GraphSetPredicateOp {
    GraphSetPredicateOp::Compare {
        left: GraphSetOperand::Column(left),
        comparison: IntegerComparison::Equal,
        right: GraphSetOperand::Column(right),
    }
}

fn right() -> PreparedGraphSet {
    leaf()
        .project(
            vec![
                GraphSetProjection::new("child", GraphSetValue::Column(0)),
                GraphSetProjection::new("score", GraphSetValue::Value(scalar(Some(10)))),
                GraphSetProjection::new("missing", GraphSetValue::Value(scalar(None))),
            ],
            GraphSetQuantifier::All,
        )
        .unwrap()
}

fn selected_body() -> PreparedGraphSet {
    arguments(vec![GraphSetColumnType::Stored; 3])
        .cross_join(right())
        .unwrap()
        .project(
            vec![
                GraphSetProjection::new("score", GraphSetValue::Column(4)),
                GraphSetProjection::new("wanted", GraphSetValue::Column(0)),
                GraphSetProjection::new("child", GraphSetValue::Column(3)),
                GraphSetProjection::new("repeated", GraphSetValue::Column(2)),
            ],
            GraphSetQuantifier::All,
        )
        .unwrap()
        .filter(&[equal(0, 1)])
        .unwrap()
}

fn query() -> PreparedGraphSet {
    outer()
        .optional_apply(witness(selected_body()), vec![2, 0, 2])
        .unwrap()
}

struct Circuit {
    present_join: IncrementalRowJoin,
    absent_join: IncrementalRowJoin,
    present: IncrementalRowProjection,
    absent: IncrementalRowProjection,
    union: IncrementalSet<GraphValueRow>,
    output: IncrementalRowProjection,
}

impl Circuit {
    fn new(query: &PreparedGraphSet) -> Self {
        let recipe = query
            .incremental_apply_exists_with_control(&mut allow)
            .unwrap()
            .unwrap();
        assert_eq!(recipe.join.kind(), RowJoinKind::Semi);
        Self {
            present_join: IncrementalRowJoin::new(recipe.join.clone()),
            absent_join: IncrementalRowJoin::new(recipe.join.with_kind(RowJoinKind::Anti)),
            present: IncrementalRowProjection::new(recipe.present),
            absent: IncrementalRowProjection::new(recipe.absent),
            union: IncrementalSet::new(SetOperation::UnionAll),
            output: IncrementalRowProjection::new(recipe.output),
        }
    }

    fn tick(
        &mut self,
        left: &ZSet<GraphValueRow>,
        right: &ZSet<GraphValueRow>,
    ) -> ZSet<GraphValueRow> {
        let present = self
            .present_join
            .prepare(left, right, LIMBS, None, &mut |_| Ok::<_, ()>(()))
            .unwrap()
            .commit();
        let absent = self
            .absent_join
            .prepare(left, right, LIMBS, None, &mut |_| Ok::<_, ()>(()))
            .unwrap()
            .commit();
        let present = self
            .present
            .prepare(&present, LIMBS, None, &mut |_| Ok::<_, ()>(()))
            .unwrap()
            .commit();
        let absent = self
            .absent
            .prepare(&absent, LIMBS, None, &mut |_| Ok::<_, ()>(()))
            .unwrap()
            .commit();
        let delta = self
            .union
            .prepare(&present, &absent, LIMBS, &mut |_| Ok::<_, ()>(()))
            .unwrap()
            .commit();
        self.output
            .prepare(&delta, LIMBS, None, &mut |_| Ok::<_, ()>(()))
            .unwrap()
            .commit()
    }

    fn rows(&self) -> &ZSet<GraphValueRow> {
        self.output.rows()
    }
}

fn left_rows(mask: usize) -> ZSet<GraphValueRow> {
    bag([
        (
            row(vec![
                scalar(Some(3)),
                GraphValue::Vertex(VId(u128::MAX)),
                scalar(Some(10)),
            ]),
            2,
        ),
        (
            row(vec![
                scalar(Some(4)),
                GraphValue::Vertex(VId(17)),
                scalar(Some(0)),
            ]),
            3,
        ),
    ]
    .into_iter()
    .enumerate()
    .filter_map(|(index, item)| (mask & (1 << index) != 0).then_some(item)))
}

fn right_rows(mask: usize) -> ZSet<GraphValueRow> {
    bag([
        (
            row(vec![
                GraphValue::Vertex(VId(1)),
                scalar(Some(10)),
                scalar(None),
            ]),
            2,
        ),
        (
            row(vec![
                GraphValue::Vertex(VId(u128::MAX)),
                scalar(Some(10)),
                scalar(None),
            ]),
            3,
        ),
    ]
    .into_iter()
    .enumerate()
    .filter_map(|(index, item)| (mask & (1 << index) != 0).then_some(item)))
}

fn oracle(left: &ZSet<GraphValueRow>, right: &ZSet<GraphValueRow>) -> Bag {
    let mut output = Bag::new();
    for (a, count) in left.iter() {
        let GraphValue::Scalar(CanonicalScalar::Int(wanted)) = a.values()[2] else {
            panic!("integer oracle fixture");
        };
        let matched = right.iter().any(|(b, weight)| {
            let GraphValue::Scalar(CanonicalScalar::Int(score)) = b.values()[1] else {
                panic!("integer oracle fixture");
            };
            weight > &ZWeight::ZERO && wanted == score
        });
        let mut cells = a.values().to_vec();
        cells.push(if matched { truth() } else { scalar(None) });
        output.insert(row(cells), count.to_i128().unwrap());
    }
    output
}

#[test]
fn presence_deltas_preserve_outer_bags_across_all_256_simultaneous_transitions() {
    let query = query();
    for before in 0..16 {
        for after in 0..16 {
            let mut circuit = Circuit::new(&query);
            let old_left = left_rows(before & 3);
            let old_right = right_rows(before >> 2);
            circuit.tick(&old_left, &old_right);
            assert_eq!(plain(circuit.rows()), oracle(&old_left, &old_right));
            let next_left = left_rows(after & 3);
            let next_right = right_rows(after >> 2);
            let delta = circuit.tick(
                &difference(&next_left, &old_left),
                &difference(&next_right, &old_right),
            );
            assert_eq!(plain(circuit.rows()), oracle(&next_left, &next_right));
            let expected = bag(oracle(&next_left, &next_right).into_iter().chain(
                oracle(&old_left, &old_right)
                    .into_iter()
                    .map(|(row, count)| (row, -count)),
            ));
            assert_eq!(delta, expected);
        }
    }
}

#[test]
fn real_null_rhs_is_a_witness_but_null_predicate_import_is_unknown() {
    let source = leaf()
        .project(
            vec![GraphSetProjection::new(
                "missing",
                GraphSetValue::Value(scalar(None)),
            )],
            GraphSetQuantifier::All,
        )
        .unwrap();
    let body = arguments(vec![GraphSetColumnType::Stored])
        .cross_join(source)
        .unwrap();
    let query = outer().optional_apply(witness(body), vec![2]).unwrap();
    let left = row(vec![
        scalar(Some(8)),
        GraphValue::Vertex(VId(19)),
        scalar(None),
    ]);
    let mut circuit = Circuit::new(&query);
    circuit.tick(
        &bag([(left.clone(), 4)]),
        &bag([(row(vec![scalar(None)]), 3)]),
    );
    let mut expected = left.values().to_vec();
    expected.push(truth());
    assert_eq!(plain(circuit.rows()), Bag::from([(row(expected), 4)]));
    circuit.tick(&ZSet::new(), &bag([(row(vec![scalar(None)]), -3)]));
    let mut expected = left.values().to_vec();
    expected.push(scalar(None));
    assert_eq!(
        plain(circuit.rows()),
        Bag::from([(row(expected.clone()), 4)])
    );
    let mut selected = Circuit::new(&self::query());
    selected.tick(&bag([(left, 4)]), &right_rows(3));
    assert_eq!(plain(selected.rows()), Bag::from([(row(expected), 4)]));
}

#[test]
fn typed_keys_and_numeric_where_keep_their_distinct_equality_laws() {
    let args = arguments(vec![GraphSetColumnType::Vertex; 2]);
    let source = leaf();
    let join = RowJoinSpec::new(
        args.column_types(),
        source.column_types(),
        &[(0, 0), (1, 0)],
    )
    .unwrap();
    let query = outer()
        .optional_apply(witness(args.join(source, join).unwrap()), vec![1, 1])
        .unwrap();
    let recipe = query
        .incremental_apply_exists_with_control(&mut allow)
        .unwrap()
        .unwrap();
    assert_eq!(recipe.join.keys(), &[(1, 0), (1, 0)]);
    let mut circuit = Circuit::new(&query);
    circuit.tick(
        &left_rows(1),
        &bag([(row(vec![GraphValue::Vertex(VId(u128::MAX))]), 7)]),
    );
    let mut expected = left_rows(1).iter().next().unwrap().0.values().to_vec();
    expected.push(truth());
    assert_eq!(
        plain(circuit.rows()),
        Bag::from([(row(expected.clone()), 2)])
    );

    let floating = GraphValue::Scalar(CanonicalScalar::Float(CanonicalF64::new(10.0)));
    let source = leaf()
        .project(
            vec![GraphSetProjection::new(
                "score",
                GraphSetValue::Value(floating.clone()),
            )],
            GraphSetQuantifier::All,
        )
        .unwrap();
    let body = arguments(vec![GraphSetColumnType::Stored])
        .cross_join(source)
        .unwrap()
        .filter(&[equal(0, 1)])
        .unwrap();
    let query = outer().optional_apply(witness(body), vec![2]).unwrap();
    assert!(
        query
            .incremental_apply_exists_with_control(&mut allow)
            .unwrap()
            .unwrap()
            .join
            .keys()
            .is_empty()
    );
    let mut circuit = Circuit::new(&query);
    circuit.tick(&left_rows(1), &bag([(row(vec![floating]), 5)]));
    assert_eq!(plain(circuit.rows()), Bag::from([(row(expected), 2)]));
}

#[test]
fn only_literal_one_witness_pages_are_erased_and_discarded_effects_still_refuse() {
    let base = || {
        arguments(vec![GraphSetColumnType::Stored])
            .cross_join(leaf())
            .unwrap()
    };
    let literal = base()
        .project(
            vec![GraphSetProjection::new(
                "ignored",
                GraphSetValue::Value(scalar(Some(9))),
            )],
            GraphSetQuantifier::All,
        )
        .unwrap();
    let admitted = outer().optional_apply(witness(literal), vec![2]).unwrap();
    assert!(
        admitted
            .incremental_apply_exists_with_control(&mut allow)
            .unwrap()
            .is_some()
    );
    let division = GraphSetValue::Integer(
        GraphIntegerExpression::prepare(&[
            GraphIntegerOp::Literal(Some(12)),
            GraphIntegerOp::Column(0),
            GraphIntegerOp::Binary(GraphIntegerBinary::Divide),
        ])
        .unwrap(),
    );
    let fallible = base()
        .project(
            vec![GraphSetProjection::new("ignored", division)],
            GraphSetQuantifier::All,
        )
        .unwrap()
        .filter(&[GraphSetPredicateOp::Truth(Some(false))])
        .unwrap();
    let property = PreparedGraphText::prepare("MATCH (n) RETURN n.p AS payload", |kind, _| {
        (kind == GraphSymbolKind::Property).then_some(GraphSymbol::Property(PropertyKeyId(1)))
    })
    .unwrap()
    .bind_parameters(&GqlParameters::new())
    .unwrap();
    let false_witness = base()
        .project_ordered_output(vec![GraphSetProjection::new(
            "owner",
            GraphSetValue::Value(GraphValue::Scalar(CanonicalScalar::Bool(false))),
        )])
        .unwrap()
        .with_page(0, Some(1));
    for body in [
        witness(base()).with_page(0, Some(0)),
        witness(base()).with_page(0, Some(2)),
        witness(base()).with_page(1, Some(1)),
        witness(base())
            .with_order_by(&[GraphValueOrder::ascending(0)])
            .unwrap(),
        false_witness,
        witness(base().with_page(0, Some(1))),
        witness(
            base()
                .project(
                    vec![GraphSetProjection::new("id", GraphSetValue::Column(1))],
                    GraphSetQuantifier::Distinct,
                )
                .unwrap(),
        ),
        witness(
            base()
                .group_by(&[], &[GraphAggregate::count_rows("count")])
                .unwrap(),
        ),
        witness(fallible),
        witness(
            arguments(vec![GraphSetColumnType::Stored])
                .cross_join(property.into())
                .unwrap(),
        ),
    ] {
        let query = outer().optional_apply(body, vec![2]).unwrap();
        assert!(
            query
                .incremental_apply_exists_with_control(&mut allow)
                .unwrap()
                .is_none()
        );
    }
    let outer_page = self::query().with_page(0, Some(0));
    assert!(!outer_page.incremental_window_sequence_compatible());
    assert!(
        outer_page
            .incremental_apply_exists_with_control(&mut allow)
            .unwrap()
            .is_none()
    );
}

fn admitted_apply(query: &PreparedGraphSet) -> bool {
    if query
        .incremental_apply_exists_with_control(&mut allow)
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
fn native_exists_and_not_exists_reach_presence_admission_without_general_page_admission() {
    for text in [
        "MATCH (a:L) WITH a WHERE EXISTS { MATCH (a)-[:R]->(b) } RETURN a",
        "MATCH (a:L) WITH a WHERE NOT EXISTS { MATCH (a)-[:R]->(b) } RETURN a",
        "MATCH (a:L) WITH a RETURN EXISTS { MATCH (a)-[:R]->(b) } AS present",
    ] {
        let query = PreparedGraphSetText::prepare(text, |kind, name| match (kind, name) {
            (GraphSymbolKind::Label, "L") => Some(GraphSymbol::Label(LabelId(1))),
            (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(RelationId(1))),
            _ => None,
        })
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
        assert!(query.incremental_window_sequence_compatible(), "{text}");
        assert!(admitted_apply(&query), "{text}");
        let paged = query.with_page(0, Some(0));
        assert!(!paged.incremental_window_sequence_compatible(), "{text}");
        assert!(paged.incremental_window().unwrap().is_none(), "{text}");
    }
}

#[test]
fn every_definition_checkpoint_keeps_original_schema_names_and_transcript() {
    let query = query();
    let original = query.canonical_bytes();
    let mut calls = 0;
    let recipe = query
        .incremental_apply_exists_with_control(&mut |_| {
            calls += 1;
            Ok::<_, usize>(())
        })
        .unwrap()
        .unwrap();
    assert_eq!(recipe.left, &outer());
    assert_eq!(recipe.right, &right());
    assert_eq!(recipe.output.column_types(), query.column_types());
    assert_eq!(
        recipe.output.columns().collect::<Vec<_>>(),
        query
            .columns()
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
    );
    for stop in 1..=calls {
        let mut seen = 0;
        let result = query.incremental_apply_exists_with_control(&mut |_| {
            seen += 1;
            if seen == stop { Err(stop) } else { Ok(()) }
        });
        assert!(matches!(result, Err(at) if at == stop));
        assert_eq!(seen, stop);
        assert_eq!(query.canonical_bytes(), original);
    }
}
