//! Outer predicate values are not additional positive graph matches.
//! The reference below joins raw occurrences and evaluates nulls independently.

use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::algebra::{
    GlaDirection, GlaOperator, GraphBooleanExpression, GraphBooleanOp as Op,
    GraphBooleanOperand as Operand, GraphColumn, GraphMatchClause, GraphPatternBuilder,
    GraphValueRow, IntegerComparison, MAX_PATTERN_VERTICES, PatternBuildError,
    PatternLimitDimension, PreparedGraphPattern, VertexPredicate,
};
use fgdb_gql::{GqlQueryError, GqlQueryPolicy};
use fgdb_types::{CanonicalScalar, VId};
use std::collections::BTreeMap;

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const P: PropertyKeyId = PropertyKeyId(1);
type Edge = (VId, RelationId, VId);

fn builder(names: &[&str]) -> GraphPatternBuilder {
    let mut value = GraphPatternBuilder::new();
    for name in names {
        value.vertex(name).unwrap();
    }
    value
}
fn wide() -> GqlQueryPolicy {
    GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX)
}
fn cells(rows: &[GraphValueRow]) -> Vec<Vec<Option<VId>>> {
    rows.iter()
        .map(|row| row.values().iter().map(|value| value.as_vertex()).collect())
        .collect()
}
fn clause(kind: usize, child: &GraphPatternBuilder) -> GraphMatchClause<'_> {
    match kind {
        0 => GraphMatchClause::required(child),
        1 => GraphMatchClause::optional(child),
        2 => GraphMatchClause::exists(child),
        _ => GraphMatchClause::not_exists(child),
    }
}
fn captured_null_pattern(matched: bool, count: Option<u64>) -> PreparedGraphPattern<GraphValueRow> {
    let root = builder(&["a"]);
    let mut optional = builder(&["a", "b"]);
    optional.edge("a", R, GlaDirection::Forward, "b").unwrap();
    let mut child = builder(&["c"]);
    if matched {
        child.vertex("b").unwrap();
    } else {
        child.outer_vertex("b").unwrap();
    }
    child
        .filter(
            "b",
            VertexPredicate::PropertyNull {
                key: P,
                is_null: true,
            },
        )
        .unwrap();
    root.prepare_values_with_clauses(
        &[
            GraphMatchClause::optional(&optional),
            GraphMatchClause::required(&child),
        ],
        &[GraphColumn::vertex("a", "a"), GraphColumn::vertex("c", "c")],
        0,
        count,
    )
    .unwrap()
    .with_duplicates()
}

