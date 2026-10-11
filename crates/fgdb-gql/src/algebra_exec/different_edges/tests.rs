//! Public native preparation, source admission and clause-boundary laws.

use super::*;
use crate::algebra::{
    EdgeRelation, GlaDirection, GlaPlan, GraphColumn, GraphMatchClause, GraphMatchMode,
    GraphPatternBuilder, GraphValue, GraphValueOrder, GraphValueRow, PatternBuildError,
    PreparedGraphPattern,
};
use crate::{GqlQueryError, GqlQueryPolicy, GraphWalkBounds};
use fgdb_delta_types::RelationId;
use fgdb_types::VId;
use std::cell::Cell;

const R: RelationId = RelationId(1);
type Edge = (EId, VId, RelationId, VId);

fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100, 100_000, 2_000_000, 2_000_000)
}

fn builder(names: &[&str]) -> GraphPatternBuilder {
    let mut builder = GraphPatternBuilder::new();
    builder.match_mode(GraphMatchMode::DifferentEdges);
    for name in names {
        builder.vertex(name).unwrap();
    }
    builder
}

fn prepare(builder: &GraphPatternBuilder, names: &[&str]) -> PreparedGraphPattern<GraphValueRow> {
    builder
        .prepare_values(
            &names
                .iter()
                .map(|name| GraphColumn::vertex(name, name))
                .collect::<Vec<_>>(),
            0,
            None,
        )
        .unwrap()
        .with_duplicates()
}

fn execute(
    plan: &GlaPlan<GraphValueRow>,
    edges: &[Edge],
) -> crate::GqlQueryExecution<GraphValueRow> {
    plan.execute_governed_with_identified_properties(
        (edges.len() + 3) as u64,
        [VId(1), VId(2), VId(3)],
        edges.iter().copied(),
        |_, _| Ok::<_, ()>(true),
        |_, _| Ok(None),
        policy(),
        || Ok::<_, ()>(()),
    )
    .unwrap()
}

#[test]
fn native_mode_retains_generic_default_and_requires_an_identified_source() {
    let mut different = builder(&["a", "b", "c"]);
    different.edge("a", R, GlaDirection::Forward, "b").unwrap();
    let one = prepare(&different, &["a", "b"]);
    assert!(!one.plan().requires_identified_edges());
    different
        .edge("b", R, GlaDirection::Undirected, "c")
        .unwrap();
    assert_eq!(
        different.prepare("c", 0, None).unwrap_err(),
        PatternBuildError::RequiresValueProjection
    );
    assert_eq!(
        different.prepare_bindings(&["c"], 0, None).unwrap_err(),
        PatternBuildError::RequiresValueProjection
    );
    let query = prepare(&different, &["a", "b", "c"]);
    assert!(query.plan().requires_identified_edges());
    let mut repeatable = different.clone();
    repeatable.match_mode(GraphMatchMode::RepeatableElements);
    let mut default = GraphPatternBuilder::new();
    for name in ["a", "b", "c"] {
        default.vertex(name).unwrap();
    }
    default.edge("a", R, GlaDirection::Forward, "b").unwrap();
    default.edge("b", R, GlaDirection::Undirected, "c").unwrap();
    assert_eq!(
        default.canonical_template_bytes(),
        repeatable.canonical_template_bytes()
    );
    assert_ne!(
        different.canonical_template_bytes(),
        repeatable.canonical_template_bytes()
    );
    assert_ne!(
        query.canonical_bytes(),
        prepare(&repeatable, &["a", "b", "c"]).canonical_bytes()
    );
    let consumed = Cell::new(0);
    let result = query.plan().execute_governed_with_properties(
        4,
        [VId(1), VId(2), VId(3)],
        [(VId(1), R, VId(2))]
            .into_iter()
            .inspect(|_| consumed.set(consumed.get() + 1)),
        |_, _| Ok::<_, ()>(true),
        |_, _| Ok(None),
        policy(),
        || Ok::<_, ()>(()),
    );
    assert!(matches!(
        result,
        Err(GqlQueryError::IdentifiedEdgesRequired)
    ));
    assert_eq!(consumed.get(), 0);
}

