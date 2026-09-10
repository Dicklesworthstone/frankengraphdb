//! Independent complete-assignment expectations for correlated left joins.
//! The oracle counts physical edge occurrences, then performs explicit null
//! extension. It does not use compiler slots, the visitor, or projected bags.

use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::{
    GlaDirection, GraphColumn, GraphExistence, GraphMatchClause, GraphPatternBuilder,
    GraphValueRow, IntegerComparison, MAX_PATTERN_EDGES, PatternBuildError, PreparedGraphPattern,
    ValueProjection, VertexPredicate,
};
use fgdb_gql::{GqlQueryError, GqlQueryPolicy, GraphAggregate, PreparedGraphAggregate};
use fgdb_types::{CanonicalScalar, VId};

type Edge = (VId, RelationId, VId);
type Row = Vec<Option<VId>>;
const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const T: RelationId = RelationId(3);
const P: PropertyKeyId = PropertyKeyId(1);

fn wide() -> GqlQueryPolicy {
    GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX)
}
fn builder(
    names: &[&str],
    edges: &[(&str, RelationId, GlaDirection, &str)],
) -> GraphPatternBuilder {
    let mut result = GraphPatternBuilder::new();
    for name in names {
        result.vertex(name).unwrap();
    }
    for &(source, relation, direction, target) in edges {
        result.edge(source, relation, direction, target).unwrap();
    }
    result
}
fn accepts(vid: VId, predicates: &[VertexPredicate]) -> bool {
    predicates.iter().all(|predicate| {
        predicate.matches(&[], &[(P, CanonicalScalar::Int(i64::from(vid == VId(1))))])
    })
}
fn plain(rows: &[GraphValueRow]) -> Vec<Row> {
    rows.iter()
        .map(|row| {
            row.values()
                .iter()
                .map(|value| {
                    assert!(value.is_null() || value.as_vertex().is_some());
                    value.as_vertex()
                })
                .collect()
        })
        .collect()
}
fn execute(
    pattern: &PreparedGraphPattern<GraphValueRow>,
    vertices: &[VId],
    edges: &[Edge],
) -> Vec<Row> {
    plain(
        &pattern
            .plan()
            .execute_governed_with_properties(
                (vertices.len() + edges.len()) as u64,
                vertices.iter().copied(),
                edges.iter().copied(),
                |vid, predicates| Ok::<_, ()>(accepts(vid, predicates)),
                |_, _| Ok(None),
                wide(),
                || Ok::<_, ()>(()),
            )
            .unwrap()
            .value,
    )
}
fn occurrences(
    edges: &[Edge],
    source: VId,
    relation: RelationId,
    direction: GlaDirection,
    target: VId,
) -> usize {
    edges
        .iter()
        .filter(|&&(s, r, t)| {
            r == relation
                && match direction {
                    GlaDirection::Forward => s == source && t == target,
                    GlaDirection::Reverse => t == source && s == target,
                    GlaDirection::Undirected => {
                        (s == source && t == target) || (s == target && t == source)
                    }
                }
        })
        .count()
}
fn oracle(
    vertices: &[VId],
    edges: &[Edge],
    d1: GlaDirection,
    d2: GlaDirection,
    split: bool,
) -> Vec<Row> {
    let mut rows = Vec::new();
    for &owner in vertices {
        let before = rows.len();
        for &via in vertices {
            let n1 = occurrences(edges, owner, R, d1, via);
            for _ in 0..n1 {
                let matched = rows.len();
                // The independent predicate is c == VId(1), as encoded by the
                // fixture's property values, not by the production predicate.
                for &end in vertices.iter().filter(|&&vid| vid == VId(1)) {
                    for _ in 0..occurrences(edges, via, S, d2, end) {
                        rows.push(vec![Some(owner), Some(via), Some(end)]);
                    }
                }
                if split && rows.len() == matched {
                    rows.push(vec![Some(owner), Some(via), None]);
                }
            }
        }
        if rows.len() == before {
            rows.push(vec![Some(owner), None, None]);
        }
    }
    rows.sort();
    rows
}

