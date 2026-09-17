//! Required MATCH composes with nullable and existential clauses in source order.
//! Expected bags are ordinary relational joins over raw edge occurrences.

use fgdb_delta_types::{LabelId, RelationId};
use fgdb_gql::algebra::{
    GlaDirection, GlaOperator, GraphColumn, GraphMatchClause, GraphPatternBuilder, GraphValueRow,
    PatternBuildError, PatternLimitDimension, PreparedGraphPattern, VertexPredicate,
};
use fgdb_gql::{GqlQueryError, GqlQueryPolicy, GraphWalkBounds};
use fgdb_types::VId;

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const T: RelationId = RelationId(3);
type Edge = (VId, RelationId, VId);

fn builder(names: &[&str]) -> GraphPatternBuilder {
    let mut result = GraphPatternBuilder::new();
    for name in names {
        result.vertex(name).unwrap();
    }
    result
}
fn wide() -> GqlQueryPolicy {
    GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX)
}
fn cells(rows: &[GraphValueRow]) -> Vec<Vec<Option<VId>>> {
    rows.iter()
        .map(|row| row.values().iter().map(|v| v.as_vertex()).collect())
        .collect()
}
fn oriented(edges: &[Edge], relation: RelationId, direction: GlaDirection) -> Vec<(VId, VId)> {
    let mut rows = Vec::new();
    for &(source, actual, destination) in edges {
        if actual != relation {
            continue;
        }
        match direction {
            GlaDirection::Forward => rows.push((source, destination)),
            GlaDirection::Reverse => rows.push((destination, source)),
            GlaDirection::Undirected => {
                rows.push((source, destination));
                if source != destination {
                    rows.push((destination, source));
                }
            }
        }
    }
    rows
}

