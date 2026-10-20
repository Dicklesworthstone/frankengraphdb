//! fgdb-20foe: scoped path captures through the public builder and real GLA
//! evaluator. Expected routes are enumerated from identified edges, not from
//! a second invocation of the compiler. This is not storage/transport evidence.

use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::algebra::{
    GlaDirection, GraphColumn, GraphMatchClause, GraphPathFunction, GraphPatternBuilder,
    GraphValue, GraphValueRow, IntegerComparison, MAX_PATTERN_IDENTITIES, PatternBuildError,
    PatternLimitDimension, PreparedGraphPattern, VertexPredicate,
};
use fgdb_gql::{GqlQueryError, GqlQueryPolicy, GraphWalkBounds};
use fgdb_types::{CanonicalPropertyValue, CanonicalScalar, EId, VId};
use std::cell::Cell;

type Edge = (EId, VId, RelationId, VId);
const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const T: RelationId = RelationId(3);
const P: PropertyKeyId = PropertyKeyId(1);

fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100, 100, 1_000_000, 1_000_000)
}

fn vertices(names: &[&str]) -> GraphPatternBuilder {
    let mut builder = GraphPatternBuilder::new();
    for name in names {
        builder.vertex(name).unwrap();
    }
    builder
}

fn number(builder: &mut GraphPatternBuilder, name: &str, value: i64) {
    builder
        .filter(
            name,
            VertexPredicate::IntegerProperty {
                key: P,
                comparison: IntegerComparison::Equal,
                value,
            },
        )
        .unwrap();
}

fn chain() -> GraphPatternBuilder {
    let mut builder = vertices(&["a", "b", "c", "d"]);
    builder.edge("a", R, GlaDirection::Forward, "b").unwrap();
    builder.edge("b", S, GlaDirection::Forward, "c").unwrap();
    builder.edge("c", T, GlaDirection::Forward, "d").unwrap();
    builder.capture_edge("middle", 1).unwrap();
    builder.capture_path("p").unwrap();
    builder
}

fn chain_edges() -> [Edge; 4] {
    [
        (EId(11), VId(1), R, VId(2)),
        (EId(u128::MAX), VId(1), R, VId(2)),
        (EId(21), VId(2), S, VId(3)),
        (EId(31), VId(3), T, VId(4)),
    ]
}

fn run(pattern: &PreparedGraphPattern<GraphValueRow>, edges: &[Edge]) -> Vec<GraphValueRow> {
    pattern
        .plan()
        .execute_governed_with_identified_properties(
            5 + edges.len() as u64,
            (1..=5).map(VId),
            edges.iter().copied(),
            |id, predicates| {
                let properties = [(
                    P,
                    CanonicalPropertyValue::Scalar(CanonicalScalar::Int(id.0 as i64)),
                )];
                Ok::<_, &'static str>(predicates.iter().all(|p| p.matches(&[], &properties)))
            },
            |_, _| Ok(None),
            policy(),
            || Ok::<_, &'static str>(()),
        )
        .unwrap()
        .value
}

#[test]
fn every_correlated_anchor_preserves_declared_segments_and_full_edge_identities() {
    for (anchor, value) in [("a", 1), ("b", 2), ("c", 3), ("d", 4)] {
        let mut outer = vertices(&[anchor]);
        number(&mut outer, anchor, value);
        let inner = chain();
        let pattern = outer
            .prepare_values_with_clauses(
                &[GraphMatchClause::required(&inner)],
                &[
                    GraphColumn::path("route", "p", GraphPathFunction::Value),
                    GraphColumn::path("nodes", "p", GraphPathFunction::Nodes),
                    GraphColumn::path("edges", "p", GraphPathFunction::Edges),
                    GraphColumn::path("length", "p", GraphPathFunction::Length),
                    GraphColumn::path("middle", "middle", GraphPathFunction::Edge),
                ],
                0,
                None,
            )
            .unwrap()
            .with_duplicates();
        let rows = run(&pattern, &chain_edges());
        assert_eq!(rows.len(), 2, "anchor {anchor}");
        for (row, first) in rows.iter().zip([EId(11), EId(u128::MAX)]) {
            let values = row.values();
            let path = values[0]
                .as_path()
                .expect("a complete witness is never a NULL path");
            assert_eq!(path.start(), VId(1));
            assert_eq!(
                path.steps(),
                &[(first, VId(2)), (EId(21), VId(3)), (EId(31), VId(4))]
            );
            assert_eq!(
                values[1],
                GraphValue::Vertices(vec![VId(1), VId(2), VId(3), VId(4)].into_boxed_slice())
            );
            assert_eq!(
                values[2],
                GraphValue::Edges(vec![first, EId(21), EId(31)].into_boxed_slice())
            );
            assert_eq!(values[3], GraphValue::Scalar(CanonicalScalar::Int(3)));
            assert_eq!(values[4], GraphValue::Edge(EId(21)));
        }
        let mut shuffled = chain_edges();
        shuffled.reverse();
        assert_eq!(run(&pattern, &shuffled), rows);
    }
}