#[test]
fn whole_and_chained_optional_patterns_match_independent_multigraph_assignments() {
    let ids = [VId(0), VId(1), VId(u128::MAX)];
    let root = builder(&["p"], &[]);
    let columns = [
        GraphColumn::vertex("owner", "p"),
        GraphColumn::vertex("via", "b"),
        GraphColumn::vertex("end", "c"),
    ];
    let mut cases = Vec::new();
    for d1 in [
        GlaDirection::Forward,
        GlaDirection::Reverse,
        GlaDirection::Undirected,
    ] {
        for d2 in [
            GlaDirection::Forward,
            GlaDirection::Reverse,
            GlaDirection::Undirected,
        ] {
            let mut whole = builder(&["p", "b", "c"], &[("p", R, d1, "b"), ("b", S, d2, "c")]);
            let first = builder(&["p", "b"], &[("p", R, d1, "b")]);
            let mut second = builder(&["b", "c"], &[("b", S, d2, "c")]);
            let predicate = VertexPredicate::IntegerProperty {
                key: P,
                comparison: IntegerComparison::Equal,
                value: 1,
            };
            whole.filter("c", predicate.clone()).unwrap();
            second.filter("c", predicate).unwrap();
            let one = root
                .prepare_values_with_clauses(
                    &[GraphMatchClause::optional(&whole)],
                    &columns,
                    0,
                    None,
                )
                .unwrap();
            let two = root
                .prepare_values_with_clauses(
                    &[
                        GraphMatchClause::optional(&first),
                        GraphMatchClause::optional(&second),
                    ],
                    &columns,
                    0,
                    None,
                )
                .unwrap();
            cases.push((d1, d2, false, one));
            cases.push((d1, d2, true, two));
        }
    }
    let universe = [
        (ids[0], R, ids[1]),
        (ids[1], R, ids[1]),
        (ids[2], R, ids[0]),
        (ids[1], S, ids[2]),
        (ids[1], S, ids[0]),
        (ids[0], S, ids[1]),
    ];
    for mut encoded in 0..3_usize.pow(universe.len() as u32) {
        let mut edges = Vec::new();
        for edge in universe {
            for _ in 0..encoded % 3 {
                edges.push(edge);
            }
            encoded /= 3;
        }
        for (d1, d2, split, distinct) in &cases {
            let expected = oracle(&ids, &edges, *d1, *d2, *split);
            assert_eq!(
                execute(&distinct.clone().with_duplicates(), &ids, &edges),
                expected
            );
            let mut unique = expected;
            unique.dedup();
            assert_eq!(execute(distinct, &ids, &edges), unique);
        }
    }
}

#[test]
fn a_later_rejection_does_not_reclassify_a_complete_optional_witness_as_absence() {
    let root = builder(&["p"], &[]);
    let optional = builder(&["p", "b"], &[("p", R, GlaDirection::Forward, "b")]);
    let bound_b = builder(&["b"], &[]);
    let columns = [GraphColumn::vertex("p", "p"), GraphColumn::vertex("b", "b")];
    let absent = root
        .prepare_values_with_clauses(
            &[
                GraphMatchClause::optional(&optional),
                GraphMatchClause::not_exists(&bound_b),
            ],
            &columns,
            0,
            None,
        )
        .unwrap()
        .with_duplicates();
    let present = root
        .prepare_values_with_clauses(
            &[
                GraphMatchClause::optional(&optional),
                GraphMatchClause::exists(&bound_b),
            ],
            &columns,
            0,
            None,
        )
        .unwrap()
        .with_duplicates();
    let edges = [(VId(0), R, VId(1)), (VId(0), R, VId(1))];
    assert_eq!(
        execute(&absent, &[VId(0), VId(2)], &edges),
        vec![vec![Some(VId(2)), None]]
    );
    assert_eq!(
        execute(&present, &[VId(0), VId(2)], &edges),
        vec![vec![Some(VId(0)), Some(VId(1))]; 2]
    );
    let mut no_extra_variables = builder(&["p"], &[]);
    no_extra_variables
        .filter("p", VertexPredicate::HasLabel(LabelId(9)))
        .unwrap();
    let unchanged = root
        .prepare_values_with_clauses(
            &[GraphMatchClause::optional(&no_extra_variables)],
            &[columns[0]],
            0,
            None,
        )
        .unwrap();
    assert_eq!(
        execute(&unchanged, &[VId(0), VId(2)], &[]),
        vec![vec![Some(VId(0))], vec![Some(VId(2))]]
    );
}