#[test]
fn disjoint_fixed_relation_types_keep_the_existing_identity_free_path() {
    let mut pattern = builder(&["a", "b", "c"]);
    pattern.edge("a", R, GlaDirection::Forward, "b").unwrap();
    pattern
        .edge("b", RelationId(2), GlaDirection::Forward, "c")
        .unwrap();
    let query = prepare(&pattern, &["c"]);
    assert!(!query.plan().requires_identified_edges());
    assert!(pattern.prepare("c", 0, None).is_ok());
    let mut repeatable = pattern.clone();
    repeatable.match_mode(GraphMatchMode::RepeatableElements);
    assert_eq!(
        query.canonical_bytes(),
        prepare(&repeatable, &["c"]).canonical_bytes()
    );
    pattern
        .vertex("d")
        .unwrap()
        .edge("c", EdgeRelation::Any, GlaDirection::Reverse, "d")
        .unwrap();
    let query = prepare(&pattern, &["d"]);
    assert!(query.plan().requires_identified_edges());
    let groups = query
        .plan()
        .operators()
        .iter()
        .filter_map(|op| match op {
            GlaOperator::DifferentEdges { segments } => Some(segments.len()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        groups,
        vec![3],
        "an untyped atom overlaps both earlier relation domains"
    );
}

#[test]
fn compiled_expansion_constraint_and_ordering_tags_cannot_collide() {
    let mut pattern = builder(&["a", "b"]);
    pattern
        .acyclic_walk(
            "a",
            R,
            GlaDirection::Forward,
            "b",
            GraphWalkBounds::new(1, 3).unwrap(),
        )
        .unwrap();
    let query = prepare(&pattern, &["b"])
        .with_order_by(&[GraphValueOrder::ascending(0)])
        .unwrap();
    let header = GlaPlan::<GraphValueRow>::from_operators(Vec::new())
        .canonical_bytes()
        .len();
    let tags = query
        .plan()
        .operators()
        .iter()
        .filter(|op| {
            matches!(
                op,
                GlaOperator::VarLengthExpand { .. }
                    | GlaOperator::DifferentEdges { .. }
                    | GlaOperator::OrderByValueColumns { .. }
            )
        })
        .map(|op| {
            // Encode operators produced by one valid public prepared definition;
            // isolate their application tags from their different payload shapes.
            GlaPlan::<GraphValueRow>::from_operators(vec![op.clone()]).canonical_bytes()[header]
        })
        .collect::<Vec<_>>();
    assert_eq!(tags.len(), 3);
    assert_eq!(
        tags.iter()
            .copied()
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        3
    );
}

#[test]
fn fixed_atoms_preserve_parallel_bags_but_never_reuse_an_oriented_edge() {
    let candidates = [
        (EId(11), VId(1), R, VId(1)),
        (EId(12), VId(1), R, VId(2)),
        (EId(13), VId(1), R, VId(2)),
        (EId(14), VId(2), R, VId(1)),
        (EId(15), VId(2), R, VId(3)),
    ];
    for direction in [
        GlaDirection::Forward,
        GlaDirection::Reverse,
        GlaDirection::Undirected,
    ] {
        let mut pattern = builder(&["a", "b", "c"]);
        pattern.edge("a", R, direction, "b").unwrap();
        pattern.edge("b", R, direction, "c").unwrap();
        let query = prepare(&pattern, &["a", "b", "c"]);
        for mask in 0..32 {
            let edges: Vec<_> = candidates
                .iter()
                .enumerate()
                .filter(|(at, _)| mask & (1 << at) != 0)
                .map(|(_, edge)| *edge)
                .collect();
            let mut oriented = Vec::new();
            for &(eid, source, _, target) in &edges {
                let (source, target) = if direction == GlaDirection::Reverse {
                    (target, source)
                } else {
                    (source, target)
                };
                oriented.push((eid, source, target));
                if direction == GlaDirection::Undirected && source != target {
                    oriented.push((eid, target, source));
                }
            }
            let mut expected = Vec::new();
            for &(first, a, b) in &oriented {
                for &(second, source, c) in &oriented {
                    if b == source && first != second {
                        expected.push(vec![
                            GraphValue::Vertex(a),
                            GraphValue::Vertex(b),
                            GraphValue::Vertex(c),
                        ]);
                    }
                }
            }
            expected.sort();
            let actual = execute(query.plan(), &edges)
                .value
                .into_iter()
                .map(|row| row.values().to_vec())
                .collect::<Vec<_>>();
            assert_eq!(actual, expected, "mask={mask}, direction={direction:?}");
        }
    }
    // Mutation control: erasing the logical constraint restores the incorrect
    // backwards reuse. The law depends on the shipped operator and executor.
    let mut pattern = builder(&["a", "b", "c"]);
    pattern.edge("a", R, GlaDirection::Forward, "b").unwrap();
    pattern.edge("b", R, GlaDirection::Undirected, "c").unwrap();
    let query = prepare(&pattern, &["c"]);
    let one = [(EId(1), VId(1), R, VId(2))];
    assert!(execute(query.plan(), &one).value.is_empty());
    let mutated = GlaPlan::from_operators(
        query
            .plan()
            .operators()
            .iter()
            .filter(|op| !matches!(op, GlaOperator::DifferentEdges { .. }))
            .cloned()
            .collect(),
    );
    assert_eq!(execute(&mutated, &one).value.len(), 1);
}

#[test]
fn disconnected_parts_share_edges_but_required_optional_and_probe_bodies_do_not() {
    let one = [(EId(1), VId(1), R, VId(2))];
    let mut disconnected = builder(&["a", "b", "c", "d"]);
    disconnected
        .edge("a", R, GlaDirection::Forward, "b")
        .unwrap();
    disconnected
        .edge("c", R, GlaDirection::Forward, "d")
        .unwrap();
    assert!(
        execute(prepare(&disconnected, &["a", "d"]).plan(), &one)
            .value
            .is_empty()
    );

    let mut outer = builder(&["a", "b"]);
    outer.edge("a", R, GlaDirection::Forward, "b").unwrap();
    let mut child = builder(&["b", "c"]);
    // A singleton quantified child still has its own DifferentEdges constraint.
    child
        .walk(
            "b",
            R,
            GlaDirection::Reverse,
            "c",
            GraphWalkBounds::new(1, 1).unwrap(),
        )
        .unwrap();
    for clause in [
        GraphMatchClause::required(&child),
        GraphMatchClause::optional(&child),
    ] {
        let query = outer
            .prepare_values_with_clauses(
                &[clause],
                &[GraphColumn::vertex("a", "a"), GraphColumn::vertex("c", "c")],
                0,
                None,
            )
            .unwrap()
            .with_duplicates();
        let rows = execute(query.plan(), &one).value;
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].values(),
            &[GraphValue::Vertex(VId(1)), GraphValue::Vertex(VId(1))]
        );
    }
    let exists = outer
        .prepare_values_with_clauses(
            &[GraphMatchClause::exists(&child)],
            &[GraphColumn::vertex("a", "a")],
            0,
            None,
        )
        .unwrap();
    assert_eq!(execute(exists.plan(), &one).value.len(), 1);
    let anti = outer
        .prepare_values_with_clauses(
            &[GraphMatchClause::not_exists(&child)],
            &[GraphColumn::vertex("a", "a")],
            0,
            None,
        )
        .unwrap();
    assert!(execute(anti.plan(), &one).value.is_empty());

    child
        .vertex("d")
        .unwrap()
        .edge("c", R, GlaDirection::Forward, "d")
        .unwrap();
    let absent = outer
        .prepare_values_with_clauses(
            &[GraphMatchClause::optional(&child)],
            &[GraphColumn::vertex("a", "a"), GraphColumn::vertex("d", "d")],
            0,
            None,
        )
        .unwrap();
    let rows = execute(absent.plan(), &one).value;
    assert_eq!(rows.len(), 1);
    assert!(rows[0].get(1).unwrap().is_null());
}

