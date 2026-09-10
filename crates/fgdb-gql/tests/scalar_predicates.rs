//! End-to-end scalar predicates through the ordinary scoped GLA evaluator.

use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GlaDirection, GraphColumn, GraphMatchClause, GraphPatternBuilder,
    IntegerComparison, ScalarPredicate, VertexPredicate};
use fgdb_gql::{GqlQueryError, GqlQueryPolicy};
use fgdb_types::{CanonicalScalar, VId};
use std::cell::Cell;

fn wide() -> GqlQueryPolicy { GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX) }

#[test]
fn scalar_predicate_work_is_reserved_before_source_reads_and_memoized() {
    let payload = CanonicalScalar::ucs_basic_text(&"x".repeat(129)).unwrap();
    let key = PropertyKeyId(1);
    let mut builder = GraphPatternBuilder::new(); builder.vertex("n").unwrap();
    builder.filter("n", VertexPredicate::ScalarProperty { key,
        predicate: ScalarPredicate::new(payload.clone(), IntegerComparison::Equal).unwrap() }).unwrap();
    let query = builder.prepare("n", 0, None).unwrap();
    let calls = Cell::new(0); let reads = Cell::new(0); let first_read = Cell::new(0);
    let result = query.plan().execute_with_control([VId(1), VId(1)], [], |_, predicates| {
        first_read.set(calls.get()); reads.set(reads.get() + 1);
        Ok::<_, usize>(predicates.iter().all(|predicate| predicate.matches(&[], &[(key, payload.clone())])))
    }, |_| { calls.set(calls.get() + 1); Ok(()) }).unwrap();
    assert_eq!(result, vec![VId(1)]); assert_eq!(reads.get(), 1);
    assert!(first_read.get() >= 6, "literal payload was not included in the control seam");
    for stop in 1..=first_read.get() {
        calls.set(0); reads.set(0);
        let result = query.plan().execute_with_control([VId(1)], [], |_, _| {
            reads.set(reads.get() + 1); Ok(true)
        }, |_| {
            calls.set(calls.get() + 1);
            if calls.get() == stop { Err(stop) } else { Ok(()) }
        });
        assert_eq!(result, Err(stop)); assert_eq!(reads.get(), 0);
    }
}

#[test]
fn scalar_predicates_share_exact_limits_and_never_convert_source_errors_to_null() {
    let key = PropertyKeyId(1);
    let value = CanonicalScalar::Bool(true);
    let mut builder = GraphPatternBuilder::new(); builder.vertex("n").unwrap();
    builder.filter("n", VertexPredicate::ScalarProperty { key,
        predicate: ScalarPredicate::new(value.clone(), IntegerComparison::Equal).unwrap() }).unwrap();
    let pattern = builder.prepare_values(&[GraphColumn::vertex("n", "n")], 0, None).unwrap();
    let run = |policy| pattern.plan().execute_governed_with_properties(2, [VId(1), VId(2)], [],
        |vid, predicates| Ok::<_, &str>(vid == VId(1) && predicates.iter()
            .all(|predicate| predicate.matches(&[], &[(key, value.clone())]))),
        |_, _| Ok(None), policy, || Ok::<_, ()>(()));
    let full = run(wide()).unwrap();
    assert_eq!(full.value.len(), 1);
    let exact = GqlQueryPolicy::new(2, 1, full.evaluator.work_units, full.evaluator.scratch_entries);
    assert_eq!(run(exact).unwrap(), full);
    for policy in [GqlQueryPolicy::new(2, 1, full.evaluator.work_units - 1, u64::MAX),
        GqlQueryPolicy::new(2, 1, u64::MAX, full.evaluator.scratch_entries - 1)] {
        assert!(matches!(run(policy), Err(GqlQueryError::Evaluator(_))));
    }
    assert!(matches!(run(GqlQueryPolicy::new(2, 0, u64::MAX, u64::MAX)), Err(GqlQueryError::Rows(_))));
    let mut missing = GraphPatternBuilder::new(); missing.vertex("n").unwrap();
    missing.filter("n", VertexPredicate::PropertyNull { key, is_null: true }).unwrap();
    let plan = missing.prepare("n", 0, None).unwrap();
    let result = plan.plan().execute([VId(1)], [], |_, _| Err::<bool, _>("unreadable vertex"));
    assert_eq!(result, Err("unreadable vertex"));
}