#[test]
fn nullable_chains_do_not_prevent_independent_later_branches_from_matching() {
    let root = builder(&["p"], &[]);
    let first = builder(&["p", "b"], &[("p", R, GlaDirection::Forward, "b")]);
    let second = builder(&["b", "c"], &[("b", S, GlaDirection::Forward, "c")]);
    let third = builder(&["p", "d"], &[("p", T, GlaDirection::Forward, "d")]);
    let pattern = root
        .prepare_values_with_clauses(
            &[
                GraphMatchClause::optional(&first),
                GraphMatchClause::optional(&second),
                GraphMatchClause::optional(&third),
            ],
            &[
                GraphColumn::vertex("p", "p"),
                GraphColumn::vertex("b", "b"),
                GraphColumn::vertex("c", "c"),
                GraphColumn::vertex("d", "d"),
            ],
            0,
            None,
        )
        .unwrap()
        .with_duplicates();
    let edges = [
        (VId(0), R, VId(10)),
        (VId(2), R, VId(11)),
        (VId(10), S, VId(20)),
        (VId(1), T, VId(30)),
    ];
    assert_eq!(
        execute(&pattern, &[VId(0), VId(1), VId(2)], &edges),
        vec![
            vec![Some(VId(0)), Some(VId(10)), Some(VId(20)), None],
            vec![Some(VId(1)), None, None, Some(VId(30))],
            vec![Some(VId(2)), Some(VId(11)), None, None],
        ]
    );
}

#[test]
fn multiple_outer_correlations_and_reverse_anchor_selection_keep_complete_matches() {
    let root = builder(&["p", "q"], &[("p", R, GlaDirection::Forward, "q")]);
    let inner = builder(
        &["m", "q", "p"],
        &[
            ("m", S, GlaDirection::Forward, "q"),
            ("m", T, GlaDirection::Forward, "p"),
        ],
    );
    let pattern = root
        .prepare_values_with_clauses(
            &[GraphMatchClause::optional(&inner)],
            &[
                GraphColumn::vertex("p", "p"),
                GraphColumn::vertex("q", "q"),
                GraphColumn::vertex("m", "m"),
            ],
            0,
            None,
        )
        .unwrap()
        .with_duplicates();
    let edges = [
        (VId(0), R, VId(1)),
        (VId(0), R, VId(1)),
        (VId(2), R, VId(1)),
        (VId(3), S, VId(1)),
        (VId(3), T, VId(0)),
        (VId(4), S, VId(1)),
    ];
    let ids = [VId(0), VId(1), VId(2), VId(3), VId(4)];
    let mut expected = Vec::new();
    for &(p, r, q) in &edges {
        if r != R {
            continue;
        }
        let before = expected.len();
        for &m in &ids {
            let copies = occurrences(&edges, m, S, GlaDirection::Forward, q)
                * occurrences(&edges, m, T, GlaDirection::Forward, p);
            for _ in 0..copies {
                expected.push(vec![Some(p), Some(q), Some(m)]);
            }
        }
        if expected.len() == before {
            expected.push(vec![Some(p), Some(q), None]);
        }
    }
    expected.sort();
    assert_eq!(execute(&pattern, &[], &edges), expected);
}

#[test]
fn definition_scopes_and_logical_identity_do_not_leak_existential_names() {
    let root = builder(&["p"], &[]);
    let inner = builder(
        &["p", "private_local"],
        &[("p", R, GlaDirection::Forward, "private_local")],
    );
    let column = GraphColumn::vertex("p", "p");
    let old = root
        .prepare_values_with_existence(&[GraphExistence::exists(&inner)], &[column], 0, None)
        .unwrap();
    let same = root
        .prepare_values_with_clauses(&[GraphMatchClause::exists(&inner)], &[column], 0, None)
        .unwrap();
    assert_eq!(old, same);
    assert_eq!(
        root.prepare_values_with_clauses(&[], &[column], 0, None)
            .unwrap(),
        root.prepare_values(&[column], 0, None).unwrap()
    );
    assert_eq!(
        root.prepare_values_with_clauses(
            &[GraphMatchClause::exists(&inner)],
            &[GraphColumn::vertex("leak", "private_local")],
            0,
            None
        )
        .unwrap_err(),
        PatternBuildError::UnknownVariable
    );
    let disconnected = builder(
        &["private_local", "other"],
        &[("private_local", S, GlaDirection::Forward, "other")],
    );
    assert_eq!(
        root.prepare_values_with_clauses(
            &[
                GraphMatchClause::exists(&inner),
                GraphMatchClause::optional(&disconnected)
            ],
            &[column],
            0,
            None
        )
        .unwrap_err(),
        PatternBuildError::Disconnected
    );
    let optional = root
        .prepare_values_with_clauses(
            &[GraphMatchClause::optional(&inner)],
            &[column, GraphColumn::vertex("value", "private_local")],
            0,
            None,
        )
        .unwrap();
    let renamed = root
        .prepare_values_with_clauses(
            &[GraphMatchClause::optional(&inner)],
            &[
                GraphColumn::vertex("renamed", "p"),
                GraphColumn::vertex("alias", "private_local"),
            ],
            0,
            None,
        )
        .unwrap();
    assert_eq!(optional.canonical_bytes(), renamed.canonical_bytes());
    assert_ne!(
        same.canonical_bytes(),
        root.prepare_values_with_clauses(&[GraphMatchClause::optional(&inner)], &[column], 0, None)
            .unwrap()
            .canonical_bytes()
    );
    assert!(!format!("{:?}", GraphMatchClause::optional(&inner)).contains("private_local"));
    assert!(!format!("{optional:?}").contains("private_local"));
}

