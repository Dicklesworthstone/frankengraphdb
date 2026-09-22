use super::*;
use crate::algebra::{BindingSlot, GraphColumn, GraphPatternBuilder, VertexPredicate};
use crate::{GqlQueryError, GqlQueryPolicy};
use core::convert::Infallible;
use fgdb_delta_types::{LabelId, PropertyKeyId};
use fgdb_types::EId;
use std::cell::Cell;

fn allow(_: GlaExecutionEvent) -> Result<(), Infallible> { Ok(()) }
fn wide(records: usize) -> GqlQueryPolicy {
    GqlQueryPolicy::new(records as u64, u64::MAX, u64::MAX, u64::MAX)
}
fn triangle(directions: [GlaDirection; 3]) -> GraphPatternBuilder {
    let mut builder = GraphPatternBuilder::default();
    for name in ["a", "b", "c"] { builder.vertex(name).unwrap(); }
    builder.edge("a", RelationId(1), directions[0], "b").unwrap();
    builder.edge("b", RelationId(2), directions[1], "c").unwrap();
    builder.edge("c", RelationId(3), directions[2], "a").unwrap();
    builder
}
fn forward() -> GraphPatternBuilder { triangle([GlaDirection::Forward; 3]) }
fn edges() -> Vec<(VId, RelationId, VId)> {
    vec![
        (VId(0), RelationId(1), VId(1)),
        (VId(0), RelationId(1), VId(1)),
        (VId(1), RelationId(2), VId(u128::MAX)),
        (VId(u128::MAX), RelationId(3), VId(0)),
        (VId(u128::MAX), RelationId(3), VId(0)),
        (VId(0), RelationId(1), VId(0)),
        (VId(0), RelationId(2), VId(0)),
        (VId(0), RelationId(3), VId(0)),
    ]
}

#[test]
fn governed_generic_join_matches_the_existing_evaluator_for_oriented_multigraphs() {
    let directions = [GlaDirection::Forward, GlaDirection::Reverse, GlaDirection::Undirected];
    let mut seed = 7_u64;
    for _ in 0..12 {
        let mut input = Vec::new();
        for relation in 1..=3 {
            for source in [VId(0), VId(1), VId(u128::MAX)] {
                for target in [VId(0), VId(1), VId(u128::MAX)] {
                    seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                    for _ in 0..(seed >> 32) % 3 { input.push((source, RelationId(relation), target)); }
                }
            }
        }
        for a in directions { for b in directions { for c in directions {
            let builder = triangle([a, b, c]);
            for duplicate in [false, true] {
                for (offset, count) in [(0, None), (1, Some(3)), (99, Some(0))] {
                    let mut prepared = builder.prepare_bindings(&["b", "a", "c"], offset, count).unwrap();
                    if duplicate { prepared = prepared.with_duplicates(); }
                    let plan = prepared.plan();
                    assert!(compile(plan.operators(), &mut allow).unwrap().is_some());
                    let expected = plan.execute([], input.iter().copied(), |_, _| Ok::<_, Infallible>(true)).unwrap();
                    let result = plan.execute_governed(
                        input.len() as u64, [], input.iter().copied(),
                        |_, _| -> Result<bool, Infallible> { panic!("topology has no predicate reads") },
                        wide(input.len()), || Ok::<_, Infallible>(()),
                    ).unwrap();
                    assert_eq!(result.value, expected);
                    assert_eq!(result.rows.result_rows, expected.len() as u64);
                }
            }
        } } }
    }
}

#[test]
fn equality_classes_diagonals_and_inequalities_preserve_real_occurrences() {
    for equal in [false, true] {
        let mut builder = forward();
        builder.identity("a", "b", equal).unwrap();
        let prepared = builder.prepare_bindings(&["a", "b", "c"], 0, None).unwrap().with_duplicates();
        let input = edges();
        let plan = prepared.plan();
        let expected = plan.execute([], input.clone(), |_, _| Ok::<_, Infallible>(true)).unwrap();
        let result = plan.execute_governed(input.len() as u64, [], input, |_, _| Ok::<_, Infallible>(true), wide(8), || Ok::<_, Infallible>(())).unwrap();
        assert_eq!(result.value, expected);
        assert_eq!(result.value.len(), if equal { 1 } else { 4 });
    }
}

