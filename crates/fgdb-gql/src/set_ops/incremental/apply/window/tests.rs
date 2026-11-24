use super::*;
use crate::algebra::{GraphValueOrder, GraphValueRow};
use crate::row_join::IncrementalRowJoin;
use crate::row_projection::IncrementalRowProjection;
use crate::row_window::IncrementalRowWindow;
use crate::{GqlParameters, GraphSymbol, GraphSymbolKind, PreparedGraphText};
use fgdb_delta_types::{LimbLimit, RelationId, ZSet, ZWeight};
use fgdb_types::VId;
use std::collections::BTreeMap;

const LIMBS: LimbLimit = LimbLimit::new(4);
type Bag = BTreeMap<GraphValueRow, i128>;

fn allow(_: GlaExecutionEvent) -> Result<(), usize> { Ok(()) }
fn int(value: i64) -> GraphValue { GraphValue::Scalar(CanonicalScalar::Int(value)) }
fn null() -> GraphValue { GraphValue::Scalar(CanonicalScalar::Null) }
fn row(values: Vec<GraphValue>) -> GraphValueRow { GraphValueRow::from_owned_values(values) }
fn bag(rows: impl IntoIterator<Item = (GraphValueRow, i128)>) -> ZSet<GraphValueRow> {
    ZSet::from_updates(rows.into_iter().map(|(row, n)| (row, ZWeight::from_i128(n))), LIMBS, &mut |_| Ok::<_, ()>(())).unwrap()
}
fn plain(rows: &ZSet<GraphValueRow>) -> Bag {
    rows.iter().map(|(row, n)| (row.clone(), n.to_i128().unwrap())).collect()
}
fn difference(after: &ZSet<GraphValueRow>, before: &ZSet<GraphValueRow>) -> ZSet<GraphValueRow> {
    bag(plain(after).into_iter().chain(plain(before).into_iter().map(|(row, n)| (row, -n))))
}
fn leaf(text: &str) -> PreparedGraphSet {
    PreparedGraphText::prepare(text, |kind, name| match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(RelationId(1))),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(PropertyKeyId(2))),
        _ => None,
    }).unwrap().bind_parameters(&GqlParameters::new()).unwrap().with_duplicates().into()
}
// Fixture bags represent complete native sources. Their schema witnesses use
// only total scalar constants and identities, without conditional reader work.
fn source(prefix: &str, types: &[GraphSetColumnType]) -> PreparedGraphSet {
    leaf("MATCH (n) RETURN n AS id").project(types.iter().enumerate().map(|(at, kind)| {
        GraphSetProjection::new(format!("{prefix}{at}"), match kind {
            GraphSetColumnType::Vertex => GraphSetValue::Column(0),
            _ => GraphSetValue::Value(int(0)),
        })
    }).collect(), GraphSetQuantifier::All).unwrap()
}
fn arguments() -> PreparedGraphSet {
    PreparedGraphSet::arguments(vec!["wanted".into()], vec![GraphSetColumnType::Scalar]).unwrap()
}
fn definition(optional: bool, distinct: bool, offset: u64, count: u64) -> PreparedGraphSet {
    let outer = source("outer", &[GraphSetColumnType::Scalar, GraphSetColumnType::Vertex]);
    let body = arguments().cross_join(source("right", &[GraphSetColumnType::Scalar, GraphSetColumnType::Vertex, GraphSetColumnType::Scalar])).unwrap()
        .filter(&[GraphSetPredicateOp::Compare { left: GraphSetOperand::Column(0), comparison: IntegerComparison::Equal, right: GraphSetOperand::Column(1) }]).unwrap()
        .project(vec![GraphSetProjection::new("candidate", GraphSetValue::Column(2)), GraphSetProjection::new("rank", GraphSetValue::Column(3))],
            if distinct { GraphSetQuantifier::Distinct } else { GraphSetQuantifier::All }).unwrap()
        .with_order_by(&[GraphValueOrder::descending(1), GraphValueOrder::ascending(0)]).unwrap()
        .with_page(offset, Some(count));
    if optional { outer.optional_apply(body, vec![0]).unwrap() } else { outer.apply(body, vec![0]).unwrap() }
}