#[test]
fn incoming_and_undirected_paths_keep_written_orientation_not_anchor_orientation() {
    for direction in [GlaDirection::Reverse, GlaDirection::Undirected] {
        let mut outer = vertices(&["a"]);
        number(&mut outer, "a", 1);
        let mut inner = vertices(&["d", "c", "b", "a"]);
        inner.edge("d", T, direction, "c").unwrap();
        inner.edge("c", S, direction, "b").unwrap();
        inner.edge("b", R, direction, "a").unwrap();
        inner.capture_path("p").unwrap();
        let pattern = outer
            .prepare_values_with_clauses(
                &[GraphMatchClause::required(&inner)],
                &[GraphColumn::path("p", "p", GraphPathFunction::Value)],
                0,
                None,
            )
            .unwrap();
        let rows = run(&pattern, &chain_edges());
        assert_eq!(rows.len(), 2);
        for (row, last) in rows.iter().zip([EId(11), EId(u128::MAX)]) {
            let path = row.values()[0].as_path().unwrap();
            assert_eq!(path.start(), VId(4));
            assert_eq!(
                path.steps(),
                &[(EId(31), VId(3)), (EId(21), VId(2)), (last, VId(1))]
            );
        }
    }
}

#[test]
fn scoped_shortest_paths_keep_parallel_ties_and_zero_hop_witnesses() {
    let edges = [
        (EId(11), VId(1), R, VId(2)),
        (EId(12), VId(2), R, VId(4)),
        (EId(13), VId(1), R, VId(3)),
        (EId(14), VId(3), R, VId(4)),
        (EId(15), VId(1), R, VId(2)),
    ];
    let mut outer = vertices(&["b"]);
    number(&mut outer, "b", 4);
    for any in [false, true] {
        let mut inner = vertices(&["a", "b"]);
        number(&mut inner, "a", 1);
        let bounds = GraphWalkBounds::new(1, 4).unwrap();
        if any {
            inner
                .any_shortest_walk("a", R, GlaDirection::Forward, "b", bounds)
                .unwrap();
        } else {
            inner
                .shortest_walk("a", R, GlaDirection::Forward, "b", bounds)
                .unwrap();
        }
        inner.capture_path("p").unwrap();
        let pattern = outer
            .prepare_values_with_clauses(
                &[GraphMatchClause::required(&inner)],
                &[GraphColumn::path("p", "p", GraphPathFunction::Value)],
                0,
                None,
            )
            .unwrap()
            .with_duplicates();
        let rows = run(&pattern, &edges);
        let routes: Vec<_> = rows
            .iter()
            .map(|row| {
                let path = row.values()[0].as_path().unwrap();
                assert_eq!(path.start(), VId(1));
                assert_eq!(path.nodes().last(), Some(VId(4)));
                path.edges().collect::<Vec<_>>()
            })
            .collect();
        let expected = vec![
            vec![EId(11), EId(12)],
            vec![EId(13), EId(14)],
            vec![EId(15), EId(12)],
        ];
        assert_eq!(
            routes,
            if any {
                expected[..1].to_vec()
            } else {
                expected
            }
        );
    }
    let mut outer = vertices(&["b"]);
    number(&mut outer, "b", 5);
    let mut inner = vertices(&["a", "b"]);
    inner
        .walk(
            "a",
            R,
            GlaDirection::Forward,
            "b",
            GraphWalkBounds::new(0, 0).unwrap(),
        )
        .unwrap();
    inner.capture_path("p").unwrap();
    let pattern = outer
        .prepare_values_with_clauses(
            &[GraphMatchClause::optional(&inner)],
            &[GraphColumn::path("p", "p", GraphPathFunction::Value)],
            0,
            None,
        )
        .unwrap();
    let rows = run(&pattern, &edges);
    assert_eq!(rows.len(), 1);
    let path = rows[0].values()[0].as_path().unwrap();
    assert_eq!(path.nodes().collect::<Vec<_>>(), vec![VId(5)]);
    assert!(path.is_empty());
}