#[test]
fn maximum_optional_chain_keeps_all_65_columns_without_sentinel_or_slot_aliasing() {
    let names: Vec<_> = (0..=MAX_PATTERN_EDGES).map(|at| format!("n{at}")).collect();
    let root = builder(&[names[0].as_str()], &[]);
    let inners: Vec<_> = names
        .windows(2)
        .map(|pair| {
            builder(
                &[pair[0].as_str(), pair[1].as_str()],
                &[(pair[0].as_str(), R, GlaDirection::Forward, pair[1].as_str())],
            )
        })
        .collect();
    let clauses: Vec<_> = inners.iter().map(GraphMatchClause::optional).collect();
    let columns: Vec<_> = names
        .iter()
        .map(|name| GraphColumn::vertex(name, name))
        .collect();
    let pattern = root
        .prepare_values_with_clauses(&clauses, &columns, 0, None)
        .unwrap();
    assert!(
        matches!(pattern.value_columns().last(), Some(ValueProjection::Vertex { slot }) if slot.ordinal() == 128)
    );
    let edges: Vec<_> = (0..64).map(|at| (VId(at), R, VId(at + 1))).collect();
    assert_eq!(
        execute(&pattern, &[VId(0)], &edges),
        vec![(0..=64).map(|at| Some(VId(at))).collect::<Row>()]
    );
    let mut absent = vec![None; 65];
    absent[0] = Some(VId(0));
    assert_eq!(execute(&pattern, &[VId(0)], &[]), vec![absent]);
    assert!(matches!(
        root.prepare_values_with_clauses(
            &[GraphMatchClause::optional(&inners[0]); 65],
            &columns,
            0,
            None
        ),
        Err(PatternBuildError::LimitExceeded {
            limit: 64,
            observed: 65,
            ..
        })
    ));
}