#[test]
fn optional_then_required_bags_match_independent_relational_composition() {
    let vertices = [VId(1), VId(2), VId(3), VId(9)];
    let universe = [
        (VId(1), R, VId(2)),
        (VId(1), R, VId(2)),
        (VId(2), R, VId(3)),
        (VId(3), R, VId(3)),
        (VId(2), S, VId(3)),
        (VId(2), S, VId(3)),
        (VId(3), S, VId(1)),
    ];
    for mask in 0..128_usize {
        let edges = universe
            .iter()
            .enumerate()
            .filter(|(at, _)| mask & (1 << at) != 0)
            .map(|(_, edge)| *edge)
            .collect::<Vec<_>>();
        for direction in [
            GlaDirection::Forward,
            GlaDirection::Reverse,
            GlaDirection::Undirected,
        ] {
            let left = oriented(&edges, R, direction);
            let right = oriented(&edges, S, direction);
            for independent in [false, true] {
                let root = builder(&["a"]);
                let mut optional = builder(&["a", "b"]);
                optional.edge("a", R, direction, "b").unwrap();
                let anchor = if independent { "x" } else { "b" };
                let mut required = builder(&[anchor, "c"]);
                required.edge(anchor, S, direction, "c").unwrap();
                let clauses = [
                    GraphMatchClause::optional(&optional),
                    GraphMatchClause::required(&required),
                ];
                let columns = [
                    GraphColumn::vertex("a", "a"),
                    GraphColumn::vertex("b", "b"),
                    GraphColumn::vertex("c", "c"),
                ];
                let mut expected = Vec::new();
                for a in vertices {
                    let mut optional_rows = left
                        .iter()
                        .filter(|(source, _)| *source == a)
                        .map(|(_, b)| Some(*b))
                        .collect::<Vec<_>>();
                    if optional_rows.is_empty() {
                        optional_rows.push(None);
                    }
                    for b in optional_rows {
                        for &(source, c) in &right {
                            if independent || b == Some(source) {
                                expected.push(vec![Some(a), b, Some(c)]);
                            }
                        }
                    }
                }
                expected.sort();
                for distinct in [false, true] {
                    let mut expected = expected.clone();
                    if distinct {
                        expected.dedup();
                    }
                    for (offset, count) in [(0, None), (1, Some(3)), (0, Some(0))] {
                        let pattern = root
                            .prepare_values_with_clauses(&clauses, &columns, offset, count)
                            .unwrap();
                        let pattern = if distinct {
                            pattern
                        } else {
                            pattern.with_duplicates()
                        };
                        let actual = pattern
                            .plan()
                            .execute_governed_with_properties(
                                (vertices.len() + edges.len()) as u64,
                                vertices,
                                edges.iter().copied(),
                                |_, _| Ok::<_, ()>(true),
                                |_, _| Ok(None),
                                wide(),
                                || Ok::<_, ()>(()),
                            )
                            .unwrap();
                        let page = expected
                            .iter()
                            .skip(offset as usize)
                            .take(count.unwrap_or(u64::MAX) as usize)
                            .cloned()
                            .collect::<Vec<_>>();
                        assert_eq!(
                            cells(&actual.value),
                            page,
                            "mask={mask}, direction={direction:?}, independent={independent}, distinct={distinct}"
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn required_clauses_close_all_correlations_and_export_bindings_to_later_scopes() {
    let mut root = builder(&["a", "b"]);
    root.edge("a", R, GlaDirection::Forward, "b").unwrap();
    let mut close = builder(&["c", "b", "a"]);
    // The existing outer binding is the written destination; lowering must
    // reverse the first atom without losing either of the two correlations.
    close.edge("c", S, GlaDirection::Reverse, "b").unwrap();
    close.edge("c", T, GlaDirection::Forward, "a").unwrap();
    let mut next = builder(&["c", "d"]);
    next.any_shortest_walk(
        "c",
        R,
        GlaDirection::Forward,
        "d",
        GraphWalkBounds::new(0, 0).unwrap(),
    )
    .unwrap();
    let mut probe = builder(&["d", "private"]);
    probe
        .edge("d", S, GlaDirection::Forward, "private")
        .unwrap();
    let clauses = [
        GraphMatchClause::required(&close),
        GraphMatchClause::required(&next),
        GraphMatchClause::exists(&probe),
    ];
    let pattern = root
        .prepare_values_with_clauses(
            &clauses,
            &[
                GraphColumn::vertex("a", "a"),
                GraphColumn::vertex("c", "c"),
                GraphColumn::vertex("d", "d"),
            ],
            0,
            None,
        )
        .unwrap()
        .with_duplicates();
    let edges = [
        (VId(1), R, VId(2)),
        (VId(1), R, VId(2)),
        (VId(2), S, VId(3)),
        (VId(2), S, VId(3)),
        (VId(3), T, VId(1)),
        (VId(3), T, VId(9)),
        (VId(3), S, VId(9)),
    ];
    let rows = pattern
        .plan()
        .execute_governed_with_properties(
            11,
            [VId(1), VId(2), VId(3), VId(9)],
            edges,
            |_, _| Ok::<_, ()>(true),
            |_, _| Ok(None),
            wide(),
            || Ok::<_, ()>(()),
        )
        .unwrap();
    assert_eq!(
        cells(&rows.value),
        vec![vec![Some(VId(1)), Some(VId(3)), Some(VId(3))]; 4]
    );
    assert!(matches!(
        root.prepare_values_with_clauses(
            &clauses,
            &[GraphColumn::vertex("hidden", "private")],
            0,
            None
        ),
        Err(PatternBuildError::UnknownVariable)
    ));
    assert_eq!(
        pattern
            .plan()
            .operators()
            .iter()
            .filter(|op| matches!(op, GlaOperator::Probe { .. }))
            .count(),
        1
    );
    assert!(
        !pattern
            .plan()
            .operators()
            .iter()
            .any(|op| matches!(op, GlaOperator::Optional { .. }))
    );
}

#[test]
fn required_zero_hops_never_rebind_an_optional_null() {
    let root = builder(&["a"]);
    let mut optional = builder(&["a", "b"]);
    optional.edge("a", R, GlaDirection::Forward, "b").unwrap();
    for mode in 0..3 {
        let mut required = builder(&["b", "c"]);
        let bounds = GraphWalkBounds::new(0, 0).unwrap();
        match mode {
            0 => required
                .walk("b", R, GlaDirection::Forward, "c", bounds)
                .unwrap(),
            1 => required
                .shortest_walk("b", R, GlaDirection::Forward, "c", bounds)
                .unwrap(),
            _ => required
                .any_shortest_walk("b", R, GlaDirection::Forward, "c", bounds)
                .unwrap(),
        };
        let pattern = root
            .prepare_values_with_clauses(
                &[
                    GraphMatchClause::optional(&optional),
                    GraphMatchClause::required(&required),
                ],
                &[GraphColumn::vertex("a", "a"), GraphColumn::vertex("c", "c")],
                0,
                None,
            )
            .unwrap()
            .with_duplicates();
        let rows = pattern
            .plan()
            .execute_governed_with_properties(
                2,
                [VId(0), VId(u128::MAX)],
                [],
                |_, _| Ok::<_, ()>(true),
                |_, _| Ok(None),
                wide(),
                || Ok::<_, ()>(()),
            )
            .unwrap();
        assert!(rows.value.is_empty(), "null was rebound in mode {mode}");
    }
}

#[test]
fn independent_required_scan_preserves_nulls_and_uses_the_complete_vertex_domain() {
    let mut root = builder(&["a"]);
    root.filter("a", VertexPredicate::HasLabel(LabelId(1)))
        .unwrap();
    let mut optional = builder(&["a", "b"]);
    optional.edge("a", R, GlaDirection::Forward, "b").unwrap();
    let required = builder(&["x"]);
    let pattern = root
        .prepare_values_with_clauses(
            &[
                GraphMatchClause::optional(&optional),
                GraphMatchClause::required(&required),
            ],
            &[GraphColumn::vertex("b", "b"), GraphColumn::vertex("x", "x")],
            0,
            None,
        )
        .unwrap()
        .with_duplicates();
    assert_eq!(pattern.required_vertex_label(), None);
    let rows = pattern
        .plan()
        .execute_governed_with_properties(
            2,
            [VId(1), VId(2)],
            [],
            |vid, predicates| Ok::<_, ()>(predicates.is_empty() || vid == VId(1)),
            |_, _| Ok(None),
            wide(),
            || Ok::<_, ()>(()),
        )
        .unwrap();
    assert_eq!(
        cells(&rows.value),
        vec![vec![None, Some(VId(1))], vec![None, Some(VId(2))]]
    );
}

#[test]
fn required_clauses_obey_definition_wide_limits_before_execution() {
    let root = builder(&["a"]);
    let repeat = builder(&["a"]);
    let columns = [GraphColumn::vertex("a", "a")];
    let clauses = [GraphMatchClause::required(&repeat); 64];
    let frozen = root
        .prepare_values_with_clauses(&clauses, &columns, 0, None)
        .unwrap();
    assert!(matches!(
        root.prepare_values_with_clauses(
            &[GraphMatchClause::required(&repeat); 65],
            &columns,
            0,
            None
        ),
        Err(PatternBuildError::LimitExceeded {
            dimension: PatternLimitDimension::Identities,
            ..
        })
    ));
    let wide_frame = builder(&["a", "b", "c", "d"]);
    assert!(matches!(
        root.prepare_values_with_clauses(
            &[GraphMatchClause::required(&wide_frame); 64],
            &columns,
            0,
            None
        ),
        Err(PatternBuildError::LimitExceeded {
            dimension: PatternLimitDimension::Bindings,
            ..
        })
    ));
    assert_eq!(
        root.prepare_values_with_clauses(&clauses, &columns, 0, None)
            .unwrap(),
        frozen
    );
    let empty = GraphPatternBuilder::new();
    assert!(matches!(
        root.prepare_values_with_clauses(&[GraphMatchClause::required(&empty)], &columns, 0, None),
        Err(PatternBuildError::EmptyPattern)
    ));
}

fn joined() -> PreparedGraphPattern<GraphValueRow> {
    let root = builder(&["a"]);
    let mut optional = builder(&["a", "b"]);
    optional.edge("a", R, GlaDirection::Forward, "b").unwrap();
    let mut required = builder(&["b", "c"]);
    required.edge("b", S, GlaDirection::Forward, "c").unwrap();
    root.prepare_values_with_clauses(
        &[
            GraphMatchClause::optional(&optional),
            GraphMatchClause::required(&required),
        ],
        &[GraphColumn::vertex("c", "c")],
        0,
        None,
    )
    .unwrap()
    .with_duplicates()
}

#[test]
fn one_policy_governs_the_complete_join_and_every_interruption_boundary() {
    let pattern = joined();
    let vertices = [VId(1), VId(2), VId(3)];
    let edges = [
        (VId(1), R, VId(2)),
        (VId(1), R, VId(2)),
        (VId(2), S, VId(3)),
    ];
    let run = |policy| {
        pattern.plan().execute_governed_with_properties(
            6,
            vertices,
            edges,
            |_, _| Ok::<_, ()>(true),
            |_, _| Ok(None),
            policy,
            || Ok::<_, ()>(()),
        )
    };
    let measured = run(wide()).unwrap();
    assert_eq!(measured.value.len(), 2);
    let caps = [
        measured.rows.snapshot_records,
        measured.rows.result_rows,
        measured.evaluator.work_units,
        measured.evaluator.scratch_entries,
    ];
    assert_eq!(
        run(GqlQueryPolicy::new(caps[0], caps[1], caps[2], caps[3])).unwrap(),
        measured
    );
    for dimension in 0..4 {
        let mut short = caps;
        short[dimension] -= 1;
        assert!(run(GqlQueryPolicy::new(short[0], short[1], short[2], short[3])).is_err());
    }
    let mut total = 0;
    pattern
        .plan()
        .execute_governed_with_properties(
            6,
            vertices,
            edges,
            |_, _| Ok::<_, ()>(true),
            |_, _| Ok(None),
            wide(),
            || {
                total += 1;
                Ok::<_, usize>(())
            },
        )
        .unwrap();
    for stop in 1..=total {
        let mut at = 0;
        let result = pattern.plan().execute_governed_with_properties(
            6,
            vertices,
            edges,
            |_, _| Ok::<_, ()>(true),
            |_, _| Ok(None),
            wide(),
            || {
                at += 1;
                if at == stop { Err(stop) } else { Ok(()) }
            },
        );
        assert!(matches!(result, Err(GqlQueryError::Interrupted(value)) if value == stop));
        assert_eq!(at, stop);
    }
}

#[test]
fn late_required_source_failure_is_not_absence_or_a_partial_result() {
    let root = builder(&["a"]);
    let mut optional = builder(&["a", "b"]);
    optional.edge("a", R, GlaDirection::Forward, "b").unwrap();
    let mut required = builder(&["b", "c"]);
    required.edge("b", S, GlaDirection::Forward, "c").unwrap();
    required
        .filter("c", VertexPredicate::HasLabel(LabelId(7)))
        .unwrap();
    for count in [None, Some(0)] {
        let pattern = root
            .prepare_values_with_clauses(
                &[
                    GraphMatchClause::optional(&optional),
                    GraphMatchClause::required(&required),
                ],
                &[GraphColumn::vertex("a", "a")],
                0,
                count,
            )
            .unwrap();
        let mut reads = 0;
        let result = pattern.plan().execute_governed_with_properties(
            5,
            [VId(1), VId(2), VId(3)],
            [(VId(1), R, VId(2)), (VId(2), S, VId(3))],
            |_, _| {
                reads += 1;
                Err("required source refused")
            },
            |_, _| Ok(None),
            wide(),
            || Ok::<_, ()>(()),
        );
        assert!(matches!(
            result,
            Err(GqlQueryError::Source("required source refused"))
        ));
        assert_eq!(reads, 1);
    }
}