#[test]
fn optional_scalar_filters_preserve_null_extension_and_outer_label_scope() {
    let key = PropertyKeyId(1);
    let accepted = CanonicalScalar::ucs_basic_text("accepted").unwrap();
    let rejected = CanonicalScalar::ucs_basic_text("rejected").unwrap();
    let mut outer = GraphPatternBuilder::new(); outer.vertex("a").unwrap();
    outer.filter("a", VertexPredicate::HasLabel(LabelId(1))).unwrap();
    let mut child = GraphPatternBuilder::new(); child.vertex("a").unwrap(); child.vertex("b").unwrap();
    child.edge("a", RelationId(1), GlaDirection::Forward, "b").unwrap();
    child.filter("b", VertexPredicate::ScalarProperty { key,
        predicate: ScalarPredicate::new(accepted.clone(), IntegerComparison::Equal).unwrap() }).unwrap();
    let query = outer.prepare_values_with_clauses(&[GraphMatchClause::optional(&child)],
        &[GraphColumn::vertex("a", "a"), GraphColumn::vertex("b", "b"), GraphColumn::property("status", "b", key)],
        0, None).unwrap().with_duplicates();
    assert_eq!(query.required_vertex_label(), Some(LabelId(1)));
    let edges = [(VId(1), RelationId(1), VId(10)), (VId(1), RelationId(1), VId(10)),
        (VId(2), RelationId(1), VId(20))];
    let result = query.plan().execute_governed_with_properties(6, [VId(1), VId(2), VId(3)], edges,
        |vid, predicates| {
            let labels = if vid.0 < 10 { vec![LabelId(1)] } else { vec![] };
            let props = if vid == VId(10) { vec![(key, accepted.clone())] }
                else if vid == VId(20) { vec![(key, rejected.clone())] } else { vec![] };
            Ok::<_, ()>(predicates.iter().all(|predicate| predicate.matches(&labels, &props)))
        }, |vid, _| {
            assert_eq!(vid, VId(10), "null or rejected optional bindings reached the property source");
            Ok(Some(&accepted))
        }, wide(), || Ok::<_, ()>(())).unwrap();
    assert_eq!(result.value.len(), 4);
    assert_eq!(result.value[0], result.value[1]);
    for row in &result.value[2..] {
        assert!(row.get(1).unwrap().is_null()); assert!(row.get(2).unwrap().is_null());
    }
}

#[test]
fn literal_type_value_operator_and_null_polarity_are_logical_identity() {
    let key = PropertyKeyId(1);
    let compile = |name: &str, predicate| {
        let mut builder = GraphPatternBuilder::new(); builder.vertex(name).unwrap();
        builder.filter(name, predicate).unwrap(); builder.prepare(name, 0, None).unwrap().canonical_bytes()
    };
    let make = |value, comparison| VertexPredicate::ScalarProperty {
        key, predicate: ScalarPredicate::new(value, comparison).unwrap(),
    };
    let first = compile("n", make(CanonicalScalar::Bool(true), IntegerComparison::Equal));
    assert_eq!(first, compile("renamed", make(CanonicalScalar::Bool(true), IntegerComparison::Equal)));
    for changed in [make(CanonicalScalar::Bool(false), IntegerComparison::Equal),
        make(CanonicalScalar::Int(1), IntegerComparison::Equal),
        make(CanonicalScalar::Bool(true), IntegerComparison::NotEqual),
        VertexPredicate::PropertyNull { key, is_null: true }, VertexPredicate::PropertyNull { key, is_null: false }] {
        assert_ne!(first, compile("n", changed));
    }
    assert_eq!(VertexPredicate::HasLabel(LabelId(1)).comparison_work_units(), 0);
    assert_eq!(VertexPredicate::IntegerProperty { key, comparison: IntegerComparison::Equal, value: 1 }.comparison_work_units(), 0);
}