#[test]
fn optional_path_predicates_use_local_capture_slots_before_null_extension() {
    let mut outer = vertices(&["a", "b"]);
    outer.edge("a", R, GlaDirection::Forward, "b").unwrap();
    outer.capture_edge("root_edge", 0).unwrap();
    let mut inner = vertices(&["b", "c", "d"]);
    inner.edge("b", S, GlaDirection::Forward, "c").unwrap();
    inner.edge("c", T, GlaDirection::Forward, "d").unwrap();
    inner.capture_path("p").unwrap();
    inner
        .filter_path_length("p", IntegerComparison::Equal, 2)
        .unwrap();
    inner
        .filter_path_null("p", GraphPathFunction::Value, false)
        .unwrap();
    let columns = [
        GraphColumn::path("edge", "root_edge", GraphPathFunction::Edge),
        GraphColumn::path("path", "p", GraphPathFunction::Value),
    ];
    let pattern = outer
        .prepare_values_with_clauses(&[GraphMatchClause::optional(&inner)], &columns, 0, None)
        .unwrap()
        .with_duplicates();
    let rows = run(&pattern, &chain_edges());
    assert_eq!(rows.len(), 2);
    for row in &rows {
        let path = row.values()[1].as_path().unwrap();
        assert_eq!(path.start(), VId(2));
        assert_eq!(path.steps(), &[(EId(21), VId(3)), (EId(31), VId(4))]);
    }
    // The root edge is present, but the child's complete predicate rejects
    // every path. There must be one NULL-extended row per outer occurrence.
    inner
        .filter_path_null("p", GraphPathFunction::Value, true)
        .unwrap();
    let absent = outer
        .prepare_values_with_clauses(&[GraphMatchClause::optional(&inner)], &columns, 0, None)
        .unwrap()
        .with_duplicates();
    let rows = run(&absent, &chain_edges());
    assert_eq!(rows.len(), 2);
    for (row, id) in rows.iter().zip([EId(11), EId(u128::MAX)]) {
        assert_eq!(row.values()[0], GraphValue::Edge(id));
        assert_eq!(row.values()[1], GraphValue::Scalar(CanonicalScalar::Null));
    }
}

#[test]
fn nullable_destination_correlation_cannot_be_rebound_by_a_path_scan() {
    let mut outer = vertices(&["root"]);
    number(&mut outer, "root", 5);
    let mut missing = vertices(&["root", "d"]);
    missing.edge("root", T, GlaDirection::Forward, "d").unwrap();
    let inner = chain();
    for optional in [false, true] {
        let clause = if optional {
            GraphMatchClause::optional(&inner)
        } else {
            GraphMatchClause::required(&inner)
        };
        let pattern = outer
            .prepare_values_with_clauses(
                &[GraphMatchClause::optional(&missing), clause],
                &[
                    GraphColumn::vertex("d", "d"),
                    GraphColumn::path("p", "p", GraphPathFunction::Value),
                ],
                0,
                None,
            )
            .unwrap();
        let rows = run(&pattern, &chain_edges());
        if optional {
            assert_eq!(rows.len(), 1);
            assert_eq!(
                rows[0].values(),
                &[
                    GraphValue::Scalar(CanonicalScalar::Null),
                    GraphValue::Scalar(CanonicalScalar::Null)
                ]
            );
        } else {
            assert!(rows.is_empty());
        }
    }
}