struct Circuit {
    outer: IncrementalRowProjection,
    arguments: IncrementalRowProjection,
    join: IncrementalRowJoin,
    body: Vec<IncrementalRowProjection>,
    window: IncrementalRowWindow,
    restore: IncrementalRowJoin,
    output: Vec<IncrementalRowProjection>,
}
impl Circuit {
    fn new(query: &PreparedGraphSet) -> Self {
        let recipe = query.incremental_apply_window_with_control(&mut allow).unwrap().expect("finite invocation page must admit");
        let plan = recipe.invocation;
        Self {
            outer: IncrementalRowProjection::new(plan.outer),
            arguments: IncrementalRowProjection::new(plan.arguments),
            join: IncrementalRowJoin::new(plan.join),
            body: plan.body.into_iter().map(IncrementalRowProjection::new).collect(),
            window: IncrementalRowWindow::new(recipe.window),
            restore: IncrementalRowJoin::new(plan.restore),
            output: plan.output.into_iter().map(IncrementalRowProjection::new).collect(),
        }
    }
    fn tick(&mut self, left: &ZSet<GraphValueRow>, right: &ZSet<GraphValueRow>) -> ZSet<GraphValueRow> {
        let outer = self.outer.prepare(left, LIMBS, None, &mut |_| Ok::<_, ()>(())).unwrap().commit();
        let arguments = self.arguments.prepare(&outer, LIMBS, None, &mut |_| Ok::<_, ()>(())).unwrap().commit();
        let mut delta = self.join.prepare(&arguments, right, LIMBS, None, &mut |_| Ok::<_, ()>(())).unwrap().commit();
        for projection in &mut self.body {
            delta = projection.prepare(&delta, LIMBS, None, &mut |_| Ok::<_, ()>(())).unwrap().commit();
        }
        delta = self.window.apply(&delta, LIMBS, None, &mut |_| Ok::<_, ()>(())).unwrap();
        delta = self.restore.prepare(&outer, &delta, LIMBS, None, &mut |_| Ok::<_, ()>(())).unwrap().commit();
        for projection in &mut self.output {
            delta = projection.prepare(&delta, LIMBS, None, &mut |_| Ok::<_, ()>(())).unwrap().commit();
        }
        delta
    }
    fn rows(&self) -> &ZSet<GraphValueRow> { self.output.last().unwrap().rows() }
}

fn inputs(state: usize) -> (ZSet<GraphValueRow>, ZSet<GraphValueRow>) {
    let left = [(row(vec![int(0), GraphValue::Vertex(VId(101))]), 2),
        (row(vec![int(0), GraphValue::Vertex(VId(102))]), 3),
        (row(vec![int(1), GraphValue::Vertex(VId(103))]), 2)];
    let right = [(row(vec![int(0), GraphValue::Vertex(VId(10)), int(9)]), 2),
        (row(vec![int(0), GraphValue::Vertex(VId(11)), int(9)]), 1),
        (row(vec![int(1), GraphValue::Vertex(VId(20)), null()]), 1)];
    (bag(left.into_iter().enumerate().filter_map(|(at, row)| (state & (1 << at) != 0).then_some(row))),
        bag(right.into_iter().enumerate().filter_map(|(at, row)| ((state / 2) & (1 << at) != 0).then_some(row))))
}

// An occurrence-enumerating oracle intentionally knows nothing about private
// key columns, derivative joins, ZWeight pagination or grouped operator state.
fn oracle(left: &ZSet<GraphValueRow>, right: &ZSet<GraphValueRow>, optional: bool, distinct: bool, offset: usize, count: usize) -> Bag {
    let mut output = Bag::new();
    for (outer, n) in left.iter() {
        let mut candidates = Vec::new();
        for (inner, multiplicity) in right.iter().filter(|(inner, _)| inner.values()[0] == outer.values()[0]) {
            let occurrences = if distinct { 1 } else { usize::try_from(multiplicity.to_i128().unwrap()).unwrap() };
            candidates.extend(std::iter::repeat_n(row(inner.values()[1..].to_vec()), occurrences));
        }
        candidates.sort_by(|a, b| {
            let a_null = a.values()[1] == null();
            let b_null = b.values()[1] == null();
            a_null.cmp(&b_null).then_with(|| b.values()[1].cmp(&a.values()[1])).then_with(|| a.values()[0].cmp(&b.values()[0]))
        });
        let selected: Vec<_> = candidates.into_iter().skip(offset).take(count).collect();
        if optional && selected.is_empty() {
            let mut values = outer.values().to_vec(); values.extend([null(), null()]);
            *output.entry(row(values)).or_default() += n.to_i128().unwrap();
        }
        for selected in selected {
            let mut values = outer.values().to_vec(); values.extend_from_slice(selected.values());
            *output.entry(row(values)).or_default() += n.to_i128().unwrap();
        }
    }
    output
}

#[test]
fn weighted_invocation_pages_match_independent_bags_after_each_transition() {
    for optional in [false, true] {
        for distinct in [false, true] {
            for (offset, count) in [(0, 1), (1, 1), (1, 2), (0, 0)] {
                let query = definition(optional, distinct, offset, count);
                for before in 0..16 {
                    for after in 0..16 {
                        let (left, right) = inputs(before);
                        let (next_left, next_right) = inputs(after);
                        let mut circuit = Circuit::new(&query);
                        circuit.tick(&left, &right);
                        assert_eq!(plain(circuit.rows()), oracle(&left, &right, optional, distinct, offset as usize, count as usize));
                        let previous = circuit.rows().checked_clone(LIMBS, &mut |_| Ok::<_, ()>(())).unwrap();
                        let delta = circuit.tick(&difference(&next_left, &left), &difference(&next_right, &right));
                        let expected = oracle(&next_left, &next_right, optional, distinct, offset as usize, count as usize);
                        assert_eq!(plain(circuit.rows()), expected);
                        assert_eq!(plain(&delta), plain(&difference(circuit.rows(), &previous)));
                    }
                }
            }
        }
    }
}