#[test]
fn optional_null_rows_share_every_budget_and_interruption_checkpoint() {
    let root = builder(&["p"], &[]);
    let child = builder(
        &["p", "b", "c"],
        &[
            ("p", R, GlaDirection::Forward, "b"),
            ("b", S, GlaDirection::Forward, "c"),
        ],
    );
    let pattern = root
        .prepare_values_with_clauses(
            &[GraphMatchClause::optional(&child)],
            &[
                GraphColumn::vertex("p", "p"),
                GraphColumn::vertex("b", "b"),
                GraphColumn::vertex("c", "c"),
            ],
            0,
            None,
        )
        .unwrap()
        .with_duplicates();
    let vertices = [VId(0), VId(1), VId(2)];
    let edges = [(VId(0), R, VId(1)), (VId(1), S, VId(1))];
    let run = |policy| {
        pattern.plan().execute_governed_with_properties(
            5,
            vertices,
            edges,
            |_, _| Ok::<_, ()>(true),
            |_, _| Ok(None),
            policy,
            || Ok::<_, usize>(()),
        )
    };
    let full = run(wide()).unwrap();
    assert_eq!(full.value.len(), 3);
    let exact = GqlQueryPolicy::new(
        5,
        3,
        full.evaluator.work_units,
        full.evaluator.scratch_entries,
    );
    assert_eq!(run(exact).unwrap(), full);
    for policy in [
        GqlQueryPolicy::new(5, 3, full.evaluator.work_units - 1, u64::MAX),
        GqlQueryPolicy::new(5, 3, u64::MAX, full.evaluator.scratch_entries - 1),
    ] {
        assert!(matches!(run(policy), Err(GqlQueryError::Evaluator(_))));
    }
    assert!(
        matches!(run(GqlQueryPolicy::new(5, 0, u64::MAX, u64::MAX)), Err(GqlQueryError::Rows(error)) if error.observed == 1)
    );
    let mut total = 0;
    pattern
        .plan()
        .execute_governed_with_properties(
            5,
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
            5,
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
    assert_eq!(run(exact).unwrap(), full);
}

#[test]
fn late_predicate_and_property_errors_never_become_successful_null_extensions() {
    let root = builder(&["p"], &[]);
    let mut child = builder(&["p", "b"], &[("p", R, GlaDirection::Forward, "b")]);
    child
        .filter(
            "b",
            VertexPredicate::IntegerProperty {
                key: P,
                comparison: IntegerComparison::GreaterOrEqual,
                value: 0,
            },
        )
        .unwrap();
    let pattern = root
        .prepare_values_with_clauses(
            &[GraphMatchClause::optional(&child)],
            &[
                GraphColumn::vertex("p", "p"),
                GraphColumn::property("value", "b", P),
            ],
            0,
            None,
        )
        .unwrap()
        .with_duplicates();
    let edges = [(VId(0), R, VId(1)), (VId(0), R, VId(2))];
    let scalar = CanonicalScalar::Int(7);
    let predicate_failure = pattern.plan().execute_governed_with_properties(
        3,
        [VId(0)],
        edges,
        |vid, _| {
            if vid == VId(2) {
                Err("predicate failed after an earlier match")
            } else {
                Ok(true)
            }
        },
        |_, _| Ok(Some(&scalar)),
        GqlQueryPolicy::new(3, 0, u64::MAX, u64::MAX),
        || Ok::<_, ()>(()),
    );
    assert!(matches!(
        predicate_failure,
        Err(GqlQueryError::Source(
            "predicate failed after an earlier match"
        ))
    ));
    let property_failure = pattern.plan().execute_governed_with_properties(
        3,
        [VId(0)],
        edges,
        |_, _| Ok(true),
        |vid, _| {
            if vid == VId(2) {
                Err("property failed after an earlier match")
            } else {
                Ok(Some(&scalar))
            }
        },
        wide(),
        || Ok::<_, ()>(()),
    );
    assert!(matches!(
        property_failure,
        Err(GqlQueryError::Source(
            "property failed after an earlier match"
        ))
    ));
}

#[test]
fn streaming_counts_distinguish_outer_rows_from_nullable_vertices_and_properties() {
    let root = builder(&["p"], &[]);
    let child = builder(&["p", "b"], &[("p", R, GlaDirection::Forward, "b")]);
    let pattern = root
        .prepare_values_with_clauses(
            &[GraphMatchClause::optional(&child)],
            &[
                GraphColumn::vertex("owner", "p"),
                GraphColumn::vertex("optional", "b"),
                GraphColumn::property("amount", "b", P),
            ],
            0,
            None,
        )
        .unwrap()
        .with_duplicates();
    let summary = PreparedGraphAggregate::prepare(
        pattern.clone(),
        &[0],
        &[
            GraphAggregate::count_rows("rows"),
            GraphAggregate::count("present", 1),
            GraphAggregate::count_distinct("unique", 1),
            GraphAggregate::sum_int("sum", 2),
        ],
        0,
        None,
    )
    .unwrap();
    let scalar = CanonicalScalar::Int(7);
    let edges = [(VId(0), R, VId(2)); 2];
    let result = summary
        .execute_governed(
            4,
            [VId(0), VId(1)],
            edges,
            |_, _| Ok::<_, ()>(true),
            |vid, _| {
                assert_eq!(vid, VId(2));
                Ok(Some(&scalar))
            },
            wide(),
            || Ok::<_, ()>(()),
        )
        .unwrap();
    assert_eq!(result.value.len(), 2);
    assert_eq!(result.value[0].get(0).unwrap().as_count(), Some(2));
    assert_eq!(result.value[0].get(1).unwrap().as_count(), Some(2));
    assert_eq!(result.value[0].get(2).unwrap().as_count(), Some(1));
    assert_eq!(result.value[0].get(3).unwrap().as_integer(), Some(14));
    assert_eq!(result.value[1].get(0).unwrap().as_count(), Some(1));
    assert_eq!(result.value[1].get(1).unwrap().as_count(), Some(0));
    assert_eq!(result.value[1].get(2).unwrap().as_count(), Some(0));
    assert!(result.value[1].get(3).unwrap().is_null());
    assert!(execute(&pattern, &[], &[]).is_empty());
    let count = PreparedGraphAggregate::prepare(
        pattern,
        &[],
        &[GraphAggregate::count_rows("rows")],
        0,
        None,
    )
    .unwrap();
    let empty = count
        .execute_governed(
            0,
            [],
            [],
            |_, _| Ok::<_, ()>(true),
            |_, _| Ok(None),
            wide(),
            || Ok::<_, ()>(()),
        )
        .unwrap();
    assert_eq!(empty.value[0].get(0).unwrap().as_count(), Some(0));
}