#[test]
fn private_paths_in_existence_scopes_never_escape_or_replace_exported_captures() {
    let outer = vertices(&["d"]);
    let inner = chain();
    let private = GraphColumn::path("p", "p", GraphPathFunction::Value);
    assert!(
        outer
            .prepare_values_with_clauses(&[GraphMatchClause::exists(&inner)], &[private], 0, None,)
            .is_err()
    );
    let columns = [GraphColumn::vertex("d", "d")];
    let positive = outer
        .prepare_values_with_clauses(&[GraphMatchClause::exists(&inner)], &columns, 0, None)
        .unwrap()
        .with_duplicates();
    let rows = run(&positive, &chain_edges());
    assert_eq!(rows.len(), 1); // a semi-join does not multiply the parallel paths
    assert_eq!(rows[0].values(), &[GraphValue::Vertex(VId(4))]);
    let negative = outer
        .prepare_values_with_clauses(&[GraphMatchClause::not_exists(&inner)], &columns, 0, None)
        .unwrap();
    let rows = run(&negative, &chain_edges());
    assert_eq!(
        rows.iter()
            .map(|row| row.values()[0].as_vertex().unwrap())
            .collect::<Vec<_>>(),
        vec![VId(1), VId(2), VId(3), VId(5)]
    );
}

#[test]
fn captured_scopes_retain_definition_and_runtime_limits_and_cancellation() {
    let mut outer = vertices(&["a", "b"]);
    outer.edge("a", R, GlaDirection::Forward, "b").unwrap();
    for at in 0..MAX_PATTERN_IDENTITIES {
        outer.capture_edge(&format!("root_{at}"), 0).unwrap();
    }
    let inner = chain();
    assert!(matches!(
        outer.prepare_values_with_clauses(
            &[GraphMatchClause::required(&inner)],
            &[GraphColumn::path("p", "p", GraphPathFunction::Value)],
            0,
            Some(0),
        ),
        Err(PatternBuildError::LimitExceeded {
            dimension: PatternLimitDimension::PathCaptures,
            ..
        })
    ));
    let outer = vertices(&["d"]);
    let pattern = outer
        .prepare_values_with_clauses(
            &[GraphMatchClause::required(&inner)],
            &[GraphColumn::path("p", "p", GraphPathFunction::Value)],
            0,
            None,
        )
        .unwrap();
    let calls = Cell::new(0);
    let execute = |budget, stop| {
        calls.set(0);
        pattern.plan().execute_governed_with_identified_properties(
            9,
            (1..=5).map(VId),
            chain_edges(),
            |_, _| Ok::<_, &'static str>(true),
            |_, _| Ok(None),
            budget,
            || {
                let at = calls.get();
                calls.set(at + 1);
                if at == stop { Err("cancel") } else { Ok(()) }
            },
        )
    };
    let wide = execute(policy(), usize::MAX).unwrap();
    let checkpoints = calls.get();
    assert!(checkpoints > 0);
    let exact = GqlQueryPolicy::new(
        9,
        2,
        wide.evaluator.work_units,
        wide.evaluator.scratch_entries,
    );
    assert_eq!(execute(exact, usize::MAX).unwrap().value, wide.value);
    assert!(matches!(
        execute(GqlQueryPolicy::new(9, 1, 1_000_000, 1_000_000), usize::MAX),
        Err(GqlQueryError::Rows(_))
    ));
    assert!(
        execute(
            GqlQueryPolicy::new(9, 2, wide.evaluator.work_units - 1, 1_000_000),
            usize::MAX
        )
        .is_err()
    );
    assert!(
        execute(
            GqlQueryPolicy::new(9, 2, 1_000_000, wide.evaluator.scratch_entries - 1),
            usize::MAX
        )
        .is_err()
    );
    for stop in 0..checkpoints {
        assert!(
            matches!(
                execute(policy(), stop),
                Err(GqlQueryError::Interrupted("cancel"))
            ),
            "checkpoint {stop}"
        );
    }
}