#[test]
fn quantified_atoms_consume_only_actual_steps_and_prior_clause_segments() {
    let edges = [(EId(1), VId(1), R, VId(2)), (EId(2), VId(1), R, VId(2))];
    for (low, high, expected) in [(0, 0, 2), (1, 1, 2), (2, 2, 0), (0, 2, 4)] {
        let mut pattern = builder(&["a", "b", "c"]);
        pattern.edge("a", R, GlaDirection::Forward, "b").unwrap();
        pattern
            .walk(
                "b",
                R,
                GlaDirection::Undirected,
                "c",
                GraphWalkBounds::new(low, high).unwrap(),
            )
            .unwrap();
        assert_eq!(
            execute(prepare(&pattern, &["a", "c"]).plan(), &edges)
                .value
                .len(),
            expected
        );
    }
    let mut high = builder(&["a", "b"]);
    high.walk(
        "a",
        R,
        GlaDirection::Undirected,
        "b",
        GraphWalkBounds::new(crate::MAX_GRAPH_WALK_HOPS, crate::MAX_GRAPH_WALK_HOPS).unwrap(),
    )
    .unwrap();
    let execution = execute(prepare(&high, &["b"]).plan(), &edges[..1]);
    assert!(execution.value.is_empty());
    assert!(
        execution.evaluator.work_units < 100,
        "impossible trails stop at their exhausted frontier"
    );
}