#[test]
fn all_public_governed_value_adapters_select_the_same_topology_plan() {
    let prepared = forward().prepare_values(&[
        GraphColumn::Vertex { name: "last", variable: "c" },
        GraphColumn::Vertex { name: "first", variable: "a" },
    ], 1, Some(3)).unwrap().with_duplicates();
    let plan = prepared.plan();
    let input = edges();
    let expected = plan.execute_with_properties_control([], input.clone(), |_, _| Ok::<_, &str>(true), |_, _| Err("property read"), |_| Ok(())).unwrap();
    let plain = plan.execute_governed_with_properties(
        8, [], input.clone(), |_, _| Err("predicate read"), |_, _| Err("property read"), wide(8), || Ok::<_, Infallible>(()),
    ).unwrap();
    assert_eq!(plain.value, expected);
    let identified = || input.iter().copied().enumerate().map(|(at, (s, r, d))| (EId(at as u128), s, r, d));
    let ids = plan.execute_governed_with_identified_properties(
        8, [], identified(), |_, _| Err("predicate read"), |_, _| Err("property read"), wide(8), || Ok::<_, Infallible>(()),
    ).unwrap();
    let elements = plan.execute_governed_with_element_accessors(
        8, [], identified(), |_, _| Err("predicate read"), |_, _| Err("property read"),
        |_, _| Err("edge read"), |_| Err("label read"), |_| Err("type read"),
        wide(8), || Ok::<_, Infallible>(()),
    ).unwrap();
    assert_eq!(ids.value, expected);
    assert_eq!(elements.value, expected);
    assert_eq!(plain.evaluator, ids.evaluator);
    assert_eq!(plain.evaluator, elements.evaluator);
}

#[test]
fn scopes_reads_paths_and_nonterminal_shapes_never_enter_the_join_island() {
    let prepared = forward().prepare("a", 0, None).unwrap();
    let original = prepared.plan().operators();
    for barrier in [
        GlaOperator::Select { slot: BindingSlot(0), predicates: vec![] },
        GlaOperator::Optional { group: 0, end: 4, slots: 2 },
        GlaOperator::Probe { group: 0, end: 4, anti: false },
        GlaOperator::BindVertex { source: BindingSlot(0) },
        GlaOperator::CapturePath { capture: 0, start: BindingSlot(0), segments: vec![BindingSlot(1)] },
        GlaOperator::ScanVertices,
    ] {
        let mut operators = original.to_vec();
        operators.insert(1, barrier);
        assert!(compile(&operators, &mut |_| -> Result<(), Infallible> { panic!("unselected lane must retain its event trace") }).unwrap().is_none());
    }
    let mut properties = forward();
    properties.filter("b", VertexPredicate::HasLabel(LabelId(9))).unwrap();
    let prepared = properties.prepare("a", 0, None).unwrap();
    assert!(compile(prepared.plan().operators(), &mut allow).unwrap().is_none());
    let result = prepared.plan().execute_governed(8, [], edges(), |_, _| Err("source predicate failed"), wide(8), || Ok::<_, Infallible>(()));
    assert!(matches!(result, Err(GqlQueryError::Source("source predicate failed"))));
    let prepared = forward().prepare_values(&[GraphColumn::Property { name: "value", variable: "a", key: PropertyKeyId(1) }], 0, None).unwrap();
    assert!(compile(prepared.plan().operators(), &mut allow).unwrap().is_none());
    let result = prepared.plan().execute_governed_with_properties(8, [], edges(), |_, _| Ok(true), |_, _| Err("source property failed"), wide(8), || Ok::<_, Infallible>(()));
    assert!(matches!(result, Err(GqlQueryError::Source("source property failed"))));
}