#[test]
fn null_imports_and_no_import_calls_activate_one_independent_page() {
    let outer = source("outer", &[GraphSetColumnType::Scalar]);
    let body = arguments().cross_join(leaf("MATCH (n) RETURN n AS id")).unwrap()
        .project(vec![GraphSetProjection::new("copied", GraphSetValue::Column(0)), GraphSetProjection::new("id", GraphSetValue::Column(1))], GraphSetQuantifier::All).unwrap()
        .with_order_by(&[GraphValueOrder::ascending(1)]).unwrap().with_page(1, Some(1));
    let query = outer.clone().optional_apply(body, vec![0]).unwrap();
    let left = bag([(row(vec![null()]), 3), (row(vec![int(7)]), 2)]);
    let right = bag([(row(vec![GraphValue::Vertex(VId(10))]), 2), (row(vec![GraphValue::Vertex(VId(11))]), 1)]);
    let mut circuit = Circuit::new(&query);
    circuit.tick(&left, &right);
    assert_eq!(plain(circuit.rows()), BTreeMap::from([
        (row(vec![null(), null(), GraphValue::Vertex(VId(10))]), 3),
        (row(vec![int(7), int(7), GraphValue::Vertex(VId(10))]), 2),
    ]));
    circuit.tick(&ZSet::new(), &bag([(row(vec![GraphValue::Vertex(VId(10))]), -2)]));
    assert_eq!(plain(circuit.rows()), BTreeMap::from([
        (row(vec![null(), null(), null()]), 3), (row(vec![int(7), null(), null()]), 2),
    ]));
    let no_import = outer.optional_apply(leaf("MATCH (n) RETURN n AS id").project(vec![GraphSetProjection::new("id", GraphSetValue::Column(0))], GraphSetQuantifier::All).unwrap().with_order_by(&[GraphValueOrder::ascending(0)]).unwrap().with_page(1, Some(1)), vec![]).unwrap();
    let mut circuit = Circuit::new(&no_import);
    circuit.tick(&ZSet::new(), &right);
    assert!(circuit.rows().is_empty());
    circuit.tick(&left, &ZSet::new());
    assert_eq!(plain(circuit.rows()), BTreeMap::from([
        (row(vec![null(), GraphValue::Vertex(VId(10))]), 3), (row(vec![int(7), GraphValue::Vertex(VId(10))]), 2),
    ]));
    circuit.tick(&difference(&ZSet::new(), &left), &ZSet::new());
    assert!(circuit.rows().is_empty());
}

#[test]
fn scalar_property_guard_requires_a_preceding_same_slot_and_key_comparison() {
    for comparison in ["=", "<", "<=", ">", ">="] {
        let source = leaf(&format!("MATCH (a)-[:R]->(b) WHERE b.p {comparison} 0 RETURN a,b,b.p AS score"));
        assert!(independent_window(&source, &mut allow).unwrap(), "{comparison}");
        assert!(!independent(&source, &mut allow).unwrap(), "old recipe must keep its source contract");
    }
    for predicate in ["", " WHERE b.p <> 0", " WHERE a.p >= 0", " WHERE b.q >= 0", " WHERE b.p >= 0 OR b.q >= 0", " WHERE NOT (b.p >= 0)"] {
        let source = leaf(&format!("MATCH (a)-[:R]->(b){predicate} RETURN a,b,b.p AS score"));
        assert!(!independent_window(&source, &mut allow).unwrap(), "{predicate}");
    }
}

#[test]
fn page_proof_is_cancellable_and_preserves_other_sequence_barriers() {
    let query = definition(true, true, 1, 1);
    let frozen = query.canonical_bytes();
    let mut events = 0;
    let expected = query.incremental_apply_window_with_control(&mut |_| { events += 1; Ok::<_, usize>(()) }).unwrap().unwrap();
    for stop in 1..=events {
        let mut at = 0;
        assert!(matches!(query.incremental_apply_window_with_control(&mut |_| { at += 1; if at == stop { Err(stop) } else { Ok(()) } }), Err(error) if error == stop));
        assert_eq!(query.canonical_bytes(), frozen);
    }
    assert_eq!(query.incremental_apply_window_with_control(&mut allow).unwrap().unwrap(), expected);
    assert!(query.incremental_window_sequence_compatible());
    assert!(!query.clone().with_page(0, Some(1)).incremental_window_sequence_compatible());
    let SetNode::Apply { left, right, imports, .. } = &query.node else { unreachable!() };
    let unbounded = right.as_ref().clone().with_page(0, None);
    assert!(left.as_ref().clone().apply(unbounded, imports.clone()).unwrap().incremental_apply_window_with_control(&mut allow).unwrap().is_none());
    let nested = right.as_ref().clone().project(vec![GraphSetProjection::new("candidate", GraphSetValue::Column(0)), GraphSetProjection::new("rank", GraphSetValue::Column(1))], GraphSetQuantifier::All).unwrap().with_order_by(&[GraphValueOrder::descending(1)]).unwrap().with_page(0, Some(1));
    assert!(left.as_ref().clone().apply(nested, imports.clone()).unwrap().incremental_apply_window_with_control(&mut allow).unwrap().is_none());
}