#[test]
fn different_edges_budget_includes_membership_and_never_returns_a_cancelled_prefix() {
    let mut pattern = builder(&["a", "b", "c"]);
    pattern.edge("a", R, GlaDirection::Forward, "b").unwrap();
    pattern
        .walk(
            "b",
            R,
            GlaDirection::Undirected,
            "c",
            GraphWalkBounds::new(0, 2).unwrap(),
        )
        .unwrap();
    let query = prepare(&pattern, &["a", "c"]);
    let edges = [(EId(1), VId(1), R, VId(2)), (EId(2), VId(1), R, VId(2))];
    let calls = Cell::new(0);
    let run = |budget, stop| {
        calls.set(0);
        query.plan().execute_governed_with_identified_properties(
            5,
            [VId(1), VId(2), VId(3)],
            edges,
            |_, _| Ok::<_, ()>(true),
            |_, _| Ok(None),
            budget,
            || {
                let at = calls.get() + 1;
                calls.set(at);
                if at == stop { Err(stop) } else { Ok(()) }
            },
        )
    };
    let measured = run(policy(), usize::MAX).unwrap();
    let total = calls.get();
    let caps = [
        5,
        measured.rows.result_rows,
        measured.evaluator.work_units,
        measured.evaluator.scratch_entries,
    ];
    assert_eq!(
        run(
            GqlQueryPolicy::new(caps[0], caps[1], caps[2], caps[3]),
            usize::MAX
        )
        .unwrap(),
        measured
    );
    for dimension in 0..4 {
        let mut cap = caps;
        cap[dimension] -= 1;
        assert!(
            run(
                GqlQueryPolicy::new(cap[0], cap[1], cap[2], cap[3]),
                usize::MAX
            )
            .is_err()
        );
    }
    for stop in 1..=total {
        assert!(matches!(run(policy(), stop), Err(GqlQueryError::Interrupted(at)) if at == stop));
        assert_eq!(calls.get(), stop);
    }
}