#[test]
fn admission_is_before_compilation_and_input_is_consumed_exactly_once() {
    let prepared = forward().prepare("a", 0, None).unwrap();
    let consumed = Cell::new(0);
    let input = || edges().into_iter().inspect(|_| consumed.set(consumed.get() + 1));
    let rejected = prepared.plan().execute_governed(8, [], input(), |_, _| Ok::<_, Infallible>(true), wide(7), || Ok::<_, Infallible>(()));
    assert!(matches!(rejected, Err(GqlQueryError::Rows(_))));
    assert_eq!(consumed.get(), 0);
    prepared.plan().execute_governed(8, [], input(), |_, _| Ok::<_, Infallible>(true), wide(8), || Ok::<_, Infallible>(())).unwrap();
    assert_eq!(consumed.get(), 8);
    let budgeted = prepared.plan().execute_budgeted(8, [], edges(), |_, _| Ok::<_, Infallible>(true), crate::GqlExecutionBudget::new(8, 1)).unwrap();
    assert_eq!(budgeted.value, vec![VId(0)]);
    assert_eq!(budgeted.stats.result_rows, 1);
}

#[test]
fn every_selected_checkpoint_refuses_without_further_work_or_partial_success() {
    let prepared = forward().prepare_bindings(&["a", "b", "c"], 0, Some(3)).unwrap().with_duplicates();
    let mut total = 0;
    let complete = prepared.plan().execute_governed(8, [], edges(), |_, _| Ok::<_, Infallible>(true), wide(8), || { total += 1; Ok::<_, usize>(()) }).unwrap();
    for stop in 0..total {
        let mut observed = 0;
        let result = prepared.plan().execute_governed(8, [], edges(), |_, _| Ok::<_, Infallible>(true), wide(8), || {
            let at = observed; observed += 1;
            if at == stop { Err(at) } else { Ok(()) }
        });
        assert!(matches!(result, Err(GqlQueryError::Interrupted(at)) if at == stop));
        assert_eq!(observed, stop + 1);
    }
    let exact = GqlQueryPolicy::new(8, 3, complete.evaluator.work_units, complete.evaluator.scratch_entries);
    assert_eq!(prepared.plan().execute_governed(8, [], edges(), |_, _| Ok::<_, Infallible>(true), exact, || Ok::<_, usize>(())).unwrap(), complete);
    for policy in [
        GqlQueryPolicy::new(8, 2, u64::MAX, u64::MAX),
        GqlQueryPolicy::new(8, 3, exact.evaluator.max_work_units - 1, u64::MAX),
        GqlQueryPolicy::new(8, 3, u64::MAX, exact.evaluator.max_scratch_entries - 1),
    ] {
        assert!(prepared.plan().execute_governed(8, [], edges(), |_, _| Ok::<_, Infallible>(true), policy, || Ok::<_, Infallible>(())).is_err());
    }
}

#[test]
fn finite_pages_and_distinct_do_not_expand_exponential_parallel_edge_products() {
    let mut builder = GraphPatternBuilder::default();
    builder.vertex("a").unwrap();
    for _ in 0..50 { builder.edge("a", RelationId(1), GlaDirection::Forward, "a").unwrap(); }
    let input = vec![(VId(0), RelationId(1), VId(0)); 100];
    for duplicate in [false, true] {
        let mut prepared = builder.prepare("a", 0, if duplicate { Some(2) } else { None }).unwrap();
        if duplicate { prepared = prepared.with_duplicates(); }
        assert!(compile(prepared.plan().operators(), &mut allow).unwrap().is_some());
        let result = prepared.plan().execute_governed(100, [], input.clone(), |_, _| Ok::<_, Infallible>(true), GqlQueryPolicy::new(100, 2, 100_000, 100_000), || Ok::<_, Infallible>(())).unwrap();
        assert_eq!(result.value, vec![VId(0); if duplicate { 2 } else { 1 }]);
        // 100^50 occurrences exceed u128; no joined cardinality is fabricated
        // or multiplied merely to retain two identical canonical output rows.
        assert!(result.evaluator.work_units < 100_000);
    }
}