#[test]
fn predicate_only_correlations_match_independent_relational_bags_in_every_scope_kind() {
    let vertices = [VId(0), VId(1), VId(2), VId(9)];
    let values = BTreeMap::from([
        (VId(0), CanonicalScalar::Int(0)),
        (VId(1), CanonicalScalar::Int(0)),
        (VId(2), CanonicalScalar::Null),
    ]);
    let universe: [Edge; 5] = [
        (VId(0), R, VId(1)),
        (VId(0), R, VId(1)),
        (VId(1), R, VId(2)),
        (VId(2), R, VId(0)),
        (VId(2), R, VId(2)),
    ];
    for mask in 0..32_usize {
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
            let mut oriented = Vec::new();
            for &(a, _, b) in &edges {
                match direction {
                    GlaDirection::Forward => oriented.push((a, b)),
                    GlaDirection::Reverse => oriented.push((b, a)),
                    GlaDirection::Undirected => {
                        oriented.push((a, b));
                        if a != b {
                            oriented.push((b, a));
                        }
                    }
                }
            }
            let root = builder(&["a"]);
            let mut optional = builder(&["a", "b"]);
            optional.edge("a", R, direction, "b").unwrap();
            // Capture declared FIRST must not become an independent child's root.
            let mut child = GraphPatternBuilder::new();
            child.outer_vertex("b").unwrap().vertex("c").unwrap();
            child
                .filter_boolean(
                    &GraphBooleanExpression::prepare(&[
                        Op::IsNull {
                            operand: Operand::Vertex("b"),
                            is_null: true,
                        },
                        Op::Compare {
                            left: Operand::Property {
                                variable: "c",
                                key: P,
                            },
                            comparison: IntegerComparison::Equal,
                            right: Operand::Property {
                                variable: "b",
                                key: P,
                            },
                        },
                        Op::Or,
                    ])
                    .unwrap(),
                )
                .unwrap();
            for kind in 0..4 {
                let mut expected = Vec::new();
                for a in vertices {
                    let mut outer_rows = oriented
                        .iter()
                        .filter(|(source, _)| *source == a)
                        .map(|(_, b)| Some(*b))
                        .collect::<Vec<_>>();
                    if outer_rows.is_empty() {
                        outer_rows.push(None);
                    }
                    for b in outer_rows {
                        let matches = vertices.iter().copied().filter(|c| b.is_none_or(|b| {
                            matches!((values.get(c), values.get(&b)),
                                (Some(CanonicalScalar::Int(x)), Some(CanonicalScalar::Int(y))) if x == y)
                        })).collect::<Vec<_>>();
                        match kind {
                            0 | 1 => {
                                if matches.is_empty() && kind == 1 {
                                    expected.push(vec![Some(a), b, None]);
                                }
                                for c in matches {
                                    expected.push(vec![Some(a), b, Some(c)]);
                                }
                            }
                            2 if !matches.is_empty() => expected.push(vec![Some(a), b]),
                            3 if matches.is_empty() => expected.push(vec![Some(a), b]),
                            _ => {}
                        }
                    }
                }
                expected.sort();
                let mut columns =
                    vec![GraphColumn::vertex("a", "a"), GraphColumn::vertex("b", "b")];
                if kind < 2 {
                    columns.push(GraphColumn::vertex("c", "c"));
                }
                for distinct in [false, true] {
                    let mut expected = expected.clone();
                    if distinct {
                        expected.dedup();
                    }
                    for (offset, count) in [(0, None), (1, Some(2)), (0, Some(0))] {
                        let pattern = root
                            .prepare_values_with_clauses(
                                &[GraphMatchClause::optional(&optional), clause(kind, &child)],
                                &columns,
                                offset,
                                count,
                            )
                            .unwrap();
                        let pattern = if distinct {
                            pattern
                        } else {
                            pattern.with_duplicates()
                        };
                        let result = pattern
                            .plan()
                            .execute_governed_with_properties(
                                (vertices.len() + edges.len()) as u64,
                                vertices,
                                edges.iter().copied(),
                                |_, _| Ok::<_, ()>(true),
                                |vid, _| Ok(values.get(&vid)),
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
                            cells(&result.value),
                            page,
                            "mask={mask}, direction={direction:?}, kind={kind}, distinct={distinct}"
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn captured_null_property_is_null_without_source_access_but_matched_null_refuses() {
    for matched in [false, true] {
        let pattern = captured_null_pattern(matched, None);
        let rows = pattern
            .plan()
            .execute_governed_with_properties(
                3,
                [VId(0), VId(1), VId(u128::MAX)],
                [],
                |_, _| Err::<bool, _>("null must not call the vertex source"),
                |_, _| Err::<Option<&CanonicalScalar>, _>("null must not call the property source"),
                wide(),
                || Ok::<_, ()>(()),
            )
            .unwrap();
        assert_eq!(rows.value.len(), if matched { 0 } else { 9 });
        assert_eq!(
            pattern
                .plan()
                .operators()
                .iter()
                .filter(|op| matches!(op, GlaOperator::BindOuterVertex { .. }))
                .count(),
            usize::from(!matched)
        );
    }
}

#[test]
fn captures_require_an_outer_scope_cannot_be_pattern_endpoints_and_obey_visibility() {
    let root = builder(&["a"]);
    let mut child = builder(&["c"]);
    child.outer_vertex("a").unwrap();
    assert_eq!(
        child
            .prepare_values(&[GraphColumn::vertex("c", "c")], 0, None)
            .unwrap_err(),
        PatternBuildError::OuterVertexRequiresScope
    );
    let before = root
        .prepare_values_with_clauses(
            &[GraphMatchClause::required(&child)],
            &[GraphColumn::vertex("c", "c")],
            0,
            None,
        )
        .unwrap();
    assert_eq!(
        child.edge("a", R, GlaDirection::Forward, "c").unwrap_err(),
        PatternBuildError::OuterVertexInPattern
    );
    assert_eq!(
        child.edge("c", R, GlaDirection::Reverse, "a").unwrap_err(),
        PatternBuildError::OuterVertexInPattern
    );
    assert_eq!(
        root.prepare_values_with_clauses(
            &[GraphMatchClause::required(&child)],
            &[GraphColumn::vertex("c", "c")],
            0,
            None
        )
        .unwrap(),
        before
    );
    assert_eq!(
        child.outer_vertex("a").unwrap_err(),
        PatternBuildError::DuplicateVariable
    );
    assert_eq!(
        child.outer_vertex("bad name").unwrap_err(),
        PatternBuildError::InvalidVariableName
    );
    let private = builder(&["secret"]);
    let mut late = builder(&["c"]);
    late.outer_vertex("secret").unwrap();
    assert_eq!(
        root.prepare_values_with_clauses(
            &[
                GraphMatchClause::exists(&private),
                GraphMatchClause::required(&late)
            ],
            &[GraphColumn::vertex("a", "a")],
            0,
            None
        )
        .unwrap_err(),
        PatternBuildError::UnknownOuterVertex
    );
    let mut only = GraphPatternBuilder::new();
    only.outer_vertex("a").unwrap();
    assert_eq!(
        root.prepare_values_with_clauses(
            &[GraphMatchClause::exists(&only)],
            &[GraphColumn::vertex("a", "a")],
            0,
            None
        )
        .unwrap_err(),
        PatternBuildError::EmptyPattern
    );
    let mut full = builder(&["local"]);
    for at in 1..MAX_PATTERN_VERTICES {
        full.outer_vertex(&format!("v{at}")).unwrap();
    }
    assert!(matches!(
        full.outer_vertex("overflow"),
        Err(PatternBuildError::LimitExceeded {
            dimension: PatternLimitDimension::Vertices,
            ..
        })
    ));
    let mut repeated = builder(&["a"]);
    for at in 1..MAX_PATTERN_VERTICES {
        repeated.vertex(&format!("v{at}")).unwrap();
    }
    let mut capture = builder(&["a"]);
    for at in 1..MAX_PATTERN_VERTICES {
        capture.outer_vertex(&format!("v{at}")).unwrap();
    }
    assert!(matches!(
        repeated.prepare_values_with_clauses(
            &[
                GraphMatchClause::exists(&capture),
                GraphMatchClause::exists(&capture)
            ],
            &[GraphColumn::vertex("a", "a")],
            0,
            None
        ),
        Err(PatternBuildError::LimitExceeded {
            dimension: PatternLimitDimension::Bindings,
            ..
        })
    ));
}

#[test]
fn reversed_positive_anchor_and_predicate_capture_keep_both_correlations_and_ties() {
    let mut root = builder(&["a", "b"]);
    root.edge("a", R, GlaDirection::Forward, "b").unwrap();
    let mut child = GraphPatternBuilder::new();
    child
        .outer_vertex("a")
        .unwrap()
        .vertex("d")
        .unwrap()
        .vertex("b")
        .unwrap();
    child.edge("d", S, GlaDirection::Reverse, "b").unwrap();
    child
        .compare_properties("d", P, IntegerComparison::Equal, "a", P)
        .unwrap();
    let pattern = root
        .prepare_values_with_clauses(
            &[GraphMatchClause::required(&child)],
            &[GraphColumn::vertex("a", "a"), GraphColumn::vertex("d", "d")],
            0,
            None,
        )
        .unwrap()
        .with_duplicates();
    let ten = CanonicalScalar::Int(10);
    let other = CanonicalScalar::Int(11);
    let rows = pattern
        .plan()
        .execute_governed_with_properties(
            9,
            [VId(1), VId(2), VId(3), VId(4)],
            [
                (VId(1), R, VId(2)),
                (VId(1), R, VId(2)),
                (VId(2), S, VId(3)),
                (VId(2), S, VId(3)),
                (VId(2), S, VId(4)),
            ],
            |_, _| Ok::<_, ()>(true),
            |vid, _| Ok(Some(if vid == VId(4) { &other } else { &ten })),
            wide(),
            || Ok::<_, ()>(()),
        )
        .unwrap();
    assert_eq!(
        cells(&rows.value),
        vec![vec![Some(VId(1)), Some(VId(3))]; 4]
    );
}

#[test]
fn captures_share_exact_source_result_work_and_scratch_allowances() {
    let pattern = captured_null_pattern(false, None);
    let run = |policy| {
        pattern.plan().execute_governed_with_properties(
            3,
            [VId(1), VId(2), VId(3)],
            [],
            |_, _| Ok::<_, ()>(true),
            |_, _| Ok(None),
            policy,
            || Ok::<_, ()>(()),
        )
    };
    let measured = run(wide()).unwrap();
    let limits = [
        measured.rows.snapshot_records,
        measured.rows.result_rows,
        measured.evaluator.work_units,
        measured.evaluator.scratch_entries,
    ];
    assert_eq!(
        run(GqlQueryPolicy::new(
            limits[0], limits[1], limits[2], limits[3]
        ))
        .unwrap(),
        measured
    );
    for dimension in 0..4 {
        let mut short = limits;
        short[dimension] -= 1;
        assert!(run(GqlQueryPolicy::new(short[0], short[1], short[2], short[3])).is_err());
    }
}

#[test]
fn every_capture_and_null_continuation_boundary_is_interruptible() {
    let pattern = captured_null_pattern(false, None);
    let mut calls = 0;
    pattern
        .plan()
        .execute_governed_with_properties(
            2,
            [VId(1), VId(2)],
            [],
            |_, _| Ok::<_, ()>(true),
            |_, _| Ok(None),
            wide(),
            || {
                calls += 1;
                Ok::<_, usize>(())
            },
        )
        .unwrap();
    for stop in 1..=calls {
        let mut seen = 0;
        let result = pattern.plan().execute_governed_with_properties(
            2,
            [VId(1), VId(2)],
            [],
            |_, _| Ok::<_, ()>(true),
            |_, _| Ok(None),
            wide(),
            || {
                seen += 1;
                if seen == stop { Err(stop) } else { Ok(()) }
            },
        );
        assert!(matches!(result, Err(GqlQueryError::Interrupted(at)) if at == stop));
        assert_eq!(seen, stop);
    }
}

#[test]
fn eager_outer_property_errors_are_not_null_extension_or_not_exists_success_even_at_limit_zero() {
    let root = builder(&["a"]);
    let mut child = builder(&["c"]);
    child.outer_vertex("a").unwrap();
    child
        .filter_boolean(
            &GraphBooleanExpression::prepare(&[
                Op::Truth(Some(true)),
                Op::IsNull {
                    operand: Operand::Property {
                        variable: "a",
                        key: P,
                    },
                    is_null: true,
                },
                Op::Or,
            ])
            .unwrap(),
        )
        .unwrap();
    for kind in 0..4 {
        let pattern = root
            .prepare_values_with_clauses(
                &[clause(kind, &child)],
                &[GraphColumn::vertex("a", "a")],
                0,
                Some(0),
            )
            .unwrap();
        let result = pattern.plan().execute_governed_with_properties(
            2,
            [VId(1), VId(2)],
            [],
            |_, _| Ok::<_, &str>(true),
            |_, _| Err("outer property failure"),
            wide(),
            || Ok::<_, ()>(()),
        );
        assert!(matches!(
            result,
            Err(GqlQueryError::Source("outer property failure"))
        ));
    }
}

#[test]
fn captures_have_distinct_transcripts_while_ordinary_required_scans_keep_frozen_bytes() {
    let make = |capture: bool| {
        let root = builder(&["a"]);
        let mut child = builder(&["c"]);
        if capture {
            child.outer_vertex("a").unwrap();
        }
        root.prepare_values_with_clauses(
            &[GraphMatchClause::required(&child)],
            &[GraphColumn::vertex("a", "a"), GraphColumn::vertex("c", "c")],
            0,
            None,
        )
        .unwrap()
    };
    for captured in [false, true] {
        let mut expected = b"fgdb:bounded-gla:v1\0".to_vec();
        expected.extend_from_slice(&(if captured { 7_u64 } else { 6_u64 }).to_be_bytes());
        expected.extend_from_slice(&[1, 1]);
        if captured {
            expected.push(25);
            expected.extend_from_slice(&0_u32.to_be_bytes());
        }
        expected.push(12);
        expected.extend_from_slice(&2_u64.to_be_bytes());
        for slot in [0_u32, 1] {
            expected.push(0);
            expected.extend_from_slice(&slot.to_be_bytes());
        }
        expected.extend_from_slice(&[7, 13, 9]);
        expected.extend_from_slice(&0_u64.to_be_bytes());
        expected.push(0);
        assert_eq!(make(captured).canonical_bytes(), expected);
    }
    assert_ne!(make(false).canonical_bytes(), make(true).canonical_bytes());
}
