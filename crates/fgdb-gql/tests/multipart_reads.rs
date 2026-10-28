//! Native multi-part reads execute real GLA leaves through one governed source.

use fgdb_delta_types::{ElementId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GraphValue, GraphValueRow};
use fgdb_gql::{
    GqlParameterType, GqlParameters, GqlQueryError, GqlQueryExecution, GqlQueryPolicy,
    GraphSetExecutionError, GraphSymbol, GraphSymbolKind, PreparedGraphSet, PreparedGraphSetText,
};
use fgdb_types::{CanonicalPropertyValue, CanonicalScalar, VId};
use std::cell::RefCell;
use std::collections::BTreeSet;

const P: PropertyKeyId = PropertyKeyId(1);
const R: RelationId = RelationId(1);
const EDGES: [(VId, RelationId, VId); 5] = [
    (VId(1), R, VId(2)),
    (VId(1), R, VId(2)),
    (VId(1), R, VId(3)),
    (VId(2), R, VId(3)),
    (VId(3), R, VId(4)),
];
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        _ => None,
    }
}
fn wide() -> GqlQueryPolicy {
    GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX)
}
fn prepare(text: &str) -> PreparedGraphSet {
    PreparedGraphSetText::prepare(text, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}
/// A template whose `$items` binds a list. `WITH $items AS xs` is no use
/// that types it, and an undeclared parameter defaults to Int64 there, so
/// these laws declare it, as the native engine declares every argument.
fn prepare_items(text: &str) -> PreparedGraphSetText {
    PreparedGraphSetText::prepare_with_parameter_types(
        text,
        &[("items", GqlParameterType::List)],
        symbols,
    )
    .unwrap()
}
fn run(
    query: &PreparedGraphSet,
    policy: GqlQueryPolicy,
    checkpoint: &mut impl FnMut() -> Result<(), usize>,
) -> Result<
    GqlQueryExecution<GraphValueRow>,
    GqlQueryError<GraphSetExecutionError<&'static str>, usize>,
> {
    run_reading(query, policy, checkpoint, false)
}
fn run_reading(
    query: &PreparedGraphSet,
    policy: GqlQueryPolicy,
    checkpoint: &mut impl FnMut() -> Result<(), usize>,
    fail_elements: bool,
) -> Result<
    GqlQueryExecution<GraphValueRow>,
    GqlQueryError<GraphSetExecutionError<&'static str>, usize>,
> {
    let values = [1, 2, 2, 3].map(|n| CanonicalPropertyValue::Scalar(CanonicalScalar::Int(n)));
    let checkpoint = RefCell::new(checkpoint);
    query.execute_governed_with_elements(
        policy,
        |pattern, remaining| {
            pattern.plan().execute_governed_with_properties(
                (values.len() + EDGES.len()) as u64,
                (1..=4).map(VId),
                EDGES,
                |vid, tests| {
                    Ok::<_, &'static str>(
                        tests.iter().all(|test| {
                            test.matches(&[], &[(P, values[vid.0 as usize - 1].clone())])
                        }),
                    )
                },
                |vid, key| Ok((key == P).then(|| &values[vid.0 as usize - 1])),
                remaining,
                || (checkpoint.borrow_mut())(),
            )
        },
        |_, _, _| panic!("this fixture has no CALL source"),
        |element, key| {
            if fail_elements {
                return Err("element-read-error");
            }
            Ok(match element {
                ElementId::Vertex(vid) if key == P => Some(values[vid.0 as usize - 1].clone()),
                _ => None,
            })
        },
        || (checkpoint.borrow_mut())(),
    )
}
fn execute(text: &str) -> Vec<GraphValueRow> {
    run(&prepare(text), wide(), &mut || Ok(())).unwrap().value
}
/// `execute` with each edge occurrence's identity, for a DIFFERENT EDGES plan
/// over more than one overlapping fixed atom.
fn execute_identified(text: &str) -> Vec<GraphValueRow> {
    run_identified(&prepare(text))
}
/// `run_identified` over a bound query, as `run` is to `execute`.
fn run_identified(query: &PreparedGraphSet) -> Vec<GraphValueRow> {
    let values = [1, 2, 2, 3].map(|n| CanonicalPropertyValue::Scalar(CanonicalScalar::Int(n)));
    query
        .execute_governed_with_elements(
            wide(),
            |pattern, remaining| {
                pattern.plan().execute_governed_with_identified_properties(
                    (values.len() + EDGES.len()) as u64,
                    (1..=4).map(VId),
                    EDGES
                        .iter()
                        .enumerate()
                        .map(|(at, &(source, relation, target))| {
                            (fgdb_types::EId(at as u128 + 1), source, relation, target)
                        }),
                    |vid, tests| {
                        Ok::<_, &'static str>(tests.iter().all(|test| {
                            test.matches(&[], &[(P, values[vid.0 as usize - 1].clone())])
                        }))
                    },
                    |vid, key| Ok((key == P).then(|| &values[vid.0 as usize - 1])),
                    remaining,
                    || Ok::<_, usize>(()),
                )
            },
            |_, _, _| panic!("this fixture has no CALL source"),
            |element, key| {
                Ok(match element {
                    ElementId::Vertex(vid) if key == P => Some(values[vid.0 as usize - 1].clone()),
                    _ => None,
                })
            },
            || Ok::<_, usize>(()),
        )
        .unwrap()
        .value
}
fn vertices(values: &[u128]) -> GraphValueRow {
    GraphValueRow::from_owned_values(
        values
            .iter()
            .map(|id| GraphValue::Vertex(VId(*id)))
            .collect(),
    )
}
fn scalar_vertex(value: i64, vid: u128) -> GraphValueRow {
    GraphValueRow::from_owned_values(vec![
        GraphValue::Scalar(CanonicalScalar::Int(value)),
        GraphValue::Vertex(VId(vid)),
    ])
}

#[test]
fn inline_call_imports_preserve_bags_nulls_and_local_pages() {
    let row = |cells: &[Option<i64>]| {
        GraphValueRow::from_owned_values(
            cells
                .iter()
                .map(|value| {
                    GraphValue::Scalar(value.map_or(CanonicalScalar::Null, CanonicalScalar::Int))
                })
                .collect(),
        )
    };
    assert_eq!(
        execute("CALL { RETURN 1 AS a } RETURN a"),
        vec![row(&[Some(1)])]
    );
    assert_eq!(
        execute("WITH 2 AS x,3 AS z CALL (z,x) { RETURN z-x AS y } RETURN x,z,y"),
        vec![row(&[Some(2), Some(3), Some(1)])]
    );
    assert_eq!(
        execute("WITH 2 AS x CALL { WITH x RETURN x+1 AS y } RETURN x,y"),
        vec![row(&[Some(2), Some(3)])]
    );
    let expected: Vec<_> = [Some(2), Some(2), None]
        .into_iter()
        .map(|value| row(&[value, value]))
        .collect();
    assert_eq!(
        execute("UNWIND [2,2,null] AS x CALL (x) { RETURN x AS y } RETURN x,y"),
        expected
    );
    assert_eq!(
        execute(
            "WITH 2 AS x CALL (x) { UNWIND [x,x+1] AS y RETURN y ORDER BY y DESC LIMIT 1 } RETURN x,y"
        ),
        vec![row(&[Some(2), Some(3)])]
    );
    assert!(execute("WITH 1 AS x CALL (x) { UNWIND [] AS y RETURN y } RETURN x,y").is_empty());
    assert_eq!(
        execute("WITH 3 AS x CALL (x) { CALL (x) { RETURN x*2 AS z } RETURN z+1 AS y } RETURN x,y"),
        vec![row(&[Some(3), Some(7)])]
    );
}

#[test]
fn inline_call_correlates_actual_graph_sources_before_export() {
    let expected: Vec<_> = EDGES
        .iter()
        .map(|(left, _, right)| vertices(&[left.0, right.0]))
        .collect();
    assert_eq!(
        execute("MATCH (n) CALL (n) { MATCH (n)-[:R]->(m) RETURN m } RETURN n,m ORDER BY n,m"),
        expected
    );
    let expected: Vec<_> = (1..=4)
        .map(|id| scalar_vertex([1, 2, 2, 3][id - 1], id as u128))
        .collect();
    assert_eq!(
        execute("MATCH (n) CALL (n) { RETURN n.p AS p } RETURN p,n ORDER BY n"),
        expected
    );
    assert_eq!(
        execute("CALL { MATCH (n) RETURN n } RETURN n.p AS p,n ORDER BY n"),
        expected
    );
    assert_eq!(
        execute("MATCH (n) CALL (n) { RETURN n.p+1 AS p } RETURN p,n ORDER BY n"),
        (1..=4)
            .map(|id| scalar_vertex([2, 3, 3, 4][id - 1], id as u128))
            .collect::<Vec<_>>()
    );
    let query = prepare("MATCH (n) CALL (n) { RETURN n.p AS p } RETURN p");
    assert!(run_reading(&query, wide(), &mut || Ok(()), true).is_err());
}

#[test]
fn inline_call_bodies_aggregate_once_per_outer_row() {
    // Each vertex's out-degree and the sum of its neighbours' p, from the
    // fixture itself. The vertex with no edge still gets its row: a global
    // aggregate over no rows is one row (count 0).
    let p = |id: u128| [1, 2, 2, 3][id as usize - 1];
    let out = |id: u128| {
        EDGES
            .iter()
            .filter(move |(source, _, _)| *source == VId(id))
    };
    let degree = |id: u128| out(id).count() as i64;
    let sum = |id: u128| out(id).map(|(_, _, target)| p(target.0)).sum::<i64>();
    let rows = |value: &dyn Fn(u128) -> i64| -> Vec<GraphValueRow> {
        (1..=4)
            .map(|id| {
                GraphValueRow::from_owned_values(vec![
                    GraphValue::Vertex(VId(id)),
                    GraphValue::Scalar(CanonicalScalar::Int(value(id))),
                ])
            })
            .collect()
    };
    assert_eq!(degree(4), 0, "the fixture keeps a vertex with no edge");
    for text in [
        "MATCH (n) CALL (n) { MATCH (n)-[:R]->(m) RETURN count(m) AS out } \
         RETURN n, out ORDER BY n",
        "MATCH (n) CALL { WITH n MATCH (n)-[:R]->(m) RETURN count(*) AS out } \
         RETURN n, out ORDER BY n",
        "MATCH (n) CALL (n) { OPTIONAL MATCH (n)-[:R]->(m) RETURN count(m) AS out } \
         RETURN n, out ORDER BY n",
        "MATCH (n) CALL (n) { MATCH (n)-[:R]->(m) WITH m WHERE m.p > 0 \
         RETURN count(*) AS out } RETURN n, out ORDER BY n",
    ] {
        let query = PreparedGraphSetText::prepare(text, symbols)
            .unwrap_or_else(|error| panic!("{text}: {error:?}"))
            .bind_parameters(&GqlParameters::new())
            .unwrap();
        let result = run(&query, wide(), &mut || Ok(())).unwrap();
        assert_eq!(result.value, rows(&degree), "{text}");
    }
    // sum over a body with no rows is NULL, as at the top level
    // (`UNWIND [] AS x RETURN sum(x)`).
    let sums: Vec<_> = (1..=4)
        .map(|id| {
            let value = if degree(id) == 0 {
                CanonicalScalar::Null
            } else {
                CanonicalScalar::Int(sum(id))
            };
            GraphValueRow::from_owned_values(vec![
                GraphValue::Vertex(VId(id)),
                GraphValue::Scalar(value),
            ])
        })
        .collect();
    assert_eq!(
        execute(
            "MATCH (n) CALL (n) { MATCH (n)-[:R]->(m) RETURN sum(m.p) AS s } \
             RETURN n, s ORDER BY n",
        ),
        sums
    );
    // A grouped body yields one row per group, and none for a vertex with
    // no edge: CALL then drops that outer row, as a subquery with no rows
    // does.
    let mut groups = std::collections::BTreeMap::new();
    for (source, _, target) in EDGES {
        *groups.entry((source.0, p(target.0))).or_insert(0_i64) += 1;
    }
    assert_eq!(
        execute(
            "MATCH (n) CALL (n) { MATCH (n)-[:R]->(m) RETURN m.p AS p, count(*) AS c } \
             RETURN n, p, c ORDER BY n, p",
        ),
        groups
            .into_iter()
            .map(|((id, p), c)| {
                GraphValueRow::from_owned_values(vec![
                    GraphValue::Vertex(VId(id)),
                    GraphValue::Scalar(CanonicalScalar::Int(p)),
                    GraphValue::Scalar(CanonicalScalar::Int(c)),
                ])
            })
            .collect::<Vec<_>>()
    );
    // An uncorrelated body aggregates once.
    assert_eq!(
        execute("CALL { MATCH (n) RETURN count(n) AS total } RETURN total"),
        vec![GraphValueRow::from_owned_values(vec![GraphValue::Scalar(
            CanonicalScalar::Int(4)
        )])]
    );
}

#[test]
fn implicit_exists_bodies_keep_clause_boundaries_and_outer_occurrences() {
    let row = |id, present| {
        GraphValueRow::from_owned_values(vec![
            GraphValue::Vertex(VId(id)),
            GraphValue::Scalar(CanonicalScalar::Bool(present)),
        ])
    };
    for (body, present) in [
        // The two MATCH clauses require a two-hop walk. Parallel first-hop
        // witnesses never multiply an outer occurrence.
        (
            "MATCH (n)-[:R]->(m) MATCH (m)-[:R]->(end)",
            [true, true, false, false],
        ),
        // A failed OPTIONAL MATCH supplies a NULL-extended row, so it is
        // still a witness. The mandatory first hop must nevertheless exist.
        (
            "MATCH (n)-[:R]->(m) OPTIONAL MATCH (m)-[:R]->(end)",
            [true, true, true, false],
        ),
        // A required clause after the optional one consumes its NULL row
        // and eliminates it. Moving that clause inside the optional scope
        // would give the wrong presence result for n=2 and n=3.
        (
            "MATCH (n)-[:R]->(m) OPTIONAL MATCH (m)-[:R]->(end) MATCH (end)-[:R]->()",
            [true, false, false, false],
        ),
        // A leading optional body has one witness even for isolated n=4.
        ("OPTIONAL MATCH (n)-[:R]->(m)", [true; 4]),
    ] {
        let statement = format!("MATCH (n) RETURN n,EXISTS {{ {body} }} AS present ORDER BY n");
        assert_eq!(
            execute_identified(&statement),
            present
                .into_iter()
                .enumerate()
                .map(|(index, present)| row(index as u128 + 1, present))
                .collect::<Vec<_>>(),
            "{statement}"
        );
    }
    assert_eq!(
        execute_identified(
            "MATCH (n) WITH n,EXISTS { MATCH (n)-[:R]->(m) MATCH (m)-[:R]->() } AS present \
             WHERE present RETURN n ORDER BY n"
        ),
        vec![vertices(&[1]), vertices(&[2])]
    );
    let query = prepare_items(
        "UNWIND $items AS x WHERE EXISTS { MATCH (x)-[:R]->(m) MATCH (m)-[:R]->() } \
         RETURN x",
    )
    .bind_parameters(
        &GqlParameters::new()
            .with_list(
                "items",
                vec![
                    GraphValue::Vertex(VId(1)),
                    GraphValue::Vertex(VId(1)),
                    GraphValue::Vertex(VId(2)),
                    GraphValue::Vertex(VId(3)),
                    GraphValue::Scalar(CanonicalScalar::Null),
                ],
            )
            .unwrap(),
    )
    .unwrap();
    let mut rows = run_identified(&query);
    rows.sort();
    assert_eq!(rows, vec![vertices(&[1]), vertices(&[1]), vertices(&[2])]);
}

#[test]
fn implicit_exists_with_stages_apply_grouping_pages_and_name_scope() {
    let query = prepare(
        "MATCH (n) RETURN n,EXISTS { MATCH (n)-[:R]->(m) \
         WITH n,count(*) AS degree WHERE degree = 1 } AS present ORDER BY n",
    );
    assert_eq!(
        run(&query, wide(), &mut || Ok(())).unwrap().value,
        (1..=4)
            .map(|id| GraphValueRow::from_owned_values(vec![
                GraphValue::Vertex(VId(id)),
                GraphValue::Scalar(CanonicalScalar::Bool(id == 2 || id == 3)),
            ]))
            .collect::<Vec<_>>()
    );
    // WITH drops n. The later MATCH (n) binds a fresh body-local vertex,
    // rather than recapturing the outer one. Only the first-hop existence
    // depends on the outer n, and the final n.p=1 is satisfied independently.
    assert_eq!(
        execute(
            "MATCH (n) WHERE EXISTS { MATCH (n)-[:R]->(m) \
             WITH m MATCH (n) WHERE n.p = 1 } RETURN n ORDER BY n"
        ),
        vec![vertices(&[1]), vertices(&[2]), vertices(&[3])]
    );
    // A keyless aggregate emits an empty-input group for n=4. The following
    // WHERE retains precisely that group, even without an explicit RETURN.
    assert_eq!(
        execute(
            "MATCH (n) WHERE EXISTS { MATCH (n)-[:R]->(m) \
             WITH count(*) AS degree WHERE degree = 0 } RETURN n"
        ),
        vec![vertices(&[4])]
    );
    assert_eq!(
        execute(
            "MATCH (n) WHERE EXISTS { MATCH (n)-[:R]->(m) \
             WITH DISTINCT m ORDER BY m SKIP 1 LIMIT 1 } RETURN n"
        ),
        vec![vertices(&[1])]
    );
    // A scalar-only body is evaluated per outer occurrence; FALSE/NULL
    // projected values are rows and do not change existential truth.
    assert_eq!(
        execute(
            "UNWIND [1,2,2] AS x RETURN x,\
             EXISTS { WITH x AS y WHERE y = 2 WITH null AS ignored } AS present ORDER BY x"
        ),
        [1, 2, 2]
            .into_iter()
            .map(|value| GraphValueRow::from_owned_values(vec![
                GraphValue::Scalar(CanonicalScalar::Int(value)),
                GraphValue::Scalar(CanonicalScalar::Bool(value == 2)),
            ]))
            .collect::<Vec<_>>()
    );
    for statement in [
        "MATCH (n) WHERE EXISTS { MATCH (n) WITH 1 AS value WITH value WHERE n.p = 1 } RETURN n",
        "MATCH (n) WHERE EXISTS { MATCH (n) MATCH (m) } RETURN m",
        "MATCH (n) WHERE EXISTS { MATCH (n) WITH n CREATE () } RETURN n",
        "MATCH (n) WHERE EXISTS { MATCH (n) MATCH } RETURN n",
        "RETURN EXISTS { WITH 1 AS n CALL { MATCH (m) } } AS present",
    ] {
        assert!(
            PreparedGraphSetText::prepare(statement, |_, _: &str| {
                panic!("scope and syntax refusals precede catalog: {statement}")
            })
            .is_err(),
            "{statement}"
        );
    }
}

#[test]
fn implicit_exists_preserves_late_refusals_and_cumulative_admission() {
    for statement in [
        "WITH 0 AS x WHERE EXISTS { WITH 1 / x AS bad } RETURN x LIMIT 0",
        "WITH 0 AS x WHERE EXISTS { WITH 1 / x AS bad LIMIT 0 } RETURN x",
        "WITH 1 AS x WHERE EXISTS { UNWIND [1,0] AS d WITH 1 / d AS bad } RETURN x",
    ] {
        assert!(
            run(&prepare(statement), wide(), &mut || Ok(())).is_err(),
            "{statement}"
        );
    }
    let query = prepare(
        "MATCH (n) WHERE EXISTS { MATCH (n)-[:R]->(m) \
         WITH count(*) AS degree WHERE degree >= 0 } RETURN n",
    );
    let mut checkpoints = 0;
    let measured = run(&query, wide(), &mut || {
        checkpoints += 1;
        Ok(())
    })
    .unwrap();
    let caps = [
        measured.rows.snapshot_records,
        measured.rows.result_rows,
        measured.evaluator.work_units,
        measured.evaluator.scratch_entries,
    ];
    assert_eq!(
        caps[1], 4,
        "only the outer result spends result-row allowance"
    );
    assert_eq!(
        run(
            &query,
            GqlQueryPolicy::new(caps[0], caps[1], caps[2], caps[3]),
            &mut || Ok(())
        )
        .unwrap(),
        measured
    );
    for dimension in 0..4 {
        let mut limited = caps;
        assert!(limited[dimension] > 0);
        limited[dimension] -= 1;
        assert!(
            run(
                &query,
                GqlQueryPolicy::new(limited[0], limited[1], limited[2], limited[3]),
                &mut || Ok(())
            )
            .is_err(),
            "dimension {dimension}"
        );
    }
    for stop in 0..checkpoints {
        let mut seen = 0;
        assert!(
            run(&query, wide(), &mut || {
                let current = seen;
                seen += 1;
                if current == stop { Err(stop) } else { Ok(()) }
            })
            .is_err(),
            "checkpoint {stop}"
        );
    }
    assert_eq!(run(&query, wide(), &mut || Ok(())).unwrap(), measured);
}

#[test]
fn returning_exists_groups_and_filters_each_correlated_body_before_testing_presence() {
    let template = PreparedGraphSetText::prepare(
        "MATCH (n) WHERE EXISTS { MATCH (n)-[:R]->(m) \
         WITH n,count(*) AS degree WHERE degree = $wanted RETURN false } \
         RETURN n ORDER BY n",
        symbols,
    )
    .unwrap();
    let frozen = template.canonical_template_bytes();
    for wanted in [0, 1, 2, 3] {
        let arguments = GqlParameters::new().with_int64("wanted", wanted).unwrap();
        let query = template.bind_parameters(&arguments).unwrap();
        let expected: Vec<_> = (1..=4)
            .filter(|id| {
                let degree = EDGES
                    .iter()
                    .filter(|(from, _, _)| *from == VId(*id))
                    .count();
                // A grouped aggregate over no matches has no group. Returning
                // false still supplies a row, and therefore an EXISTS witness.
                degree != 0 && degree as i64 == wanted
            })
            .map(|id| vertices(&[id]))
            .collect();
        assert_eq!(run(&query, wide(), &mut || Ok(())).unwrap().value, expected);
    }
    assert_eq!(template.canonical_template_bytes(), frozen);
    assert!(template.bind_parameters(&GqlParameters::new()).is_err());

    // Independent local patterns do not accidentally acquire an outer key.
    assert_eq!(
        execute(
            "MATCH (n) WHERE EXISTS { MATCH (m)-[:R]->() \
             WITH count(*) AS total WHERE total = 5 RETURN null } RETURN n ORDER BY n"
        ),
        (1..=4).map(|id| vertices(&[id])).collect::<Vec<_>>()
    );
    // Body pages apply to its completed groups before the existence test.
    assert_eq!(
        execute(
            "MATCH (n) WHERE EXISTS { MATCH (n)-[:R]->(m) \
             WITH m,count(*) AS degree RETURN m ORDER BY m SKIP 1 LIMIT 1 } RETURN n"
        ),
        vec![vertices(&[1])]
    );
}

#[test]
fn returning_exists_keeps_global_empty_groups_and_never_multiplies_outer_rows() {
    for body in [
        "MATCH (n)-[:R]->(m) RETURN count(*) AS degree",
        "MATCH (n)-[:R]->(m) WITH count(*) AS degree RETURN false",
    ] {
        let statement = format!("MATCH (n) RETURN n,EXISTS {{ {body} }} AS present ORDER BY n");
        assert_eq!(
            execute(&statement),
            (1..=4)
                .map(|id| GraphValueRow::from_owned_values(vec![
                    GraphValue::Vertex(VId(id)),
                    GraphValue::Scalar(CanonicalScalar::Bool(true)),
                ]))
                .collect::<Vec<_>>(),
            "{statement}"
        );
    }
    assert_eq!(
        execute(
            "MATCH (n) RETURN n,EXISTS { MATCH (n)-[:R]->(m) \
             WITH count(*) AS degree WHERE degree = 0 RETURN null } AS absent ORDER BY n"
        ),
        (1..=4)
            .map(|id| GraphValueRow::from_owned_values(vec![
                GraphValue::Vertex(VId(id)),
                GraphValue::Scalar(CanonicalScalar::Bool(id == 4)),
            ]))
            .collect::<Vec<_>>()
    );
    for negated in [false, true] {
        let negate = if negated { "NOT " } else { "" };
        let statement = format!(
            "UNWIND $items AS x WHERE {negate}EXISTS {{ MATCH (x)-[:R]->(m) \
             WITH x,count(*) AS degree WHERE degree > 0 RETURN degree }} RETURN x"
        );
        let template = PreparedGraphSetText::prepare(&statement, symbols).unwrap();
        let items = vec![
            GraphValue::Vertex(VId(1)),
            GraphValue::Vertex(VId(1)),
            GraphValue::Vertex(VId(4)),
            GraphValue::Vertex(VId(u128::MAX)),
            GraphValue::Scalar(CanonicalScalar::Null),
            GraphValue::Scalar(CanonicalScalar::Int(1)),
        ];
        let mut expected: Vec<_> = items
            .iter()
            .filter(|item| {
                let present = matches!(item, GraphValue::Vertex(id)
                if EDGES.iter().any(|(from, _, _)| from == id));
                present != negated
            })
            .map(|item| GraphValueRow::from_owned_values(vec![item.clone()]))
            .collect();
        expected.sort();
        let query = template
            .bind_parameters(&GqlParameters::new().with_list("items", items).unwrap())
            .unwrap();
        let mut actual = run(&query, wide(), &mut || Ok(())).unwrap().value;
        actual.sort();
        assert_eq!(actual, expected, "{statement}");
    }
}

#[test]
fn returning_exists_composes_boolean_values_without_exporting_body_names() {
    assert_eq!(
        execute(
            "MATCH (n) WITH n WHERE n.p > 1 AND EXISTS { MATCH (n)-[:R]->(m) \
             WITH count(*) AS degree WHERE degree = 1 RETURN true } RETURN n ORDER BY n"
        ),
        vec![vertices(&[2]), vertices(&[3])]
    );
    assert_eq!(
        execute(
            "MATCH (n) WITH n RETURN n,CASE WHEN EXISTS { MATCH (n)-[:R]->(m) \
             WITH count(*) AS degree WHERE degree = 0 RETURN false } \
             THEN 7 ELSE 9 END AS score ORDER BY n"
        ),
        (1..=4)
            .map(|id| GraphValueRow::from_owned_values(vec![
                GraphValue::Vertex(VId(id)),
                GraphValue::Scalar(CanonicalScalar::Int(if id == 4 { 7 } else { 9 })),
            ]))
            .collect::<Vec<_>>()
    );
    for statement in [
        "MATCH (n) WHERE EXISTS { MATCH (n)-[:R]->(m) \
         WITH count(*) AS degree RETURN m } RETURN n",
        "MATCH (n) WHERE EXISTS { MATCH (n)-[:R]->(m) \
         WITH count(*) AS degree RETURN degree } RETURN degree",
        "MATCH (n) WHERE EXISTS { MATCH (n)-[:R]->(m) \
         WITH count(*) AS degree CREATE (:R) RETURN degree } RETURN n",
    ] {
        let calls = std::cell::Cell::new(0);
        assert!(
            PreparedGraphSetText::prepare(statement, |kind, name| {
                calls.set(calls.get() + 1);
                symbols(kind, name)
            })
            .is_err(),
            "{statement}"
        );
        assert_eq!(
            calls.get(),
            0,
            "scope/effect admission precedes catalog: {statement}"
        );
    }
    let nested = |depth| {
        (0..depth).fold("RETURN true AS ok".to_owned(), |body, _| {
            format!("RETURN EXISTS {{ {body} }} AS ok")
        })
    };
    assert_eq!(execute(&nested(3)).len(), 1);
    assert!(
        PreparedGraphSetText::prepare(&nested(9), |_, _: &str| panic!(
            "subquery depth admission precedes catalog"
        ))
        .is_err()
    );
}

#[test]
fn returning_exists_shares_parameters_and_restores_outer_names_between_bodies() {
    let template = PreparedGraphSetText::prepare(
        "UNWIND $items AS x RETURN x,\
         EXISTS { WITH x + $delta AS y WHERE y >= $minimum RETURN false } AS high,\
         EXISTS { WITH x + $delta AS y WHERE y < $maximum RETURN null } AS low,\
         x + $tail AS value",
        |_, _: &str| panic!("source-free existential bodies have no catalog reads"),
    )
    .unwrap();
    let frozen = template.canonical_template_bytes();
    for delta in [2, 5] {
        let arguments = GqlParameters::new()
            .with_list(
                "items",
                [1, 2, 2]
                    .map(|x| GraphValue::Scalar(CanonicalScalar::Int(x)))
                    .to_vec(),
            )
            .unwrap()
            .with_int64("delta", delta)
            .unwrap()
            .with_int64("minimum", 4)
            .unwrap()
            .with_int64("maximum", 5)
            .unwrap()
            .with_int64("tail", 10)
            .unwrap();
        let query = template.bind_parameters(&arguments).unwrap();
        assert_eq!(query.columns(), ["x", "high", "low", "value"]);
        let mut expected: Vec<_> = [1, 2, 2]
            .into_iter()
            .map(|x| {
                GraphValueRow::from_owned_values(vec![
                    GraphValue::Scalar(CanonicalScalar::Int(x)),
                    GraphValue::Scalar(CanonicalScalar::Bool(x + delta >= 4)),
                    GraphValue::Scalar(CanonicalScalar::Bool(x + delta < 5)),
                    GraphValue::Scalar(CanonicalScalar::Int(x + 10)),
                ])
            })
            .collect();
        expected.sort();
        let mut actual = run(&query, wide(), &mut || Ok(())).unwrap().value;
        actual.sort();
        assert_eq!(actual, expected);
    }
    assert_eq!(template.canonical_template_bytes(), frozen);
}

#[test]
fn returning_exists_retains_source_and_projection_failures_before_empty_pages() {
    for statement in [
        "WITH 0 AS x WHERE EXISTS { RETURN 1 / x AS bad } RETURN x LIMIT 0",
        "WITH 0 AS x WHERE EXISTS { RETURN 1 / x AS bad LIMIT 0 } RETURN x",
        "WITH 1 AS x WHERE EXISTS { UNWIND [1,0] AS d RETURN 1 / d AS bad } RETURN x",
    ] {
        assert!(
            run(&prepare(statement), wide(), &mut || Ok(())).is_err(),
            "{statement}"
        );
    }
    let statement = "MATCH (n) WHERE EXISTS { MATCH (n)-[:R]->(m) \
                     WITH count(*) AS degree WHERE degree >= 0 RETURN true } RETURN n LIMIT 0";
    let query = prepare(statement);
    let mut calls = 0;
    let result = query.execute_governed(
        wide(),
        |pattern, remaining| {
            calls += 1;
            if calls == 3 {
                return Err(GqlQueryError::Source("second-body-source-error"));
            }
            pattern.plan().execute_governed_with_properties(
                (4 + EDGES.len()) as u64,
                (1..=4).map(VId),
                EDGES,
                |_, _| Ok::<_, &'static str>(true),
                |_, _| Ok(None),
                remaining,
                || Ok::<_, usize>(()),
            )
        },
        || Ok::<_, usize>(()),
    );
    assert_eq!(calls, 3);
    assert!(matches!(
        result,
        Err(GqlQueryError::Source(GraphSetExecutionError::Source(
            "second-body-source-error"
        )))
    ));
}

#[test]
fn returning_exists_uses_one_cumulative_allowance_and_restores_argument_frames_on_cancel() {
    let query = prepare(
        "MATCH (n) WHERE EXISTS { MATCH (n)-[:R]->(m) \
         WITH count(*) AS degree WHERE degree >= 0 RETURN degree } RETURN n",
    );
    let mut checkpoints = 0;
    let measured = run(&query, wide(), &mut || {
        checkpoints += 1;
        Ok(())
    })
    .unwrap();
    let caps = [
        measured.rows.snapshot_records,
        measured.rows.result_rows,
        measured.evaluator.work_units,
        measured.evaluator.scratch_entries,
    ];
    assert_eq!(
        caps[1], 4,
        "only outer results spend delivered-row allowance"
    );
    assert_eq!(
        run(
            &query,
            GqlQueryPolicy::new(caps[0], caps[1], caps[2], caps[3]),
            &mut || Ok(())
        )
        .unwrap(),
        measured
    );
    for dimension in 0..4 {
        let mut limited = caps;
        assert!(limited[dimension] > 0);
        limited[dimension] -= 1;
        assert!(
            run(
                &query,
                GqlQueryPolicy::new(limited[0], limited[1], limited[2], limited[3]),
                &mut || Ok(())
            )
            .is_err(),
            "dimension {dimension}"
        );
    }
    assert!(checkpoints > 0);
    for stop in 0..checkpoints {
        let mut seen = 0;
        let result = run(&query, wide(), &mut || {
            let current = seen;
            seen += 1;
            if current == stop { Err(stop) } else { Ok(()) }
        });
        assert!(
            matches!(result, Err(GqlQueryError::Interrupted(at)) if at == stop),
            "{stop}: {result:?}"
        );
    }
    assert_eq!(run(&query, wide(), &mut || Ok(())).unwrap(), measured);
}

#[test]
fn inline_call_parameters_governance_and_scope_bounds_span_the_whole_definition() {
    let text = "WITH $outer AS x CALL (x) { RETURN x+$inner AS y } RETURN y+$tail AS total";
    let template = PreparedGraphSetText::prepare(text, |_, _: &str| {
        panic!("source-free CALL must not resolve a symbol")
    })
    .unwrap();
    let before = template.canonical_template_bytes();
    let arguments = GqlParameters::new()
        .with_int64("outer", 2)
        .unwrap()
        .with_int64("inner", 3)
        .unwrap()
        .with_int64("tail", 4)
        .unwrap();
    let query = template.bind_parameters(&arguments).unwrap();
    let expected = vec![GraphValueRow::from_owned_values(vec![GraphValue::Scalar(
        CanonicalScalar::Int(9),
    )])];
    assert_eq!(run(&query, wide(), &mut || Ok(())).unwrap().value, expected);
    assert_eq!(before, template.canonical_template_bytes());
    assert!(template.bind_parameters(&GqlParameters::new()).is_err());
    assert!(
        template
            .bind_parameters(&arguments.with_int64("extra", 0).unwrap())
            .is_err()
    );
    assert!(
        run(
            &query,
            GqlQueryPolicy::new(u64::MAX, u64::MAX, 0, u64::MAX),
            &mut || Ok(())
        )
        .is_err()
    );
    let mut checkpoints = 0;
    run(&query, wide(), &mut || {
        checkpoints += 1;
        Ok(())
    })
    .unwrap();
    assert!(checkpoints > 0);
    for stop in 0..checkpoints {
        let mut seen = 0;
        let result = run(&query, wide(), &mut || {
            let current = seen;
            seen += 1;
            if current == stop { Err(stop) } else { Ok(()) }
        });
        assert!(
            matches!(result,Err(GqlQueryError::Interrupted(at)) if at==stop),
            "{stop}: {result:?}"
        );
    }
    for text in [
        "WITH 0 AS x CALL (x) { WITH 1/x AS y RETURN y LIMIT 0 } RETURN y",
        "WITH 0 AS x CALL (x) { RETURN 1/x AS y } RETURN y LIMIT 0",
    ] {
        assert!(
            run(&prepare(text), wide(), &mut || Ok(())).is_err(),
            "{text}"
        );
    }
    let nested = |count| {
        (0..count).fold("RETURN 1 AS x".to_owned(), |body, _| {
            format!("CALL {{ {body} }} RETURN x")
        })
    };
    assert_eq!(execute(&nested(8)).len(), 1);
    assert!(
        PreparedGraphSetText::prepare(&nested(9), |_, _: &str| panic!(
            "depth must refuse before catalog"
        ))
        .is_err()
    );
}

#[test]
fn inline_call_refuses_escaping_names_collisions_and_effects_before_catalog() {
    for text in [
        "WITH 1 AS x CALL (missing) { RETURN 2 AS y } RETURN y",
        "WITH 1 AS x CALL (x,x) { RETURN 2 AS y } RETURN y",
        "WITH 1 AS x CALL { RETURN x AS y } RETURN y",
        "WITH 1 AS x CALL (x) { RETURN x } RETURN x",
        "CALL { WITH 2 AS hidden RETURN 1 AS y } RETURN hidden",
        "CALL { MATCH (n) RETURN n } WITH 1 AS x RETURN n.p",
        "CALL { CREATE (n) RETURN n } RETURN n",
        "CALL { MATCH (n) DELETE n RETURN 1 AS y } RETURN y",
    ] {
        let calls = std::cell::Cell::new(0);
        let result = PreparedGraphSetText::prepare(text, |kind, name: &str| {
            calls.set(calls.get() + 1);
            symbols(kind, name)
        });
        assert!(result.is_err(), "{text}");
        assert_eq!(
            calls.get(),
            0,
            "refusal must precede catalog access: {text}"
        );
    }
}

#[test]
fn required_continuation_preserves_parallel_edges_and_applies_input_page_first() {
    // This exact form used to be on with_pipelines' refusal list. Independent
    // edge enumeration determines its positive result, including parallel edges.
    let query = prepare("MATCH (n) WITH n MATCH (n)-[:R]->(m) RETURN n,m ORDER BY n,m");
    let result = run(&query, wide(), &mut || Ok(())).unwrap();
    let expected: Vec<_> = EDGES
        .iter()
        .map(|(a, _, b)| vertices(&[a.0, b.0]))
        .collect();
    assert_eq!(result.value, expected);
    assert_eq!(result.rows.snapshot_records, 18);
    assert_eq!(query.operand_count(), 2);
    assert_eq!(
        execute("MATCH (n) WITH n ORDER BY n LIMIT 1 MATCH (n)-[:R]->(m) RETURN m ORDER BY m"),
        vec![vertices(&[2]), vertices(&[2]), vertices(&[3])],
    );
    assert_eq!(
        execute("MATCH (n) WITH n ORDER BY n LIMIT 1 WHERE n IS NULL MATCH (n)-[:R]->(m) RETURN m"),
        Vec::<GraphValueRow>::new(),
    );
}

#[test]
fn renamed_identity_and_carried_scalar_survive_three_graph_parts() {
    let text = "MATCH (n) WITH n AS root,n.p AS score WHERE score=1 \
        MATCH (root)-[:R]->(m) WITH m AS root,score+1 AS score \
        MATCH (root)-[:R]->(destination) RETURN score,destination ORDER BY destination";
    let mut expected = Vec::new();
    for &(source, _, middle) in &EDGES {
        if source != VId(1) {
            continue;
        }
        for &(next, _, destination) in &EDGES {
            if middle == next {
                expected.push(scalar_vertex(2, destination.0));
            }
        }
    }
    expected.sort();
    assert_eq!(execute(text), expected);
    assert_eq!(
        execute("MATCH (n) WITH n AS owner MATCH (owner) RETURN owner.p AS p ORDER BY p"),
        [1, 2, 2, 3]
            .map(
                |n| GraphValueRow::from_owned_values(vec![GraphValue::Scalar(
                    CanonicalScalar::Int(n)
                ),])
            )
            .to_vec(),
    );
}

#[test]
fn repeated_property_values_join_as_bags_and_null_unwind_keys_never_match() {
    let expected = vec![
        scalar_vertex(2, 2),
        scalar_vertex(2, 2),
        scalar_vertex(2, 3),
        scalar_vertex(2, 3),
    ];
    assert_eq!(
        execute(
            "MATCH (n) WITH n.p AS wanted WHERE wanted=2 MATCH (m {p:wanted}) \
         RETURN wanted,m ORDER BY m"
        ),
        expected
    );
    assert_eq!(
        execute(
            "UNWIND [2,2,NULL] AS wanted WITH wanted MATCH (m {p:wanted}) \
         RETURN wanted,m ORDER BY m"
        ),
        expected
    );
    assert_eq!(
        execute(
            "MATCH (n) WITH DISTINCT n.p AS wanted WHERE wanted=2 MATCH (m {p:wanted}) \
         RETURN wanted,m ORDER BY m"
        ),
        vec![scalar_vertex(2, 2), scalar_vertex(2, 3)]
    );
}

#[test]
fn null_imports_from_optional_match_are_not_rebound_to_arbitrary_vertices() {
    assert_eq!(
        execute(
            "MATCH (n) OPTIONAL MATCH (n)-[:R]->(m) WITH m \
         MATCH (m)-[:R]->(destination) RETURN destination ORDER BY destination"
        ),
        vec![
            vertices(&[3]),
            vertices(&[3]),
            vertices(&[4]),
            vertices(&[4])
        ]
    );
}

#[test]
fn one_parameter_and_catalog_contract_spans_all_parts_and_rebinding() {
    let text = "\u{2003}MATCH (n) WHERE n.p=$start WITH n LIMIT $top \
        MATCH (n)-[:R]->(m) WHERE m.p >= $min RETURN m.p+$add AS score";
    let mut calls = BTreeSet::new();
    let template = PreparedGraphSetText::prepare(text, |kind, name| {
        assert!(
            calls.insert((kind, name.to_owned())),
            "duplicate catalog callback"
        );
        symbols(kind, name)
    })
    .unwrap();
    assert_eq!(calls.len(), 2);
    assert_eq!(template.parameter_schema().len(), 4);
    let before = template.canonical_template_bytes();
    let args = GqlParameters::new()
        .with_int64("start", 1)
        .unwrap()
        .with_uint64("top", 1)
        .unwrap()
        .with_int64("min", 2)
        .unwrap();
    let missing = template.bind_parameters(&args).unwrap_err();
    assert_eq!(missing.offset, text.find("$add").unwrap());
    let a = template
        .bind_parameters(&args.clone().with_int64("add", 3).unwrap())
        .unwrap();
    let b = template
        .bind_parameters(&args.with_int64("add", 4).unwrap())
        .unwrap();
    assert_ne!(a.canonical_bytes(), b.canonical_bytes());
    assert_eq!(template.canonical_template_bytes(), before);
    let result = run(&a, wide(), &mut || Ok(())).unwrap();
    assert_eq!(result.value.len(), 3);
    assert!(
        result
            .value
            .iter()
            .all(|row| row.get(0).unwrap().as_scalar() == Some(&CanonicalScalar::Int(5)))
    );
    assert!(!format!("{template:?}").contains("score"));
}

#[test]
fn all_sources_and_fallible_projections_execute_even_after_empty_input_or_limit_zero() {
    for text in [
        "MATCH (n) WITH n LIMIT 0 MATCH (n) RETURN n",
        "MATCH (n) WITH n MATCH (n) RETURN n LIMIT 0",
    ] {
        let query = prepare(text);
        let mut calls = 0;
        let result = query.execute_governed(
            wide(),
            |_, _| {
                calls += 1;
                if calls == 1 {
                    Ok(GqlQueryExecution {
                        value: Vec::new(),
                        rows: fgdb_gql::GqlExecutionStats {
                            snapshot_records: 0,
                            result_rows: 0,
                        },
                        evaluator: fgdb_gql::GlaExecutionStats::default(),
                    })
                } else {
                    Err(GqlQueryError::Source("second-source-error"))
                }
            },
            || Ok::<_, usize>(()),
        );
        assert!(matches!(
            result,
            Err(GqlQueryError::Source(GraphSetExecutionError::Source(
                "second-source-error"
            )))
        ));
        assert_eq!(calls, 2);
    }
    let late =
        prepare("MATCH (n) WITH n LIMIT 1 MATCH (n)-[:R]->(m) RETURN 1/(m.p-m.p) AS bad LIMIT 0");
    assert!(matches!(
        run(&late, wide(), &mut || Ok(())),
        Err(GqlQueryError::Source(
            GraphSetExecutionError::Projection { .. }
        ))
    ));
}

#[test]
fn cumulative_budgets_and_every_cancellation_checkpoint_cover_the_whole_join() {
    let query = prepare("MATCH (n) WITH n LIMIT 1 MATCH (n)-[:R]->(m) RETURN m");
    let mut calls = 0;
    let measured = run(&query, wide(), &mut || {
        calls += 1;
        Ok(())
    })
    .unwrap();
    let caps = [
        measured.rows.snapshot_records,
        measured.rows.result_rows,
        measured.evaluator.work_units,
        measured.evaluator.scratch_entries,
    ];
    assert_eq!(
        run(
            &query,
            GqlQueryPolicy::new(caps[0], caps[1], caps[2], caps[3]),
            &mut || Ok(())
        )
        .unwrap(),
        measured
    );
    for dimension in 0..4 {
        let mut limits = caps;
        assert!(limits[dimension] > 0);
        limits[dimension] -= 1;
        assert!(
            run(
                &query,
                GqlQueryPolicy::new(limits[0], limits[1], limits[2], limits[3]),
                &mut || Ok(())
            )
            .is_err()
        );
    }
    for stop in 1..=calls {
        let mut seen = 0;
        let result = run(&query, wide(), &mut || {
            seen += 1;
            if seen == stop { Err(stop) } else { Ok(()) }
        });
        assert!(matches!(result, Err(GqlQueryError::Interrupted(value)) if value == stop));
        assert_eq!(seen, stop);
    }
}

#[test]
fn illegal_imports_scopes_and_statement_wide_depth_refuse_before_catalog() {
    // A carried vertex's property after a later MATCH re-matches it by
    // identity (fgdb-dhcfo; the rows are pinned below).
    assert!(
        PreparedGraphSetText::prepare(
            "MATCH (n) WITH n AS kept MATCH (m) RETURN kept.p AS value",
            symbols
        )
        .is_ok()
    );
    for text in [
        "MATCH (n) WITH n.p AS n MATCH (n)-[:R]->(m) RETURN m",
        "MATCH (n) WITH n AS kept MATCH (m) RETURN n",
        "MATCH (n) WITH n AS kept MATCH (m) OPTIONAL MATCH (kept)-[:R]->(x) RETURN x",
        "MATCH (n) WITH n AS kept MATCH (m) WHERE EXISTS { MATCH (kept)-[:R]->(x) } RETURN m",
        "MATCH (n) WITH n MATCH (n) RETURN n+1 AS bad",
    ] {
        let mut calls = 0;
        assert!(
            PreparedGraphSetText::prepare(text, |kind, name| {
                calls += 1;
                symbols(kind, name)
            })
            .is_err(),
            "{text}"
        );
        assert_eq!(calls, 0, "{text}");
    }
    let too_deep = format!(
        "MATCH (n){} RETURN n",
        " WITH n MATCH (n)".repeat(fgdb_gql::MAX_GRAPH_SET_DEPTH)
    );
    let mut calls = 0;
    assert!(
        PreparedGraphSetText::prepare(&too_deep, |kind, name| {
            calls += 1;
            symbols(kind, name)
        })
        .is_err()
    );
    assert_eq!(calls, 0);
}

#[test]
fn set_arms_and_literal_routing_keep_their_original_boundaries() {
    assert_eq!(
        execute(
            "(MATCH (n) WITH n ORDER BY n LIMIT 1 MATCH (n)-[:R]->(m) RETURN m LIMIT 1) \
         UNION ALL (MATCH (n) WITH n WHERE n.p=3 MATCH (n) RETURN n AS m) ORDER BY m"
        ),
        vec![vertices(&[2]), vertices(&[4])]
    );
    let literal = "RETURN 'WITH n MATCH (m)' AS text";
    let template = PreparedGraphSetText::prepare(literal, |_, _| None).unwrap();
    assert_eq!(template.columns(), &["text"]);
    assert_eq!(execute("MATCH (n) WITH n MATCH (n) RETURN *").len(), 4);
}

/// fgdb-ezgeq: after WITH, `MATCH (q) WHERE q.p = m` is the equi-join that
/// openCypher's `MATCH (q {p: m})` spells, not a parameter slot. The two
/// definitions are byte-identical, so they share one semantics: NULL never
/// matches and kinds never coerce.
#[test]
fn a_top_level_where_equality_with_an_imported_column_is_the_property_map_join() {
    let template = |text: &str| {
        PreparedGraphSetText::prepare(text, symbols)
            .unwrap()
            .canonical_template_bytes()
    };
    for (map, spelled) in [
        (
            "MATCH (n) WITH max(n.p) AS m MATCH (q {p: m}) RETURN q",
            "MATCH (n) WITH max(n.p) AS m MATCH (q) WHERE q.p = m RETURN q",
        ),
        (
            "MATCH (n) WITH max(n.p) AS m MATCH (q {p: m}) RETURN q",
            "MATCH (n) WITH max(n.p) AS m MATCH (q) WHERE m = q.p RETURN q",
        ),
        (
            "MATCH (n) WITH n.p AS v MATCH (q {p: v})-[:R]->(r) WHERE r.p > 1 RETURN v, r",
            "MATCH (n) WITH n.p AS v MATCH (q)-[:R]->(r) WHERE q.p = v AND r.p > 1 RETURN v, r",
        ),
    ] {
        assert_eq!(template(map), template(spelled), "{spelled}");
    }
    // p = [1, 2, 2, 3]: only vertex 4 holds the maximum.
    assert_eq!(
        execute("MATCH (n) WITH max(n.p) AS m MATCH (q) WHERE q.p = m RETURN q"),
        vec![vertices(&[4])]
    );
    // Each imported v joins every vertex holding it: v = 2 arrives twice and
    // joins vertices 2 and 3 each time.
    assert_eq!(
        execute("MATCH (n) WITH n.p AS v MATCH (q) WHERE q.p = v RETURN v, q ORDER BY v, q"),
        [(1, 1), (2, 2), (2, 2), (2, 3), (2, 3), (3, 4)]
            .map(|(value, vid)| scalar_vertex(value, vid))
            .to_vec()
    );
    // A comparison that is not equality filters the same imported product:
    // every vertex below the maximum 3.
    assert_eq!(
        execute("MATCH (n) WITH max(n.p) AS m MATCH (q) WHERE q.p < m RETURN q ORDER BY q"),
        [1, 2, 3].map(|id| vertices(&[id])).to_vec()
    );
    // Not a top-level AND conjunct: no join key, but the row filter after
    // the join that `WITH * WHERE` spells. m = 3, held only by vertex 4.
    for (text, ids) in [
        (
            "MATCH (n) WITH max(n.p) AS m MATCH (q) WHERE q.p = m OR q.p = 1 RETURN q ORDER BY q",
            &[1, 4][..],
        ),
        (
            "MATCH (n) WITH max(n.p) AS m MATCH (q) WHERE NOT q.p = m RETURN q ORDER BY q",
            &[1, 2, 3][..],
        ),
    ] {
        let expected: Vec<_> = ids.iter().map(|&id| vertices(&[id])).collect();
        assert_eq!(execute(text), expected, "{text}");
    }
    // An OPTIONAL clause after MATCH (a) would hide the imported column in
    // a child scope. It starts a new part at an implicit `WITH *`, which
    // openCypher's clause-by-clause join already draws: the template of the
    // explicit spelling, and m = 3 held only by vertex 4, reached from 3.
    let text = "MATCH (n) WITH max(n.p) AS m MATCH (a) OPTIONAL MATCH (a)-[:R]->(q) \
                WHERE q.p = m RETURN a, q ORDER BY a";
    let explicit = "MATCH (n) WITH max(n.p) AS m MATCH (a) WITH * OPTIONAL MATCH (a)-[:R]->(q) \
                    WHERE q.p = m RETURN a, q ORDER BY a";
    assert_eq!(
        PreparedGraphSetText::prepare(text, symbols)
            .unwrap()
            .canonical_template_bytes(),
        PreparedGraphSetText::prepare(explicit, symbols)
            .unwrap()
            .canonical_template_bytes()
    );
    let null = GraphValue::Scalar(CanonicalScalar::Null);
    let row = |a: u128, q: GraphValue| {
        GraphValueRow::from_owned_values(vec![GraphValue::Vertex(VId(a)), q])
    };
    assert_eq!(
        execute(text),
        vec![
            row(1, null.clone()),
            row(2, null.clone()),
            row(3, GraphValue::Vertex(VId(4))),
            row(4, null),
        ]
    );
}

/// A required continuation's WHERE that is no graph predicate, such as a
/// computed graph operand against an imported column (TCK Comparison1
/// [1]-[3]: `WITH ... AS expected MATCH (n) WHERE toInteger(n.id) =
/// expected`), is that part's `WITH * WHERE`: the same template, so the same
/// rows. An OPTIONAL continuation's WHERE belongs to its left join: a
/// computed operand there filters inside an optional Apply, before the null
/// extension.
#[test]
fn a_continuation_where_over_a_computed_graph_operand_filters_after_the_join() {
    let template = |text: &str| {
        PreparedGraphSetText::prepare(text, symbols)
            .unwrap()
            .canonical_template_bytes()
    };
    for (implicit, explicit) in [
        (
            "MATCH (n) WITH max(n.p) AS m MATCH (q) WHERE q.p + 0 = m RETURN q",
            "MATCH (n) WITH max(n.p) AS m MATCH (q) WITH * WHERE q.p + 0 = m RETURN q",
        ),
        (
            "MATCH (n) WITH max(n.p) AS m MATCH (q) WHERE toInteger(q.p) = m RETURN q",
            "MATCH (n) WITH max(n.p) AS m MATCH (q) WITH * WHERE toInteger(q.p) = m RETURN q",
        ),
    ] {
        assert_eq!(template(implicit), template(explicit), "{implicit}");
        // m = 3, held only by vertex 4.
        assert_eq!(execute(implicit), vec![vertices(&[4])], "{implicit}");
    }
    // The same after a leading UNWIND, the import idiom's shape (`UNWIND
    // $rows AS row MATCH (n) WHERE toLower(n.name) = row.name`).
    assert_eq!(
        template("UNWIND [3, 1] AS m MATCH (q) WHERE q.p + 0 = m RETURN m, q"),
        template("UNWIND [3, 1] AS m MATCH (q) WITH * WHERE q.p + 0 = m RETURN m, q")
    );
    assert_eq!(
        execute("UNWIND [3, 1] AS m MATCH (q) WHERE q.p + 0 = m RETURN m, q ORDER BY m, q"),
        [(1, 1), (3, 4)]
            .map(|(value, vid)| scalar_vertex(value, vid))
            .to_vec()
    );
    // An OPTIONAL clause's WHERE over a computed graph operand filters inside
    // an optional Apply, after the implicit `WITH *` before the clause: each
    // a keeps its out-edges to a q with q.p = m, or one NULL extension.
    let p = |n: u128| [1, 2, 2, 3][n as usize - 1];
    let m = (1..=4).map(p).max().unwrap();
    let pair = |a: u128, q: GraphValue| {
        GraphValueRow::from_owned_values(vec![GraphValue::Vertex(VId(a)), q])
    };
    let mut expected: Vec<_> = (1..=4)
        .flat_map(|a| {
            let ends: Vec<_> = EDGES
                .iter()
                .filter(|&&(start, _, end)| start == VId(a) && p(end.0) == m)
                .map(|&(_, _, end)| pair(a, GraphValue::Vertex(end)))
                .collect();
            if ends.is_empty() {
                vec![pair(a, GraphValue::Scalar(CanonicalScalar::Null))]
            } else {
                ends
            }
        })
        .collect();
    expected.sort();
    let mut rows = execute(
        "MATCH (n) WITH max(n.p) AS m MATCH (a) OPTIONAL MATCH (a)-[:R]->(q) \
         WHERE q.p + 0 = m RETURN a, q",
    );
    rows.sort();
    assert_eq!(rows, expected);
}

/// `MATCH (a) WITH a MATCH (b) WHERE a = b` correlates `b` with the carried
/// element: the identity form of the same join key, answering what reusing
/// the name `MATCH (a)` answers (TCK Comparison1 [4]/[5], MatchWhere6 [5]).
#[test]
fn a_top_level_where_identity_with_an_imported_element_is_the_reused_name_join() {
    assert_eq!(
        execute("MATCH (a) WITH a MATCH (b) WHERE a = b RETURN a, b ORDER BY a"),
        [1, 2, 3, 4].map(|id| vertices(&[id, id])).to_vec()
    );
    // Parallel edges keep their multiplicity, in either operand order.
    let out = [(1, 2), (1, 2), (1, 3), (2, 3), (3, 4)].map(|(a, m)| vertices(&[a, m]));
    for text in [
        "MATCH (a) WITH a MATCH (b)-[:R]->(m) WHERE b = a RETURN a, m ORDER BY a, m",
        "MATCH (a) WITH a MATCH (b)-[:R]->(m) WHERE a = b AND m.p > 0 RETURN a, m ORDER BY a, m",
        "MATCH (a) WITH a MATCH (a)-[:R]->(m) RETURN a, m ORDER BY a, m",
    ] {
        assert_eq!(execute(text), out.to_vec(), "{text}");
    }
    // OPTIONAL: the equality decides the match before null extension, so
    // vertex 4, which has no outgoing edge, keeps one row with a NULL m.
    let mut optional = out.to_vec();
    optional.push(GraphValueRow::from_owned_values(vec![
        GraphValue::Vertex(VId(4)),
        GraphValue::Scalar(CanonicalScalar::Null),
    ]));
    assert_eq!(
        execute(
            "MATCH (a) WITH a OPTIONAL MATCH (b)-[:R]->(m) WHERE b = a RETURN a, m ORDER BY a, m"
        ),
        optional
    );
    // Relationship identity needs identified edges: multipart_optional_reads
    // a_top_level_where_identity_correlates_a_carried_relationship.
    // Inequality filters the carried product: each of the four vertices is
    // every other carried vertex's b, three times.
    assert_eq!(
        execute("MATCH (a) WITH a MATCH (b) WHERE a <> b RETURN b ORDER BY b"),
        [1, 1, 1, 2, 2, 2, 3, 3, 3, 4, 4, 4]
            .map(|id| vertices(&[id]))
            .to_vec()
    );
    // Not a top-level AND conjunct: no join key, but the row filter after the
    // join that `WITH * WHERE` spells, as for the property form (fgdb-c6hvm).
    // Only vertex 1 has p = 1, so every carried a keeps it: a = 1 keeps 1;
    // a = 2, 3 and 4 keep themselves and 1.
    assert_eq!(
        execute("MATCH (a) WITH a MATCH (b) WHERE a = b OR b.p = 1 RETURN b ORDER BY b"),
        [1, 1, 1, 1, 2, 3, 4].map(|id| vertices(&[id])).to_vec()
    );
    assert_eq!(
        execute("MATCH (a) WITH a MATCH (b) WHERE NOT a = b RETURN b ORDER BY b"),
        [1, 1, 1, 2, 2, 2, 3, 3, 3, 4, 4, 4]
            .map(|id| vertices(&[id]))
            .to_vec()
    );
}

/// fgdb-dhcfo: a carried vertex's property read after a later MATCH is that
/// vertex's own. The part re-matches it by identity, as the name-reuse
/// spelling `WITH a MATCH (a), (b)` does, so both spellings answer alike.
#[test]
fn a_carried_vertex_property_read_after_a_later_match_re_matches_it_by_identity() {
    let ints = |x: i64, y: i64| {
        GraphValueRow::from_owned_values(vec![
            GraphValue::Scalar(CanonicalScalar::Int(x)),
            GraphValue::Scalar(CanonicalScalar::Int(y)),
        ])
    };
    // p = [1, 2, 2, 3] on vertices 1..4: every pair with a.p < b.p.
    let theta = execute(
        "MATCH (a) WITH a MATCH (b) WHERE a.p < b.p RETURN a.p AS x, b.p AS y ORDER BY x, y",
    );
    assert_eq!(
        theta,
        [(1, 2), (1, 2), (1, 3), (2, 3), (2, 3)]
            .map(|(x, y)| ints(x, y))
            .to_vec()
    );
    assert_eq!(
        theta,
        execute(
            "MATCH (a) WITH a MATCH (a), (b) WHERE a.p < b.p RETURN a.p AS x, b.p AS y ORDER BY x, y"
        )
    );
    // A RETURN-only read keeps the carried product: 4 x 4 rows.
    assert_eq!(
        execute("MATCH (a) WITH a MATCH (b) RETURN a.p AS x, b.p AS y ORDER BY x, y").len(),
        16
    );
    // OPTIONAL reads the carried identity on the LEFT, including its value
    // on an unmatched row. The independent bag oracle below covers it.
    // A binding the WITH dropped is still unknown after the next MATCH.
    assert!(
        PreparedGraphSetText::prepare(
            "MATCH (a), (c) WITH a MATCH (b) WHERE c.p < b.p RETURN b",
            symbols
        )
        .is_err()
    );
}

fn nullable_pair(left: Option<i64>, right: Option<i64>) -> GraphValueRow {
    GraphValueRow::from_owned_values(
        [left, right]
            .map(|value| {
                GraphValue::Scalar(value.map_or(CanonicalScalar::Null, CanonicalScalar::Int))
            })
            .to_vec(),
    )
}

#[test]
fn carried_properties_are_optional_on_operands_with_exact_bag_and_null_semantics() {
    let scores = [1_i64, 2, 2, 3];
    for (operator, reverse) in [
        ("=", "="),
        ("<>", "<>"),
        ("!=", "!="),
        ("<", ">"),
        (">", "<"),
        ("<=", ">="),
        (">=", "<="),
    ] {
        let mut expected = Vec::new();
        for a in scores {
            let mut matched = false;
            for b in scores {
                let keep = match operator {
                    "=" => a == b,
                    "<>" | "!=" => a != b,
                    "<" => a < b,
                    ">" => a > b,
                    "<=" => a <= b,
                    ">=" => a >= b,
                    _ => unreachable!(),
                };
                if keep {
                    expected.push((a, Some(b)));
                    matched = true;
                }
            }
            if !matched {
                expected.push((a, None));
            }
        }
        expected.sort_by_key(|&(a, b)| (a, b.unwrap_or(i64::MAX)));
        let expected: Vec<_> = expected
            .into_iter()
            .map(|(a, b)| nullable_pair(Some(a), b))
            .collect();
        for predicate in [format!("a.p {operator} b.p"), format!("b.p {reverse} a.p")] {
            let text = format!(
                "MATCH (a) WITH a OPTIONAL MATCH (b) WHERE {predicate} \
                 RETURN a.p AS x,b.p AS y ORDER BY x,y"
            );
            assert_eq!(execute(&text), expected, "{text}");
        }
    }
    assert_eq!(
        execute(
            "MATCH (a) WITH a OPTIONAL MATCH (b {p:a.p}) RETURN a.p AS x,b.p AS y ORDER BY x,y"
        ),
        [(1, 1), (2, 2), (2, 2), (2, 2), (2, 2), (3, 3)]
            .map(|(a, b)| nullable_pair(Some(a), Some(b)))
            .to_vec()
    );
    // Private computed map keys use the same join path; NULL/absence never
    // satisfies equality, and duplicate left occurrences are not deduplicated.
    assert_eq!(
        execute(
            "UNWIND [{p:2},{p:2},{p:NULL},{}] AS row WITH row \
                 OPTIONAL MATCH (b {p:row.p}) RETURN row.p AS x,b.p AS y ORDER BY x,y"
        ),
        [(Some(2), Some(2)); 4]
            .into_iter()
            .chain([(None, None); 2])
            .map(|(a, b)| nullable_pair(a, b))
            .collect::<Vec<_>>()
    );
}

#[test]
fn null_carried_bindings_survive_later_required_and_optional_parts() {
    for kind in ["MATCH", "OPTIONAL MATCH"] {
        let text = format!(
            "MATCH (n) WHERE n.p=3 OPTIONAL MATCH (n)-[:R]->(a) WITH a \
             {kind} (b) RETURN a.p AS x,b.p AS y ORDER BY y"
        );
        assert_eq!(
            execute(&text),
            [1, 2, 2, 3].map(|b| nullable_pair(None, Some(b))).to_vec(),
            "{text}"
        );
    }
    assert_eq!(
        execute(
            "MATCH (n) WHERE n.p=3 OPTIONAL MATCH (n)-[:R]->(a) WITH a \
                 OPTIONAL MATCH (b) WHERE a.p < b.p RETURN a.p AS x,b.p AS y"
        ),
        vec![nullable_pair(None, None)]
    );
    // Explicitly matching the carried NULL remains a failed required match.
    assert!(
        execute(
            "MATCH (n) WHERE n.p=3 OPTIONAL MATCH (n)-[:R]->(a) WITH a \
                     MATCH (a) RETURN a.p AS x"
        )
        .is_empty()
    );
}

#[test]
fn carried_property_grouping_keeps_nulls_and_completed_optional_occurrences() {
    assert_eq!(
        execute(
            "MATCH (a) WITH a OPTIONAL MATCH (b) WHERE a.p < b.p \
                 WITH a.p AS score,COUNT(b) AS matched RETURN score,matched ORDER BY score"
        ),
        [(1, 3), (2, 2), (3, 0)]
            .map(|(score, count)| nullable_pair(Some(score), Some(count)))
            .to_vec()
    );
    assert_eq!(
        execute(
            "MATCH (n) WHERE n.p=3 OPTIONAL MATCH (n)-[:R]->(a) WITH a \
                 MATCH (b) WITH a.p AS score,COUNT(*) AS rows RETURN score,rows"
        ),
        vec![nullable_pair(None, Some(4))]
    );
}

#[test]
fn carried_property_ordering_shadowing_and_private_columns_keep_the_visible_scope() {
    let query = prepare("MATCH (a) WITH a OPTIONAL MATCH (b {p:a.p}) RETURN *");
    assert_eq!(query.columns(), &["a", "b"]);
    assert!(
        run(&query, wide(), &mut || Ok(()))
            .unwrap()
            .value
            .iter()
            .all(|row| row.len() == 2)
    );
    let values = |text: &str| {
        execute(text)
            .into_iter()
            .map(|row| row.get(0).unwrap().as_scalar().cloned().unwrap())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        values(
            "MATCH (a) WITH a OPTIONAL MATCH (b) WHERE b.p>a.p \
                RETURN a.p AS x ORDER BY a.p DESC"
        ),
        [3, 2, 2, 1, 1, 1].map(CanonicalScalar::Int).to_vec()
    );
    for page in ["ORDER BY x", "ORDER BY a.p"] {
        assert_eq!(
            values(&format!(
                "MATCH (a) WITH a OPTIONAL MATCH (b) WHERE b.p>a.p \
                             RETURN DISTINCT a.p AS x {page}"
            )),
            [1, 2, 3].map(CanonicalScalar::Int).to_vec()
        );
    }
    assert_eq!(
        values(
            "MATCH (a) WITH a OPTIONAL MATCH (b) WHERE b.p>a.p \
                RETURN b.p AS a ORDER BY a"
        ),
        [2, 2, 3, 3, 3]
            .map(CanonicalScalar::Int)
            .into_iter()
            .chain([CanonicalScalar::Null])
            .collect::<Vec<_>>()
    );
    for text in [
        "MATCH (a) WITH a.p AS x OPTIONAL MATCH (b) RETURN a.p",
        "MATCH (a) WITH a OPTIONAL MATCH (b {p:a.p}) RETURN __fg_match_key_1",
        "MATCH (a) WITH a OPTIONAL MATCH (b {p:a.p}) RETURN __fg_match_input_0",
    ] {
        let mut calls = 0;
        assert!(
            PreparedGraphSetText::prepare(text, |kind, name| {
                calls += 1;
                symbols(kind, name)
            })
            .is_err(),
            "{text}"
        );
        assert_eq!(calls, 0);
    }
}

#[test]
fn carried_property_join_preserves_every_checkpoint_and_element_source_refusal() {
    let text = "MATCH (a) WITH a OPTIONAL MATCH (b) WHERE a.p>b.p \
                RETURN a.p AS x,b.p AS y ORDER BY x,y";
    let query = prepare(text);
    let mut calls = 0;
    let measured = run(&query, wide(), &mut || {
        calls += 1;
        Ok(())
    })
    .unwrap();
    let caps = [
        measured.rows.snapshot_records,
        measured.rows.result_rows,
        measured.evaluator.work_units,
        measured.evaluator.scratch_entries,
    ];
    assert_eq!(
        run(
            &query,
            GqlQueryPolicy::new(caps[0], caps[1], caps[2], caps[3]),
            &mut || Ok(())
        )
        .unwrap(),
        measured
    );
    for dimension in 0..4 {
        let mut limits = caps;
        limits[dimension] -= 1;
        assert!(
            run(
                &query,
                GqlQueryPolicy::new(limits[0], limits[1], limits[2], limits[3]),
                &mut || Ok(())
            )
            .is_err()
        );
    }
    for stop in 1..=calls {
        let mut seen = 0;
        let result = run(&query, wide(), &mut || {
            seen += 1;
            if seen == stop { Err(stop) } else { Ok(()) }
        });
        assert!(matches!(result, Err(GqlQueryError::Interrupted(value)) if value == stop));
        assert_eq!(seen, stop);
    }
    for suffix in ["", " LIMIT 0"] {
        let failure = run_reading(
            &prepare(&format!("{text}{suffix}")),
            wide(),
            &mut || Ok(()),
            true,
        )
        .unwrap_err();
        assert!(
            format!("{failure:?}").contains("element-read-error"),
            "{failure:?}"
        );
    }
}

/// A grouping WITH right after MATCH keeps its grouped keys' reads for the
/// scope after it, even when its aggregate reads a property or a path
/// function (`WITH p, avg(f.age) AS a RETURN p.name, a`; TCK Return6 [13]:
/// `WITH a, other, min(length(p)) AS len RETURN a.name ...`). aa92179b let
/// the boundary hoist scan that WITH's own items, and hoisting `b.p` or
/// `length(p)` replaced the reads with_graph_head had made for `a.p`.
#[test]
fn a_grouping_head_keeps_its_key_reads_beside_a_property_aggregate() {
    let ints = |x: i64, y: i64| {
        GraphValueRow::from_owned_values(vec![
            GraphValue::Scalar(CanonicalScalar::Int(x)),
            GraphValue::Scalar(CanonicalScalar::Int(y)),
        ])
    };
    // a = 1 reaches b = 2, 2, 3 (min p 2); a = 2 reaches 3 (p 2); a = 3
    // reaches 4 (p 3). Their own p is 1, 2, 2.
    assert_eq!(
        execute(
            "MATCH (a)-[:R]->(b) WITH a, min(b.p) AS m \
             RETURN a.p AS ap, m ORDER BY ap, m"
        ),
        vec![ints(1, 2), ints(2, 2), ints(2, 3)]
    );
}

/// openCypher existence predicates anywhere in a WHERE, under OR or NOT and
/// beside any other predicate (TCK Pattern1 [21], MatchWhere4 [2],
/// WithWhere4 [2]). The graph predicate takes them only as AND conjuncts, so
/// a refused root MATCH WHERE filters at an implicit `WITH *`, and an
/// explicit WITH's WHERE reads them the same way: each predicate is a hidden
/// Boolean column of that boundary over a Collect scope. An existence test
/// never multiplies rows, whatever its witness count (1 -R-> 2 twice).
#[test]
fn existence_predicates_combine_with_any_predicate_in_a_where() {
    let out = |n: u128| EDGES.iter().any(|&(from, _, _)| from == VId(n));
    let into = |n: u128| EDGES.iter().any(|&(_, _, to)| to == VId(n));
    let edge = |a: u128, b: u128| EDGES.iter().any(|&(x, _, y)| (x, y) == (VId(a), VId(b)));
    let p = |n: u128| [1, 2, 2, 3][n as usize - 1];
    let ids = |rows: Vec<GraphValueRow>| -> Vec<Vec<u128>> {
        let mut ids: Vec<_> = rows
            .iter()
            .map(|row| {
                row.values()
                    .iter()
                    .map(|cell| cell.as_vertex().expect("vertex output").0)
                    .collect()
            })
            .collect();
        ids.sort_unstable();
        ids
    };
    let singles = |keep: &dyn Fn(u128) -> bool| -> Vec<Vec<u128>> {
        (1..=4).filter(|&n| keep(n)).map(|n| vec![n]).collect()
    };
    for (text, expected) in [
        (
            "MATCH (n) WHERE (n)-[:R]->() OR (n)<-[:R]-() RETURN n",
            singles(&|n| out(n) || into(n)),
        ),
        (
            "MATCH (n) WHERE n.p = 1 OR NOT (n)-[:R]->() RETURN n",
            singles(&|n| p(n) == 1 || !out(n)),
        ),
        (
            "MATCH (n) WHERE EXISTS { MATCH (n)-[:R]->(m) WHERE m.p = 3 } OR n.p = 2 RETURN n",
            singles(&|n| (1..=4).any(|m| edge(n, m) && p(m) == 3) || p(n) == 2),
        ),
        (
            "MATCH (n) WHERE NOT (exists((n)<-[:R]-()) OR n.p = 3) RETURN n",
            singles(&|n| !(into(n) || p(n) == 3)),
        ),
    ] {
        assert_eq!(ids(execute(text)), expected, "{text}");
    }
    // Two correlated bindings, at a root MATCH and after a plain WITH.
    let mut pairs: Vec<Vec<u128>> = Vec::new();
    for a in 1..=4 {
        for b in 1..=4 {
            if (p(a) == 1 && edge(a, b)) || edge(b, a) {
                pairs.push(vec![a, b]);
            }
        }
    }
    for text in [
        "MATCH (a), (b) WHERE a.p = 1 AND (a)-[:R]->(b) OR (b)-[:R]->(a) RETURN a, b",
        "MATCH (a), (b) WITH a, b WHERE a.p = 1 AND (a)-[:R]->(b) OR (b)-[:R]->(a) \
         RETURN a, b",
    ] {
        assert_eq!(ids(execute(text)), pairs, "{text}");
    }
    // Edge-shaped arithmetic stays arithmetic: `(x) - -1` is x + 1.
    assert_eq!(
        ids(execute(
            "MATCH (n) WITH n, n.p AS x WHERE (x) - -1 > 2 OR (n)<-[:R]-() RETURN n"
        )),
        singles(&|n| p(n) + 1 > 2 || into(n))
    );
    // The hidden columns are never output columns.
    let star = execute("MATCH (n) WHERE (n)-[:R]->() OR n.p = 3 RETURN *");
    assert!(star.iter().all(|row| row.len() == 1), "{star:?}");
    assert_eq!(ids(star), singles(&|n| out(n) || p(n) == 3));
    // The same root WHERE before a later part (LDBC SNB IC10's `WHERE NOT
    // friend = person AND NOT (friend)-[:KNOWS]-(person) ... OPTIONAL MATCH`)
    // filters the first part's rows before the later part joins them, as
    // the explicit `WITH * WHERE` does. Vertex 4 is the one with p = 3.
    let cases: [(&str, &dyn Fn(u128) -> bool); 4] = [
        ("NOT n.p = 1 AND NOT (n)-[:R]->()", &|n| {
            p(n) != 1 && !out(n)
        }),
        ("n.p = 2 OR (n)<-[:R]-()", &|n| p(n) == 2 || into(n)),
        ("(n)-[:R]->() OR n.p = 2", &|n| out(n) || p(n) == 2),
        ("n.p = 1 OR exists((n)<-[:R]-())", &|n| p(n) == 1 || into(n)),
    ];
    for (predicate, keep) in cases {
        let later = "WITH n MATCH (m {p: 3}) RETURN n, m";
        let implicit = format!("MATCH (n) WHERE {predicate} {later}");
        let explicit = format!("MATCH (n) WITH * WHERE {predicate} {later}");
        let expected: Vec<Vec<u128>> = (1..=4).filter(|&n| keep(n)).map(|n| vec![n, 4]).collect();
        assert_eq!(ids(execute(&implicit)), expected, "{implicit}");
        assert_eq!(ids(execute(&explicit)), expected, "{explicit}");
        // The explicit WITH's own WHERE before a later MATCH part, with no
        // further WITH: the head's existence columns are typed as the lists
        // they are (projection_type), so the part's projection admits them.
        let direct = format!("MATCH (n) WITH n WHERE {predicate} MATCH (m {{p: 3}}) RETURN n, m");
        assert_eq!(ids(execute(&direct)), expected, "{direct}");
        let optional =
            format!("MATCH (n) WHERE {predicate} OPTIONAL MATCH (n)-[:R]->(m) RETURN DISTINCT n");
        assert_eq!(ids(execute(&optional)), singles(keep), "{optional}");
    }
    // A shadowing alias is correlated after the WITH projects its identity.
    // The two parallel 1->2 edges still contribute two outer occurrences.
    assert_eq!(
        ids(execute(
            "MATCH (a)-[:R]->(b) WITH b AS a WHERE (a)-[:R]->() OR a.p = 1 RETURN a"
        )),
        vec![vec![2], vec![2], vec![3], vec![3]]
    );
    // A dropped binding has no carried identity and remains a typed refusal.
    assert!(
        PreparedGraphSetText::prepare(
            "MATCH (a)-[:R]->(b) WITH a WHERE (b)-[:R]->() OR a.p = 1 RETURN a",
            symbols,
        )
        .is_err()
    );
}

/// A pattern comprehension among a grouping WITH's items (fgdb-1f8f7, TCK
/// Pattern2 [8], [9]): the WITH's graph head gathers it into a hidden list
/// column, as a RETURN's head does, and the grouping reads that column, as a
/// key or inside an aggregate's argument. The path-capturing form needs edge
/// identities: multipart_optional_reads holds it.
#[test]
fn a_grouping_with_reads_pattern_comprehensions_its_head_gathers() {
    let p = |n: u128| [1, 2, 2, 3][n as usize - 1];
    let int = |n: i64| GraphValue::Scalar(CanonicalScalar::Int(n));
    let row = |cells: Vec<GraphValue>| GraphValueRow::from_owned_values(cells);
    let sorted = |mut rows: Vec<GraphValueRow>| {
        rows.sort();
        rows
    };
    // Inside an aggregate's argument: every edge once.
    assert_eq!(
        execute("MATCH (n) WITH sum(size([(n)-->() | 1])) AS total RETURN total"),
        vec![row(vec![int(EDGES.len() as i64)])]
    );
    // A projection reading the pattern's own local: each n's target values,
    // grouped (vertex 4 has none: the empty list is its key).
    let mut expected: Vec<GraphValueRow> = (1..=4)
        .map(|n| {
            let targets: Vec<GraphValue> = EDGES
                .iter()
                .filter(|&&(from, _, _)| from == VId(n))
                .map(|&(_, _, to)| int(p(to.0)))
                .collect();
            row(vec![
                GraphValue::List(targets.into_boxed_slice()),
                GraphValue::Scalar(CanonicalScalar::Int(1)),
            ])
        })
        .collect();
    expected.sort();
    let grouped = execute("MATCH (n) WITH [(n)-->(m) | m.p] AS ps, count(*) AS c RETURN ps, c");
    let grouped: Vec<GraphValueRow> = grouped
        .into_iter()
        .map(|row| {
            let cells: Vec<GraphValue> = row
                .values()
                .iter()
                .map(|cell| match cell.as_scalar() {
                    // count(*) delivers a count cell; compare as an integer.
                    Some(CanonicalScalar::Int(n)) => int(*n),
                    _ => cell.clone(),
                })
                .collect();
            GraphValueRow::from_owned_values(cells)
        })
        .collect();
    assert_eq!(sorted(grouped), expected);
}

/// A pattern comprehension anywhere in a WITH's row scope, its WHERE, pages
/// and RETURN, in the first UNWIND after a MATCH, or in a root MATCH's WHERE
/// at an implicit `WITH *` (fgdb-1f8f7): the head gathers it into a hidden
/// list column, once per row, which the stage reads where it stands.
/// List-local graph expressions and quantifier predicates use their own
/// correlated bodies; reduce locals retain their existing typed refusal.
/// A binding the row does not carry under its own name remains unavailable.
#[test]
fn a_row_scope_reads_pattern_comprehensions_its_head_gathers() {
    let out = |n: u128| EDGES.iter().filter(|&&(from, _, _)| from == VId(n)).count() as i64;
    let p = |n: u128| [1, 2, 2, 3][n as usize - 1];
    let int = |n: i64| GraphValue::Scalar(CanonicalScalar::Int(n));
    let sorted = |mut rows: Vec<GraphValueRow>| {
        rows.sort();
        rows
    };
    let by_degree = |keep: &dyn Fn(i64) -> bool| -> Vec<GraphValueRow> {
        (1..=4)
            .filter(|&n| keep(out(n)))
            .map(|n| vertices(&[n]))
            .collect()
    };
    assert_eq!(
        sorted(execute(
            "MATCH (n) WITH n WHERE size([(n)-->() | 1]) > 1 RETURN n"
        )),
        by_degree(&|degree| degree > 1)
    );
    assert_eq!(
        sorted(execute("MATCH (n) WHERE size([(n)-->() | 1]) = 1 RETURN n")),
        by_degree(&|degree| degree == 1)
    );
    // A page: degrees 3, 1, 1, 0, ties broken by identity.
    assert_eq!(
        sorted(execute(
            "MATCH (n) WITH n ORDER BY size([(n)-->() | 1]) DESC, n LIMIT 2 RETURN n"
        )),
        vec![vertices(&[1]), vertices(&[2])]
    );
    // A RETURN after the WITH, the comprehension reading its own local, and
    // one inside a list comprehension that reads no local of it.
    let expected: Vec<GraphValueRow> = (1..=4)
        .map(|n| {
            let targets: Vec<GraphValue> = EDGES
                .iter()
                .filter(|&&(from, _, _)| from == VId(n))
                .map(|&(_, _, to)| int(p(to.0)))
                .collect();
            GraphValueRow::from_owned_values(vec![
                GraphValue::Vertex(VId(n)),
                GraphValue::List(targets.into_boxed_slice()),
                GraphValue::List(Box::new([int(1 + out(n)), int(2 + out(n))])),
            ])
        })
        .collect();
    assert_eq!(
        sorted(execute(
            "MATCH (n) WITH n RETURN n, [(n)-->(m) | m.p] AS ps, \
             [x IN [1, 2] | x + size([(n)-->() | 1])] AS shifted"
        )),
        expected
    );
    // A plain WITH's own WHERE reads a MATCH binding it drops, as its
    // property reads do: the sources of edges into a vertex with no out-edge.
    let mut sinks: Vec<GraphValueRow> = EDGES
        .iter()
        .filter(|&&(_, _, to)| out(to.0) == 0)
        .map(|&(from, _, _)| vertices(&[from.0]))
        .collect();
    sinks.sort();
    assert_eq!(
        sorted(execute(
            "MATCH (n)-->(m) WITH n WHERE size([(m)-->() | 1]) = 0 RETURN n"
        )),
        sinks
    );
    // The first UNWIND after a MATCH: one row per out-edge.
    let mut pairs: Vec<GraphValueRow> = EDGES
        .iter()
        .map(|&(from, _, to)| vertices(&[from.0, to.0]))
        .collect();
    pairs.sort();
    assert_eq!(
        sorted(execute("MATCH (n) UNWIND [(n)-->(m) | m] AS t RETURN n, t")),
        pairs
    );
    for text in [
        "MATCH (n) WITH reduce(a = 0, x IN [n] | a + size([(x)-->() | 1])) AS l, \
         count(*) AS c RETURN l, c",
        // A binding the row drops.
        "MATCH (n)-->(m) WITH n RETURN [(m)-->() | 1] AS l",
    ] {
        assert!(
            PreparedGraphSetText::prepare(text, symbols).is_err(),
            "{text}"
        );
    }
}

/// `COUNT { ... }`, a count subquery in either form EXISTS takes, is the
/// size of a Collect scope's witness list, one entry per witness: read in
/// place by a RETURN's head, and gathered by a WITH head for its WHERE, pages
/// and RETURN, by a grouping WITH for its items, and by the implicit `WITH *`
/// of a MATCH WHERE. The parallel 1 -R-> 2 edges are two witnesses.
/// `size((n)-->())` stays refused, as the openCypher TCK requires (List6
/// [6]).
#[test]
fn pattern_counts_read_witness_list_sizes() {
    let out = |n: u128| EDGES.iter().filter(|&&(from, _, _)| from == VId(n)).count() as i64;
    let p = |n: u128| [1, 2, 2, 3][n as usize - 1];
    let into_high = |n: u128| {
        EDGES
            .iter()
            .filter(|&&(from, _, to)| from == VId(n) && p(to.0) > 2)
            .count() as i64
    };
    let int = |n: i64| GraphValue::Scalar(CanonicalScalar::Int(n));
    let sorted = |mut rows: Vec<GraphValueRow>| {
        rows.sort();
        rows
    };
    let per_vertex = |value: &dyn Fn(u128) -> i64| -> Vec<GraphValueRow> {
        (1..=4)
            .map(|n| {
                GraphValueRow::from_owned_values(vec![GraphValue::Vertex(VId(n)), int(value(n))])
            })
            .collect()
    };
    for (text, expected) in [
        (
            "MATCH (n) RETURN n, COUNT { (n)-->() } AS d",
            per_vertex(&out),
        ),
        (
            "MATCH (n) RETURN n, COUNT { MATCH (n)-->(m) WHERE m.p > 2 } AS d",
            per_vertex(&into_high),
        ),
        (
            "MATCH (n) RETURN n, COUNT { (n)-->() } + 1 AS d",
            per_vertex(&|n| out(n) + 1),
        ),
        (
            "MATCH (n) WITH n RETURN n, COUNT { (n)-->() } * 2 AS d",
            per_vertex(&|n| out(n) * 2),
        ),
    ] {
        assert_eq!(sorted(execute(text)), expected, "{text}");
    }
    let with_degree = |keep: &dyn Fn(i64) -> bool| -> Vec<GraphValueRow> {
        (1..=4)
            .filter(|&n| keep(out(n)))
            .map(|n| vertices(&[n]))
            .collect()
    };
    assert_eq!(
        sorted(execute("MATCH (n) WHERE COUNT { (n)-->() } > 1 RETURN n")),
        with_degree(&|degree| degree > 1)
    );
    assert_eq!(
        sorted(execute(
            "MATCH (n) WITH n WHERE COUNT { (n)-->() } = 1 RETURN n"
        )),
        with_degree(&|degree| degree == 1)
    );
    assert_eq!(
        execute("MATCH (n) WITH n ORDER BY COUNT { (n)-->() } DESC, n LIMIT 1 RETURN n"),
        vec![vertices(&[1])]
    );
    // A grouping key: degrees 3, 1, 1 and 0.
    let mut groups: Vec<(i64, i64)> = Vec::new();
    for n in 1..=4 {
        match groups.iter_mut().find(|(degree, _)| *degree == out(n)) {
            Some((_, members)) => *members += 1,
            None => groups.push((out(n), 1)),
        }
    }
    let grouped: Vec<GraphValueRow> = groups
        .into_iter()
        .map(|(degree, members)| GraphValueRow::from_owned_values(vec![int(degree), int(members)]))
        .collect();
    assert_eq!(
        sorted(execute(
            "MATCH (n) WITH COUNT { (n)-->() } AS d, collect(n) AS ns RETURN d, size(ns) AS k"
        )),
        sorted(grouped)
    );
    // The later k is the carried source n, not the old target k. Preserve
    // one count per matched edge occurrence, including the parallel pair.
    assert_eq!(
        sorted(execute(
            "MATCH (n)-->(k) WITH n AS k RETURN COUNT { (k)-->() } AS l"
        )),
        sorted(
            EDGES
                .iter()
                .map(|&(from, _, _)| GraphValueRow::from_owned_values(vec![int(out(from.0))]))
                .collect()
        )
    );
    // An enclosing element local reads through its list-local graph owner
    // (68812ac2): one count per member, parallel witnesses included.
    assert_eq!(
        sorted(execute(
            "MATCH (n) WITH n RETURN n, [x IN [n] | COUNT { (x)-->() }] AS l"
        )),
        (1..=4)
            .map(|n| GraphValueRow::from_owned_values(vec![
                GraphValue::Vertex(VId(n)),
                GraphValue::List(vec![int(out(n))].into()),
            ]))
            .collect::<Vec<_>>()
    );
    for text in [
        // A pattern expression inside size() (openCypher TCK List6 [6]).
        "MATCH (n) RETURN n, size((n)-->()) AS d",
        "MATCH (n) WHERE size((n)-->()) > 1 RETURN n",
    ] {
        assert!(
            PreparedGraphSetText::prepare(text, symbols).is_err(),
            "{text}"
        );
    }
}

/// `x IN <computed list>` in a WHERE: a list comprehension (`[x IN list |
/// e]`, TCK List12 [6]) or a pattern comprehension after IN is one list
/// value, never a literal of members, at a root MATCH's WHERE (its implicit
/// `WITH *`) and after a WITH.
#[test]
fn a_where_membership_reads_a_computed_list() {
    let p = |n: u128| [1, 2, 2, 3][n as usize - 1];
    let sorted = |mut rows: Vec<GraphValueRow>| {
        rows.sort();
        rows
    };
    // n.p is m.p or twice m.p, edge by edge.
    let pairs = sorted(
        EDGES
            .iter()
            .filter(|&&(n, _, m)| [p(m.0), 2 * p(m.0)].contains(&p(n.0)))
            .map(|&(n, _, m)| vertices(&[n.0, m.0]))
            .collect(),
    );
    assert!(!pairs.is_empty());
    for text in [
        "MATCH (n)-->(m) WHERE n.p IN [x IN [1, 2] | x * m.p] RETURN n, m",
        "MATCH (n)-->(m) WITH n, m WHERE n.p IN [x IN [1, 2] | x * m.p] RETURN n, m",
    ] {
        assert_eq!(sorted(execute(text)), pairs, "{text}");
    }
    // A pattern comprehension: the vertices with an out-neighbour of p 2.
    let expected = sorted(
        (1..=4)
            .filter(|&n| {
                EDGES
                    .iter()
                    .any(|&(from, _, to)| from == VId(n) && p(to.0) == 2)
            })
            .map(|n| vertices(&[n]))
            .collect(),
    );
    assert_eq!(
        sorted(execute(
            "MATCH (n) WITH n WHERE 2 IN [(n)-->(m) | m.p] RETURN n"
        )),
        expected
    );
    // A literal list keeps its member-by-member reading.
    assert_eq!(
        sorted(execute("MATCH (n) WITH n WHERE n.p IN [1, 3] RETURN n")),
        vec![vertices(&[1]), vertices(&[4])]
    );
}

/// A plain RETURN's ORDER BY over a count subquery or a pattern
/// comprehension (`ORDER BY COUNT { (n)-->() } DESC`, top-N by degree): the
/// RETURN's head gathers each into a private output, which the page sorts by
/// and then projects away. Degrees are 3, 1, 1 and 0; ties fall to the next
/// key. Under DISTINCT, and for a name an output alias shadows, it refuses.
#[test]
fn a_return_page_orders_by_pattern_counts() {
    let out = |n: u128| EDGES.iter().filter(|&&(from, _, _)| from == VId(n)).count();
    let mut by_degree: Vec<u128> = (1..=4).collect();
    by_degree.sort_by_key(|&n| (core::cmp::Reverse(out(n)), n));
    assert_eq!(
        execute("MATCH (n) RETURN n ORDER BY COUNT { (n)-->() } DESC, n"),
        by_degree
            .iter()
            .map(|&n| vertices(&[n]))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        execute("MATCH (n) RETURN n ORDER BY size([(n)-->(m) | m]) DESC, n LIMIT 2"),
        by_degree[..2]
            .iter()
            .map(|&n| vertices(&[n]))
            .collect::<Vec<_>>()
    );
    for text in [
        "MATCH (n) RETURN DISTINCT n.p AS p ORDER BY COUNT { (n)-->() }",
        "MATCH (n), (k) RETURN k AS n ORDER BY COUNT { (n)-->() }",
    ] {
        assert!(
            PreparedGraphSetText::prepare(text, symbols).is_err(),
            "{text}"
        );
    }
}

/// openCypher names a parameter by a decimal integer as well as by an
/// identifier (`$1`, TCK MatchWhere2 [2], WithWhere2 [2]): the statement
/// lexer reads `$1` as the parameter `1`, and the argument `1` binds it.
#[test]
fn a_parameter_may_be_named_by_a_decimal_integer() {
    let p = |n: u128| [1, 2, 2, 3][n as usize - 1];
    let query = PreparedGraphSetText::prepare(
        "MATCH (n) WHERE n.p = $1 OR n.p = $2 RETURN n ORDER BY n",
        symbols,
    )
    .unwrap()
    .bind_parameters(
        &GqlParameters::new()
            .with_int64("1", 1)
            .unwrap()
            .with_int64("2", 3)
            .unwrap(),
    )
    .unwrap();
    let expected: Vec<GraphValueRow> = (1..=4)
        .filter(|&n| [1, 3].contains(&p(n)))
        .map(|n| vertices(&[n]))
        .collect();
    assert_eq!(run(&query, wide(), &mut || Ok(())).unwrap().value, expected);
    // A name still never carries its `$`.
    assert!(GqlParameters::new().with_int64("$1", 1).is_err());
}

/// A carried vertex read past its boundary's scope, after an UNWIND or a
/// later WITH (`MATCH (a)-->(b) WITH a, collect(b) AS bs UNWIND bs AS x
/// RETURN a.p, x.p`): no hidden boundary column holds `a.p` there, so the
/// element reader reads it, as it reads `x.p` of any element value. Inside
/// the boundary's own scope the hidden column still serves.
#[test]
fn a_carried_element_reads_its_properties_past_an_unwind() {
    let p = |n: u128| [1, 2, 2, 3][n as usize - 1];
    let int = |n: i64| GraphValue::Scalar(CanonicalScalar::Int(n));
    let row = |cells: Vec<GraphValue>| GraphValueRow::from_owned_values(cells);
    let sorted = |mut rows: Vec<GraphValueRow>| {
        rows.sort();
        rows
    };
    let unwound = sorted(
        (1..=4)
            .flat_map(|n| [1, 2].map(|i| row(vec![int(p(n)), int(i)])))
            .collect(),
    );
    assert_eq!(
        sorted(execute(
            "MATCH (n) WITH n UNWIND [1, 2] AS i RETURN n.p AS p, i"
        )),
        unwound
    );
    let pairs = sorted(
        EDGES
            .iter()
            .map(|&(a, _, b)| row(vec![int(p(a.0)), int(p(b.0))]))
            .collect(),
    );
    for text in [
        "MATCH (a)-[:R]->(b) WITH a, collect(b) AS bs UNWIND bs AS x RETURN a.p AS ap, x.p AS xp",
        "MATCH (a)-[:R]->(b) WITH a, collect(b) AS bs UNWIND bs AS x WITH a, x \
         RETURN a.p AS ap, x.p AS xp",
    ] {
        assert_eq!(sorted(execute(text)), pairs, "{text}");
    }
    let high = sorted(
        (1..=4)
            .filter(|&n| p(n) > 1)
            .map(|n| row(vec![int(p(n))]))
            .collect(),
    );
    assert_eq!(
        sorted(execute(
            "MATCH (n) WITH n UNWIND [1] AS i WITH n, i WHERE n.p > 1 RETURN n.p AS p"
        )),
        high
    );
    // A later stage after a grouping keeps the grouped key a vertex value.
    let every = sorted((1..=4).map(|n| row(vec![int(p(n))])).collect());
    assert_eq!(
        sorted(execute(
            "MATCH (n) WITH n, count(*) AS c WITH n, c RETURN n.p AS p"
        )),
        every
    );
    // The boundary's own RETURN keeps its hidden column.
    assert_eq!(sorted(execute("MATCH (n) WITH n RETURN n.p AS p")), every);
}

/// A pattern that the scope after a grouping WITH reads over that WITH's
/// bare keys alone is a function of each group: the graph head before the
/// WITH gathers it and the WITH carries it as a hidden key, so the groups and
/// their counts are unchanged (LDBC SNB IC7's `NOT ((liker)-[:KNOWS]-(person))
/// AS isNew` after `WITH liker, ..., person`). It takes the bare spelling in
/// a Boolean position, `exists(...)` and `COUNT { ... }`.
#[test]
fn a_pattern_over_grouping_keys_reads_past_the_grouping_with() {
    let int = |n: i64| GraphValue::Scalar(CanonicalScalar::Int(n));
    let out = |n: u128| EDGES.iter().filter(|&&(s, _, _)| s == VId(n)).count() as i64;
    // Every (a, b) an edge joins, with its number of parallel edges.
    let mut paths: std::collections::BTreeMap<(u128, u128), i64> = Default::default();
    for &(a, _, b) in &EDGES {
        *paths.entry((a.0, b.0)).or_default() += 1;
    }
    let as_int = |rows: Vec<GraphValueRow>| {
        let mut rows: Vec<GraphValueRow> = rows
            .into_iter()
            .map(|row| {
                GraphValueRow::from_owned_values(
                    row.values()
                        .iter()
                        .map(|cell| match cell.as_scalar() {
                            Some(CanonicalScalar::Int(n)) => int(*n),
                            _ => cell.clone(),
                        })
                        .collect(),
                )
            })
            .collect();
        rows.sort();
        rows
    };
    let expected = |value: &dyn Fn(u128, u128) -> GraphValue| {
        as_int(
            paths
                .iter()
                .map(|(&(a, b), &n)| {
                    GraphValueRow::from_owned_values(vec![
                        GraphValue::Vertex(VId(a)),
                        GraphValue::Vertex(VId(b)),
                        int(n),
                        value(a, b),
                    ])
                })
                .collect(),
        )
    };
    // Vertex 4 has no out-edge; every other end vertex has one.
    let sink = expected(&|_, b| GraphValue::Scalar(CanonicalScalar::Bool(out(b) == 0)));
    for text in [
        "MATCH (a)-[:R]->(b) WITH a, b ORDER BY b.p WITH a, b, count(*) AS n \
         RETURN a, b, n, NOT ((b)-[:R]->()) AS sink",
        "MATCH (a)-[:R]->(b) WITH a, b ORDER BY b.p WITH a, b, count(*) AS n \
         RETURN a, b, n, NOT exists((b)-[:R]->()) AS sink",
    ] {
        assert_eq!(as_int(execute(text)), sink, "{text}");
    }
    assert_eq!(
        as_int(execute(
            "MATCH (a)-[:R]->(b) WITH a, b WITH a, b, count(*) AS n \
             RETURN a, b, n, COUNT { (b)-[:R]->() } AS out"
        )),
        expected(&|_, b| int(out(b)))
    );
}

/// An existence test read as a value, `exists((n)-->())` or `EXISTS { ... }`:
/// the size of the pattern's witness list being positive, as `COUNT { ... }`
/// reads its size. It answers in a RETURN, a WITH item, a CASE condition,
/// under NOT, as a grouping key and as an ORDER BY key; a WHERE keeps
/// reading it as its existence predicate.
#[test]
fn existence_tests_read_as_values() {
    let p = |n: u128| [1, 2, 2, 3][n as usize - 1];
    let out = |n: u128| EDGES.iter().any(|&(from, _, _)| from == VId(n));
    let into = |n: u128| EDGES.iter().any(|&(_, _, to)| to == VId(n));
    let high = |n: u128| {
        EDGES
            .iter()
            .any(|&(from, _, to)| from == VId(n) && p(to.0) > 2)
    };
    let bool_value = |b: bool| GraphValue::Scalar(CanonicalScalar::Bool(b));
    let int = |n: i64| GraphValue::Scalar(CanonicalScalar::Int(n));
    let sorted = |mut rows: Vec<GraphValueRow>| {
        rows.sort();
        rows
    };
    let per_vertex = |value: &dyn Fn(u128) -> GraphValue| -> Vec<GraphValueRow> {
        sorted(
            (1..=4)
                .map(|n| {
                    GraphValueRow::from_owned_values(vec![GraphValue::Vertex(VId(n)), value(n)])
                })
                .collect(),
        )
    };
    for (text, expected) in [
        (
            "MATCH (n) RETURN n, exists((n)-->()) AS b",
            per_vertex(&|n| bool_value(out(n))),
        ),
        (
            "MATCH (n) RETURN n, EXISTS { (n)<--() } AS b",
            per_vertex(&|n| bool_value(into(n))),
        ),
        (
            "MATCH (n) RETURN n, NOT exists((n)-->()) AS b",
            per_vertex(&|n| bool_value(!out(n))),
        ),
        (
            "MATCH (n) RETURN n, EXISTS { MATCH (n)-->(m) WHERE m.p > 2 } AS b",
            per_vertex(&|n| bool_value(high(n))),
        ),
        (
            "MATCH (n) WITH n, exists((n)-->()) AS o RETURN n, o AS b",
            per_vertex(&|n| bool_value(out(n))),
        ),
        (
            "MATCH (n) RETURN n, CASE WHEN exists((n)<--()) THEN 1 ELSE 0 END AS b",
            per_vertex(&|n| int(i64::from(into(n)))),
        ),
    ] {
        assert_eq!(sorted(execute(text)), expected, "{text}");
    }
    let members = |b: bool| (1..=4).filter(|&n| out(n) == b).count() as i64;
    assert_eq!(
        sorted(execute(
            "MATCH (n) WITH exists((n)-->()) AS o, collect(n) AS ns RETURN o, size(ns) AS k"
        )),
        sorted(
            [false, true]
                .map(|b| GraphValueRow::from_owned_values(vec![bool_value(b), int(members(b))]))
                .into_iter()
                .filter(|row| row.values()[1] != int(0))
                .collect()
        )
    );
    let mut ordered: Vec<u128> = (1..=4).collect();
    ordered.sort_by_key(|&n| (out(n), n));
    assert_eq!(
        execute("MATCH (n) RETURN n ORDER BY exists((n)-->()), n"),
        ordered.iter().map(|&n| vertices(&[n])).collect::<Vec<_>>()
    );
    assert_eq!(
        sorted(execute("MATCH (n) WITH n WHERE exists((n)-->()) RETURN n")),
        sorted(
            (1..=4)
                .filter(|&n| out(n))
                .map(|n| vertices(&[n]))
                .collect()
        )
    );
}

/// `OPTIONAL MATCH ... WHERE w`, where `w` reads the incoming row in a way the
/// graph predicate cannot place, filters inside an optional Apply: each
/// incoming row keeps the matches `w` holds for, or gets one NULL extension
/// when none does. A filter after the left join would drop such a row
/// instead (fgdb-2ht8c, LDBC SNB IC5's `WHERE friend IN friends`).
#[test]
fn an_optional_match_where_over_imported_values_filters_before_null_extension() {
    let p = |n: u128| [1, 2, 2, 3][n as usize - 1];
    let sorted = |mut rows: Vec<GraphValueRow>| {
        rows.sort();
        rows
    };
    let pair = |a: u128, b: Option<u128>| {
        GraphValueRow::from_owned_values(vec![
            GraphValue::Vertex(VId(a)),
            b.map_or(GraphValue::Scalar(CanonicalScalar::Null), |b| {
                GraphValue::Vertex(VId(b))
            }),
        ])
    };
    // Each of a's out-edge ends that `keep` admits, or one NULL extension.
    let expected = |keep: &dyn Fn(u128, u128) -> bool| {
        sorted(
            (1..=4)
                .flat_map(|a| {
                    let ends: Vec<_> = EDGES
                        .iter()
                        .filter(|&&(start, _, end)| start == VId(a) && keep(a, end.0))
                        .map(|&(_, _, end)| pair(a, Some(end.0)))
                        .collect();
                    if ends.is_empty() {
                        vec![pair(a, None)]
                    } else {
                        ends
                    }
                })
                .collect(),
        )
    };
    // Membership in a collected list of the vertices with p = 2.
    assert_eq!(
        sorted(execute(
            "MATCH (s) WHERE s.p = 2 WITH collect(s) AS ss MATCH (a) WITH a, ss \
             OPTIONAL MATCH (a)-[:R]->(b) WHERE b IN ss RETURN a, b"
        )),
        expected(&|_, b| p(b) == 2)
    );
    // A computed operand against a carried property.
    assert_eq!(
        sorted(execute(
            "MATCH (a) WITH a OPTIONAL MATCH (a)-[:R]->(b) WHERE b.p + 0 > a.p RETURN a, b"
        )),
        expected(&|a, b| p(b) > p(a))
    );
}

#[test]
fn ordered_collect_arguments_preserve_selected_child_rows_and_parallel_values() {
    let int = |value| GraphValue::Scalar(CanonicalScalar::Int(value));
    for (head, ids) in [
        ("MATCH (n) WITH n ORDER BY n DESC", vec![4, 3, 2, 1]),
        (
            "MATCH (n) WITH n ORDER BY n.p + 10 DESC,n DESC SKIP 1 LIMIT 2",
            vec![3, 2],
        ),
    ] {
        let statement = format!(
            "{head} WITH collect(n) AS nodes,collect(n.p + 10) AS values RETURN nodes,values"
        );
        let expected = GraphValueRow::from_owned_values(vec![
            GraphValue::List(ids.iter().map(|&id| GraphValue::Vertex(VId(id))).collect()),
            GraphValue::List(
                ids.iter()
                    .map(|&id| int([1, 2, 2, 3][id as usize - 1] + 10))
                    .collect(),
            ),
        ]);
        assert_eq!(execute(&statement), vec![expected], "{statement}");
    }
    // An unordered computed input retains its ordinary canonical projection
    // contract. Only a completed explicit ordering survives argument binding.
    assert_eq!(
        execute("MATCH (n) WITH n WITH collect(-n.p) AS values RETURN values"),
        vec![GraphValueRow::from_owned_values(vec![GraphValue::List(
            [-3, -2, -2, -1].into_iter().map(int).collect(),
        )])]
    );
}

#[test]
fn row_scope_graph_predicates_preserve_collected_occurrences_and_local_pages() {
    let head = "MATCH (n) WITH collect(n) AS nodes UNWIND nodes AS x";
    for predicate in [
        "(x)-[:R]->()",
        "EXISTS((x)-[:R]->())",
        "EXISTS((x)-->())",
        "EXISTS { MATCH (x)-[:R]->() }",
    ] {
        for negated in [false, true] {
            let negate = if negated { "NOT " } else { "" };
            let statement = format!("{head} WHERE {negate}{predicate} RETURN x ORDER BY x");
            let expected: Vec<_> = if negated { vec![4] } else { vec![1, 2, 3] }
                .into_iter()
                .map(|id| vertices(&[id]))
                .collect();
            assert_eq!(execute(&statement), expected, "{statement}");
        }
    }
    // Each compound leaf has a different absolute token offset. Its WHERE
    // probe must rebase against that leaf's actual statement slice.
    assert_eq!(
        execute(&format!(
            "\u{2003}({head} WHERE EXISTS((x)-[:R]->()) RETURN x) \
             UNION ALL ({head} WHERE NOT EXISTS((x)-[:R]->()) RETURN x) ORDER BY x"
        )),
        [1, 2, 3, 4]
            .into_iter()
            .map(|id| vertices(&[id]))
            .collect::<Vec<_>>()
    );
    // Vertex 1 has three physical witnesses. Existence must retain each
    // outer occurrence once, without multiplying it by those witnesses.
    let duplicated = "MATCH (n) WITH n ORDER BY n DESC WITH collect(n) AS nodes \
                      UNWIND nodes AS candidate UNWIND [candidate,candidate] AS x \
                      WHERE (x)-[:R]->()";
    assert_eq!(
        execute(&format!("{duplicated} RETURN x ORDER BY x DESC")),
        [3, 3, 2, 2, 1, 1]
            .into_iter()
            .map(|id| vertices(&[id]))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        execute(&format!("{duplicated} LIMIT 3 RETURN x ORDER BY x DESC")),
        [3, 3, 2]
            .into_iter()
            .map(|id| vertices(&[id]))
            .collect::<Vec<_>>()
    );
    let statement = "MATCH (n) WITH collect(n) AS nodes \
                     UNWIND nodes AS __fg_exists_witness \
                     WHERE EXISTS((__fg_exists_witness)-[:R]->()) \
                     RETURN __fg_exists_witness ORDER BY __fg_exists_witness";
    let query = prepare(statement);
    assert_eq!(query.columns(), ["__fg_exists_witness"]);
    assert_eq!(
        run(&query, wide(), &mut || Ok(())).unwrap().value,
        [1, 2, 3]
            .into_iter()
            .map(|id| vertices(&[id]))
            .collect::<Vec<_>>()
    );
}

#[test]
fn row_scope_graph_predicates_match_nullable_graph_head_existence() {
    let head = "MATCH (n) OPTIONAL MATCH (n)-[:R]->(m) WITH n,m";
    for negated in [false, true] {
        let negate = if negated { "NOT " } else { "" };
        // The unchanged graph-head owner already handles this nullable m.
        let oracle = execute(&format!(
            "{head} WHERE {negate}EXISTS((m)-[:R]->()) RETURN n,m ORDER BY n,m"
        ));
        let expected = if negated {
            vec![
                vertices(&[3, 4]),
                GraphValueRow::from_owned_values(vec![
                    GraphValue::Vertex(VId(4)),
                    GraphValue::Scalar(CanonicalScalar::Null),
                ]),
            ]
        } else {
            vec![
                vertices(&[1, 2]),
                vertices(&[1, 2]),
                vertices(&[1, 3]),
                vertices(&[2, 3]),
            ]
        };
        assert_eq!(oracle, expected);
        // UNWIND preserves the NULL inside [m], instead of rebinding that
        // missing identity to an arbitrary vertex in the predicate body.
        assert_eq!(
            execute(&format!(
                "{head} UNWIND [m] AS x WHERE {negate}EXISTS((x)-[:R]->()) \
                 RETURN n,x ORDER BY n,x"
            )),
            oracle,
        );
    }
}

#[test]
fn compound_row_graph_predicates_preserve_boolean_truth_bags_and_local_pages() {
    let head = "MATCH (n) WITH n ORDER BY n DESC WITH collect(n) AS nodes \
                UNWIND nodes AS candidate UNWIND [candidate,candidate] AS x";
    // Hand answers: outgoing identities are 1,2,3; incoming identities are
    // 2,3,4; p is 1,2,2,3. Parallel witnesses never multiply an outer row.
    for (predicate, expected) in [
        ("x.p = 2 AND EXISTS((x)-[:R]->())", vec![3, 3, 2, 2]),
        (
            "EXISTS((x)<-[:R]-()) OR x.p = 1",
            vec![4, 4, 3, 3, 2, 2, 1, 1],
        ),
        (
            "EXISTS((x)-[:R]->()) XOR EXISTS((x)<-[:R]-())",
            vec![4, 4, 1, 1],
        ),
        (
            "NOT ((x)-[:R]->() AND EXISTS { MATCH (x)<-[:R]-() })",
            vec![4, 4, 1, 1],
        ),
        (
            "CASE WHEN EXISTS((x)-[:R]->()) THEN x.p = 2 ELSE x.p = 3 END",
            vec![4, 4, 3, 3, 2, 2],
        ),
        (
            "x.missing = 1 OR EXISTS((x)-[:R]->())",
            vec![3, 3, 2, 2, 1, 1],
        ),
        ("x.missing = 1 AND EXISTS((x)-[:R]->())", vec![]),
        ("NULL XOR EXISTS((x)-[:R]->())", vec![]),
        (
            "EXISTS((x)-[:R]->()) XOR EXISTS((x)<-[:R]-()) AND x.p = 2",
            vec![1, 1],
        ),
        (
            "CASE WHEN (x)-[:R]->() THEN x.p = 2 ELSE x.p = 3 END",
            vec![4, 4, 3, 3, 2, 2],
        ),
        (
            "EXISTS((x)-[:R]->(z)) XOR EXISTS((x)<-[:R]-(z))",
            vec![4, 4, 1, 1],
        ),
        (
            "EXISTS((x)-[:R]->(__fg_boundary_0)) AND EXISTS((x)-[:R]->(__fg_boundary_0))",
            vec![3, 3, 2, 2, 1, 1],
        ),
    ] {
        let text = format!("{head} WHERE {predicate} RETURN x ORDER BY x DESC");
        assert_eq!(
            execute(&text),
            expected
                .into_iter()
                .map(|id| vertices(&[id]))
                .collect::<Vec<_>>(),
            "{text}"
        );
    }
    assert_eq!(
        execute(&format!(
            "{head} WHERE EXISTS((x)-[:R]->()) XOR EXISTS((x)<-[:R]-()) \
             LIMIT 3 RETURN x ORDER BY x DESC"
        )),
        [4, 4, 1]
            .into_iter()
            .map(|id| vertices(&[id]))
            .collect::<Vec<_>>()
    );
    let query = prepare(
        "MATCH (n) WITH collect(n) AS nodes UNWIND nodes AS __fg_boundary_0 \
         WHERE __fg_boundary_0.p = 2 AND EXISTS((__fg_boundary_0)-[:R]->()) RETURN *",
    );
    // RETURN * lists its columns in name order (fgdb-7jja3).
    assert_eq!(query.columns(), ["__fg_boundary_0", "nodes"]);
    assert_eq!(run(&query, wide(), &mut || Ok(())).unwrap().value.len(), 2);
    for boundary in ["WITH DISTINCT x", "WITH x AS y"] {
        let name = if boundary.ends_with('y') { "y" } else { "x" };
        assert_eq!(
            execute(&format!(
                "MATCH (n) WITH collect(n) AS nodes UNWIND nodes AS x {boundary} \
                 WHERE {name}.p = 2 AND EXISTS(({name})-[:R]->()) RETURN {name} ORDER BY {name}"
            )),
            vec![vertices(&[2]), vertices(&[3])]
        );
    }
}

#[test]
fn compound_row_graph_predicates_keep_lazy_scalar_cases_and_private_scopes() {
    for (property, condition, expected) in [
        (
            1,
            "CASE WHEN EXISTS((x)-[:R]->()) THEN true ELSE 1 / 0 = 0 END",
            1,
        ),
        (
            3,
            "CASE WHEN EXISTS((x)-[:R]->()) THEN 1 / 0 = 0 ELSE true END",
            4,
        ),
    ] {
        let text = format!(
            "MATCH (n) WHERE n.p = {property} WITH collect(n) AS nodes UNWIND nodes AS x \
             WHERE {condition} RETURN x"
        );
        assert_eq!(execute(&text), vec![vertices(&[expected])], "{text}");
    }
    assert_eq!(
        execute(
            "MATCH (n) WITH collect(n) AS nodes UNWIND nodes AS x \
             WHERE COALESCE(EXISTS((x)-[:R]->()), 1 / 0 = 0) RETURN x ORDER BY x"
        ),
        [1, 2, 3]
            .into_iter()
            .map(|id| vertices(&[id]))
            .collect::<Vec<_>>()
    );
    for condition in [
        "EXISTS((x)-[:R]->()) AND __fg_boundary_0",
        "EXISTS((x)-[:R]->(z)) OR z.p = 2",
        "true OR EXISTS((fresh)-[:R]->())",
        "EXISTS((x)-[:R]->()) OR 1 = (x)-[:R]->()",
        "EXISTS((x)-[:R]->()) OR [(x)-[:R]->()] = [true]",
    ] {
        let text = format!(
            "MATCH (n) WITH collect(n) AS nodes UNWIND nodes AS x WHERE {condition} RETURN x"
        );
        let resolutions = std::cell::Cell::new(0);
        assert!(
            PreparedGraphSetText::prepare(&text, |kind, name| {
                resolutions.set(resolutions.get() + 1);
                symbols(kind, name)
            })
            .is_err(),
            "{text}"
        );
        assert_eq!(
            resolutions.get(),
            0,
            "scope refusal precedes catalog access: {text}"
        );
    }
}

#[test]
fn compound_row_graph_predicates_require_boolean_results() {
    for result in ["1", "[true]", "{value: true}"] {
        let text = format!(
            "MATCH (n) WITH collect(n) AS nodes UNWIND nodes AS x \
             WHERE CASE WHEN EXISTS((x)-[:R]->()) THEN {result} ELSE false END RETURN x"
        );
        if let Ok(template) = PreparedGraphSetText::prepare(&text, symbols)
            && let Ok(query) = template.bind_parameters(&GqlParameters::new())
        {
            assert!(run(&query, wide(), &mut || Ok(())).is_err(), "{text}");
        }
    }
    assert!(
        execute(
            "MATCH (n) WITH collect(n) AS nodes UNWIND nodes AS x \
             WHERE CASE WHEN EXISTS((x)-[:R]->()) THEN NULL ELSE false END RETURN x"
        )
        .is_empty()
    );
    // The same Any-typed row column can carry a valid Boolean, NULL, a
    // numeric scalar or a composite value without changing the template.
    let template = PreparedGraphSetText::prepare(
        "UNWIND $items AS x UNWIND $results AS result \
         WHERE CASE WHEN EXISTS((x)-[:R]->()) THEN result ELSE false END RETURN x",
        symbols,
    )
    .unwrap();
    for (result, expected) in [
        (GraphValue::Scalar(CanonicalScalar::Bool(true)), Some(true)),
        (GraphValue::Scalar(CanonicalScalar::Null), Some(false)),
        (GraphValue::Scalar(CanonicalScalar::Int(1)), None),
        (GraphValue::List(vec![].into()), None),
    ] {
        let args = GqlParameters::new()
            .with_list("items", vec![GraphValue::Vertex(VId(1))])
            .unwrap()
            .with_list("results", vec![result])
            .unwrap();
        let query = template.bind_parameters(&args).unwrap();
        let outcome = run(&query, wide(), &mut || Ok(()));
        match expected {
            Some(keep) => assert_eq!(
                outcome.unwrap().value,
                if keep { vec![vertices(&[1])] } else { vec![] }
            ),
            None => assert!(outcome.is_err()),
        }
    }
}

#[test]
fn compound_row_graph_predicates_rebind_multiple_captures_without_any_coercion() {
    let items = vec![
        GraphValue::Vertex(VId(1)),
        GraphValue::Vertex(VId(1)),
        GraphValue::Vertex(VId(4)),
        GraphValue::Vertex(VId(u128::MAX)),
        GraphValue::Scalar(CanonicalScalar::Null),
        GraphValue::Scalar(CanonicalScalar::Int(1)),
        GraphValue::Edge(fgdb_types::EId(1)),
    ];
    let targets = vec![
        GraphValue::Vertex(VId(2)),
        GraphValue::Vertex(VId(3)),
        GraphValue::Scalar(CanonicalScalar::Null),
    ];
    let text = "UNWIND $items AS x UNWIND $targets AS y \
                WHERE CASE WHEN $enabled = 1 THEN EXISTS((x)-[:R]->(y {p: $p})) \
                           ELSE false END \
                   OR (y IS NULL AND NOT EXISTS((x)-[:R]->())) RETURN x,y";
    let template = PreparedGraphSetText::prepare(text, symbols).unwrap();
    let frozen = template.canonical_template_bytes();
    for (enabled, property, input) in [
        (1, 2, items.clone()),
        (0, 2, items.clone()),
        (1, 3, items),
        (1, 2, vec![]),
    ] {
        let mut expected = Vec::new();
        for x in &input {
            for y in &targets {
                let outgoing = matches!(x, GraphValue::Vertex(id)
                    if EDGES.iter().any(|&(from, _, _)| from == *id));
                let pair = match (x, y) {
                    (GraphValue::Vertex(from), GraphValue::Vertex(to)) => {
                        enabled == 1
                            && [1, 2, 2, 3][to.0 as usize - 1] == property
                            && EDGES.iter().any(|&(a, _, b)| a == *from && b == *to)
                    }
                    _ => false,
                };
                let null_target = matches!(y, GraphValue::Scalar(CanonicalScalar::Null));
                if pair || (null_target && !outgoing) {
                    expected.push(GraphValueRow::from_owned_values(vec![x.clone(), y.clone()]));
                }
            }
        }
        expected.sort();
        let arguments = GqlParameters::new()
            .with_list("items", input)
            .unwrap()
            .with_list("targets", targets.clone())
            .unwrap()
            .with_int64("enabled", enabled)
            .unwrap()
            .with_int64("p", property)
            .unwrap();
        let query = template.bind_parameters(&arguments).unwrap();
        let mut actual = run(&query, wide(), &mut || Ok(())).unwrap().value;
        actual.sort();
        assert_eq!(actual, expected, "enabled={enabled}, p={property}");
    }
    assert_eq!(template.canonical_template_bytes(), frozen);
    assert!(template.bind_parameters(&GqlParameters::new()).is_err());
}

#[test]
fn compound_row_graph_predicates_keep_governed_failures_before_boolean_filtering() {
    for predicate in [
        "true OR EXISTS((x)-[:R]->())",
        "false AND EXISTS((x)-[:R]->())",
        "CASE WHEN true THEN true ELSE EXISTS((x)-[:R]->()) END",
    ] {
        for tail in ["RETURN x", "LIMIT 0 RETURN x", "RETURN x LIMIT 0"] {
            let query = prepare(&format!(
                "MATCH (n) WITH collect(n) AS nodes UNWIND nodes AS x WHERE {predicate} {tail}"
            ));
            let mut calls = 0;
            let result = query.execute_governed(
                wide(),
                |pattern, remaining| {
                    calls += 1;
                    if calls == 3 {
                        return Err(GqlQueryError::Source("second-predicate-source-error"));
                    }
                    pattern.plan().execute_governed_with_properties(
                        (4 + EDGES.len()) as u64,
                        (1..=4).map(VId),
                        EDGES,
                        |_, _| Ok::<_, &'static str>(true),
                        |_, _| Ok(None),
                        remaining,
                        || Ok::<_, usize>(()),
                    )
                },
                || Ok::<_, usize>(()),
            );
            assert_eq!(calls, 3, "{predicate}, {tail}");
            assert!(matches!(
                result,
                Err(GqlQueryError::Source(GraphSetExecutionError::Source(
                    "second-predicate-source-error"
                )))
            ));
        }
    }
    let query = prepare(
        "MATCH (n) WITH collect(n) AS nodes UNWIND nodes AS x \
         WHERE EXISTS((x)-[:R]->()) XOR EXISTS((x)<-[:R]-()) RETURN x LIMIT 0",
    );
    let mut checkpoints = 0;
    let measured = run(&query, wide(), &mut || {
        checkpoints += 1;
        Ok(())
    })
    .unwrap();
    let caps = [
        measured.rows.snapshot_records,
        measured.rows.result_rows,
        measured.evaluator.work_units,
        measured.evaluator.scratch_entries,
    ];
    assert_eq!(
        run(
            &query,
            GqlQueryPolicy::new(caps[0], caps[1], caps[2], caps[3]),
            &mut || Ok(()),
        )
        .unwrap(),
        measured
    );
    for dimension in [0, 2, 3] {
        let mut limited = caps;
        assert!(limited[dimension] > 0);
        limited[dimension] -= 1;
        assert!(
            run(
                &query,
                GqlQueryPolicy::new(limited[0], limited[1], limited[2], limited[3]),
                &mut || Ok(()),
            )
            .is_err(),
            "dimension {dimension}"
        );
    }
    for stop in 1..=checkpoints {
        let mut seen = 0;
        assert!(
            run(&query, wide(), &mut || {
                seen += 1;
                if seen == stop { Err(stop) } else { Ok(()) }
            })
            .is_err(),
            "checkpoint {stop}/{checkpoints}"
        );
    }
}

#[test]
fn row_scope_graph_predicate_arguments_rebind_without_coercing_any_values_to_vertices() {
    let mixed = vec![
        GraphValue::Vertex(VId(1)),
        GraphValue::Vertex(VId(1)),
        GraphValue::Vertex(VId(4)),
        GraphValue::Vertex(VId(u128::MAX)),
        GraphValue::Scalar(CanonicalScalar::Null),
        GraphValue::Scalar(CanonicalScalar::Int(1)),
        GraphValue::Edge(fgdb_types::EId(1)),
    ];
    for negated in [false, true] {
        let negate = if negated { "NOT " } else { "" };
        let statement = format!("UNWIND $items AS x WHERE {negate}EXISTS((x)-[:R]->()) RETURN x");
        let template = PreparedGraphSetText::prepare(&statement, symbols).unwrap();
        let frozen = template.canonical_template_bytes();
        for values in [
            mixed.clone(),
            vec![GraphValue::Vertex(VId(2)), GraphValue::Vertex(VId(3))],
            vec![],
        ] {
            let mut expected: Vec<_> = values
                .iter()
                .filter(|value| {
                    let witness = matches!(value, GraphValue::Vertex(id)
                        if EDGES.iter().any(|&(from, _, _)| from == *id));
                    witness != negated
                })
                .map(|value| GraphValueRow::from_owned_values(vec![value.clone()]))
                .collect();
            expected.sort();
            let arguments = GqlParameters::new().with_list("items", values).unwrap();
            let query = template.bind_parameters(&arguments).unwrap();
            let mut actual = run(&query, wide(), &mut || Ok(())).unwrap().value;
            actual.sort();
            assert_eq!(actual, expected, "{statement}");
        }
        assert_eq!(template.canonical_template_bytes(), frozen);
        assert!(template.bind_parameters(&GqlParameters::new()).is_err());
    }
}

#[test]
fn row_graph_predicate_routing_preserves_scalar_minus_comparisons_and_not() {
    for (predicate, ordinary) in [
        ("(x) - -1 > 2", "x + 1 > 2"),
        ("(x) - -(y) > 2", "x + y > 2"),
        ("(x) < - -3", "x < 3"),
        ("(x) < - -(y)", "x < y"),
        ("((x) - -1 > 2)", "x + 1 > 2"),
        ("NOT ((x) - -1 <= 2)", "x + 1 > 2"),
        ("NOT x = 3", "x <> 3"),
    ] {
        let statement = format!("WITH 2 AS x,3 AS y WHERE {predicate} RETURN x,y");
        let expected = execute(&format!("WITH 2 AS x,3 AS y WHERE {ordinary} RETURN x,y"));
        assert_eq!(expected.len(), 1, "{ordinary}");
        assert_eq!(execute(&statement), expected, "{statement}");
    }
}

#[test]
fn row_scope_graph_predicates_keep_late_body_errors_and_cumulative_budgets() {
    let head = "MATCH (n) WITH collect(n) AS nodes UNWIND nodes AS x \
                WHERE EXISTS((x)-[:R]->())";
    for tail in ["RETURN x", "LIMIT 0 RETURN x", "RETURN x LIMIT 0"] {
        let query = prepare(&format!("{head} {tail}"));
        let mut calls = 0;
        let result = query.execute_governed(
            wide(),
            |pattern, remaining| {
                calls += 1;
                if calls == 3 {
                    return Err(GqlQueryError::Source("second-predicate-source-error"));
                }
                pattern.plan().execute_governed_with_properties(
                    (4 + EDGES.len()) as u64,
                    (1..=4).map(VId),
                    EDGES,
                    |_, _| Ok::<_, &'static str>(true),
                    |_, _| Ok(None),
                    remaining,
                    || Ok::<_, usize>(()),
                )
            },
            || Ok::<_, usize>(()),
        );
        // The graph leaf is call 1, and the first body's complete three
        // witnesses are call 2. Neither its private LIMIT 1 nor an outer
        // empty page may hide a later body source refusal.
        assert_eq!(calls, 3, "{tail}");
        assert!(matches!(
            result,
            Err(GqlQueryError::Source(GraphSetExecutionError::Source(
                "second-predicate-source-error"
            )))
        ));
    }

    for tail in ["RETURN x", "RETURN x LIMIT 0"] {
        let query = prepare(&format!("{head} {tail}"));
        let measured = run(&query, wide(), &mut || Ok(())).unwrap();
        let caps = [
            measured.rows.snapshot_records,
            measured.rows.result_rows,
            measured.evaluator.work_units,
            measured.evaluator.scratch_entries,
        ];
        assert_eq!(
            run(
                &query,
                GqlQueryPolicy::new(caps[0], caps[1], caps[2], caps[3]),
                &mut || Ok(()),
            )
            .unwrap(),
            measured,
        );
        for dimension in [0, 2, 3] {
            let mut limited = caps;
            assert!(limited[dimension] > 0);
            limited[dimension] -= 1;
            assert!(
                run(
                    &query,
                    GqlQueryPolicy::new(limited[0], limited[1], limited[2], limited[3]),
                    &mut || Ok(()),
                )
                .is_err(),
                "{tail}, dimension {dimension}"
            );
        }
    }
}

#[test]
fn row_scope_existence_values_rebind_dynamic_anchors_without_losing_occurrences() {
    let template = PreparedGraphSetText::prepare(
        "UNWIND $items AS x RETURN x,EXISTS((x)-[:R]->()) AS outgoing,\
         NOT EXISTS((x)-[:R]->()) AS sink,\
         CASE WHEN EXISTS((x)<-[:R]-()) THEN 7 ELSE 9 END AS score",
        symbols,
    )
    .unwrap();
    let frozen = template.canonical_template_bytes();
    for items in [
        vec![
            GraphValue::Vertex(VId(1)),
            GraphValue::Vertex(VId(1)),
            GraphValue::Vertex(VId(4)),
            GraphValue::Vertex(VId(u128::MAX)),
            GraphValue::Scalar(CanonicalScalar::Null),
            GraphValue::Scalar(CanonicalScalar::Int(1)),
            GraphValue::Edge(fgdb_types::EId(1)),
            GraphValue::List(vec![].into()),
        ],
        vec![GraphValue::Vertex(VId(2)), GraphValue::Vertex(VId(3))],
        vec![],
    ] {
        let mut expected: Vec<_> = items
            .iter()
            .map(|item| {
                let outgoing = matches!(item, GraphValue::Vertex(id)
                    if EDGES.iter().any(|&(from, _, _)| from == *id));
                let incoming = matches!(item, GraphValue::Vertex(id)
                    if EDGES.iter().any(|&(_, _, to)| to == *id));
                GraphValueRow::from_owned_values(vec![
                    item.clone(),
                    GraphValue::Scalar(CanonicalScalar::Bool(outgoing)),
                    GraphValue::Scalar(CanonicalScalar::Bool(!outgoing)),
                    GraphValue::Scalar(CanonicalScalar::Int(if incoming { 7 } else { 9 })),
                ])
            })
            .collect();
        expected.sort();
        let arguments = GqlParameters::new().with_list("items", items).unwrap();
        let query = template.bind_parameters(&arguments).unwrap();
        assert_eq!(query.columns(), ["x", "outgoing", "sink", "score"]);
        let mut actual = run(&query, wide(), &mut || Ok(())).unwrap().value;
        actual.sort();
        assert_eq!(actual, expected);
    }
    assert_eq!(template.canonical_template_bytes(), frozen);
    assert!(template.bind_parameters(&GqlParameters::new()).is_err());
}

#[test]
fn row_scope_existence_projection_composes_booleans_and_keeps_private_names_private() {
    let prefix = "MATCH (n) WITH collect(n) AS nodes UNWIND nodes AS x";
    for (expression, expected) in [
        ("NOT ((x)-[:R]->())", [false, false, false, true]),
        (
            "x.p = 2 AND EXISTS((x)-[:R]->())",
            [false, true, true, false],
        ),
        (
            "EXISTS((x)-[:R]->()) XOR EXISTS { MATCH (x)<-[:R]-() }",
            [true, false, false, true],
        ),
        (
            "EXISTS((x)-[:R]->()) AND NOT ((x)<-[:R]-())",
            [true, false, false, false],
        ),
        (
            "COALESCE(EXISTS((x)-[:R]->()), 1 / 0 = 0)",
            [true, true, true, false],
        ),
        (
            "CASE WHEN EXISTS((x)-[:R]->()) THEN true ELSE x.p = 3 END",
            [true, true, true, true],
        ),
    ] {
        let statement = format!("{prefix} RETURN x,{expression} AS value ORDER BY x");
        assert_eq!(
            execute(&statement),
            expected
                .into_iter()
                .enumerate()
                .map(|(at, value)| GraphValueRow::from_owned_values(vec![
                    GraphValue::Vertex(VId(at as u128 + 1)),
                    GraphValue::Scalar(CanonicalScalar::Bool(value)),
                ]))
                .collect::<Vec<_>>(),
            "{statement}"
        );
    }
    let query = prepare(
        "MATCH (n) WITH collect(n) AS nodes UNWIND nodes AS __fg_boundary_0 \
         RETURN __fg_boundary_0,EXISTS((__fg_boundary_0)-[:R]->()) AS present",
    );
    assert_eq!(query.columns(), ["__fg_boundary_0", "present"]);
    assert_eq!(run(&query, wide(), &mut || Ok(())).unwrap().value.len(), 4);

    for expression in [
        "EXISTS((fresh)-[:R]->()) AS present",
        "EXISTS((x)-[:R]->(local)) AS present,local.p AS leaked",
        "EXISTS((x)-[:R]->()) AS present,__fg_boundary_0 AS leaked",
    ] {
        let statement = format!("{prefix} RETURN {expression}");
        let resolutions = std::cell::Cell::new(0);
        assert!(
            PreparedGraphSetText::prepare(&statement, |kind, name| {
                resolutions.set(resolutions.get() + 1);
                symbols(kind, name)
            })
            .is_err(),
            "{statement}"
        );
        assert_eq!(
            resolutions.get(),
            0,
            "scope admission precedes catalog reads"
        );
    }
}

#[test]
fn row_scope_existence_projection_keeps_late_source_failures_before_empty_pages() {
    for expression in [
        "true OR EXISTS((x)-[:R]->())",
        "false AND EXISTS((x)-[:R]->())",
        "CASE WHEN true THEN false ELSE EXISTS((x)-[:R]->()) END",
    ] {
        let query = prepare(&format!(
            "MATCH (n) WITH collect(n) AS nodes UNWIND nodes AS x \
             RETURN {expression} AS value LIMIT 0"
        ));
        let mut calls = 0;
        let result = query.execute_governed(
            wide(),
            |pattern, remaining| {
                calls += 1;
                if calls == 3 {
                    return Err(GqlQueryError::Source("later-projection-witness-error"));
                }
                pattern.plan().execute_governed_with_properties(
                    (4 + EDGES.len()) as u64,
                    (1..=4).map(VId),
                    EDGES,
                    |_, _| Ok::<_, &'static str>(true),
                    |_, _| Ok(None),
                    remaining,
                    || Ok::<_, usize>(()),
                )
            },
            || Ok::<_, usize>(()),
        );
        assert_eq!(calls, 3, "{expression}");
        assert!(matches!(
            result,
            Err(GqlQueryError::Source(GraphSetExecutionError::Source(
                "later-projection-witness-error"
            )))
        ));
    }
}

#[test]
fn row_scope_existence_projection_shares_exact_budgets_and_original_interruptions() {
    for suffix in ["", " LIMIT 0"] {
        let query = prepare(&format!(
            "MATCH (n) WITH collect(n) AS nodes UNWIND nodes AS x \
             RETURN x,EXISTS((x)-[:R]->()) XOR EXISTS((x)<-[:R]-()) AS different{suffix}"
        ));
        let mut checkpoints = 0;
        let measured = run(&query, wide(), &mut || {
            checkpoints += 1;
            Ok(())
        })
        .unwrap();
        let caps = [
            measured.rows.snapshot_records,
            measured.rows.result_rows,
            measured.evaluator.work_units,
            measured.evaluator.scratch_entries,
        ];
        assert_eq!(
            run(
                &query,
                GqlQueryPolicy::new(caps[0], caps[1], caps[2], caps[3]),
                &mut || Ok(()),
            )
            .unwrap(),
            measured
        );
        for dimension in 0..4 {
            if caps[dimension] == 0 {
                continue;
            }
            let mut bounded = caps;
            bounded[dimension] -= 1;
            assert!(
                run(
                    &query,
                    GqlQueryPolicy::new(bounded[0], bounded[1], bounded[2], bounded[3]),
                    &mut || Ok(()),
                )
                .is_err(),
                "{suffix}, dimension {dimension}"
            );
        }
        for stop in [1, checkpoints / 2, checkpoints] {
            let mut seen = 0;
            let result = run(&query, wide(), &mut || {
                seen += 1;
                if seen == stop { Err(stop) } else { Ok(()) }
            });
            assert!(
                matches!(result, Err(GqlQueryError::Interrupted(found)) if found == stop),
                "checkpoint {stop}/{checkpoints}"
            );
            assert_eq!(seen, stop);
        }
    }
}

#[test]
fn row_scope_existence_projection_preserves_call_boundaries_aliases_and_distinct_pages() {
    let prefix = "MATCH (n) WITH collect(n) AS nodes UNWIND nodes AS x";
    let expected: Vec<_> = (1..=4)
        .map(|id| {
            GraphValueRow::from_owned_values(vec![
                GraphValue::Vertex(VId(id)),
                GraphValue::Scalar(CanonicalScalar::Bool(id < 4)),
            ])
        })
        .collect();
    let aliased = prepare(&format!(
        "{prefix} RETURN x AS WITH,EXISTS((x)-[:R]->()) AS CALL ORDER BY WITH"
    ));
    assert_eq!(aliased.columns(), ["WITH", "CALL"]);
    assert_eq!(
        run(&aliased, wide(), &mut || Ok(())).unwrap().value,
        expected
    );

    // The CALL's RETURN probe must stop at its closing brace. The following
    // outer WHERE has an anchor that is deliberately not imported into CALL.
    assert_eq!(
        execute(&format!(
            "{prefix} WITH x,nodes[0] AS other \
             CALL (x) {{ RETURN EXISTS((x)-[:R]->()) AS outgoing }} \
             WHERE EXISTS((other)-[:R]->()) RETURN x,outgoing ORDER BY x"
        )),
        expected
    );
    let star = prepare(&format!(
        "{prefix} WITH x CALL (x) {{ RETURN EXISTS((x)-[:R]->()) AS outgoing }} \
         RETURN * ORDER BY x"
    ));
    assert_eq!(star.columns(), ["outgoing", "x"]);
    assert_eq!(
        run(&star, wide(), &mut || Ok(())).unwrap().value,
        (1..=4)
            .map(|id| GraphValueRow::from_owned_values(vec![
                GraphValue::Scalar(CanonicalScalar::Bool(id < 4)),
                GraphValue::Vertex(VId(id)),
            ]))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        execute(&format!(
            "{prefix} RETURN DISTINCT EXISTS((x)-[:R]->()) AS outgoing \
             ORDER BY outgoing DESC SKIP 1 LIMIT 1"
        )),
        vec![GraphValueRow::from_owned_values(vec![GraphValue::Scalar(
            CanonicalScalar::Bool(false),
        )])]
    );
    for (expression, expected) in [
        ("EXISTS((x)-[:R]->()) = true", [true, true, true, false]),
        ("EXISTS((x)-[:R]->()) IS NOT NULL", [true, true, true, true]),
    ] {
        let rows = execute(&format!(
            "{prefix} RETURN x,{expression} AS value ORDER BY x"
        ));
        let values: Vec<_> = expected
            .into_iter()
            .enumerate()
            .map(|(at, value)| {
                GraphValueRow::from_owned_values(vec![
                    GraphValue::Vertex(VId(at as u128 + 1)),
                    GraphValue::Scalar(CanonicalScalar::Bool(value)),
                ])
            })
            .collect();
        assert_eq!(rows, values, "{expression}");
    }
}

#[test]
fn row_scope_pattern_counts_rebind_anchors_and_count_parallel_witnesses() {
    let template = PreparedGraphSetText::prepare(
        "UNWIND $items AS x RETURN x,COUNT { (x)-[:R]->() } AS outgoing,\
         COUNT { MATCH (x)<-[:R]-(y) WHERE y.p >= $minimum } AS incoming",
        symbols,
    )
    .unwrap();
    let frozen = template.canonical_template_bytes();
    for items in [
        vec![
            GraphValue::Vertex(VId(1)),
            GraphValue::Vertex(VId(1)),
            GraphValue::Vertex(VId(4)),
            GraphValue::Vertex(VId(u128::MAX)),
            GraphValue::Scalar(CanonicalScalar::Null),
            GraphValue::Scalar(CanonicalScalar::Int(1)),
            GraphValue::Edge(fgdb_types::EId(1)),
            GraphValue::List(vec![].into()),
        ],
        vec![GraphValue::Vertex(VId(2)), GraphValue::Vertex(VId(3))],
        vec![],
    ] {
        for minimum in [1_i64, 2, 3] {
            let values = [1_i64, 2, 2, 3];
            let mut expected: Vec<_> = items
                .iter()
                .map(|item| {
                    let outgoing = EDGES
                        .iter()
                        .filter(|&&(from, _, _)| *item == GraphValue::Vertex(from))
                        .count();
                    let incoming = EDGES
                        .iter()
                        .filter(|&&(from, _, to)| {
                            *item == GraphValue::Vertex(to)
                                && values[from.0 as usize - 1] >= minimum
                        })
                        .count();
                    GraphValueRow::from_owned_values(vec![
                        item.clone(),
                        GraphValue::Scalar(CanonicalScalar::Int(outgoing as i64)),
                        GraphValue::Scalar(CanonicalScalar::Int(incoming as i64)),
                    ])
                })
                .collect();
            expected.sort();
            let arguments = GqlParameters::new()
                .with_list("items", items.clone())
                .unwrap()
                .with_int64("minimum", minimum)
                .unwrap();
            let query = template.bind_parameters(&arguments).unwrap();
            assert_eq!(query.columns(), ["x", "outgoing", "incoming"]);
            let mut actual = run(&query, wide(), &mut || Ok(())).unwrap().value;
            actual.sort();
            assert_eq!(actual, expected, "minimum {minimum}");
        }
    }
    assert_eq!(template.canonical_template_bytes(), frozen);
    assert!(template.bind_parameters(&GqlParameters::new()).is_err());
    let presence = PreparedGraphSetText::prepare(
        "UNWIND $items AS x RETURN x,EXISTS { MATCH (x)-[:R]->() } AS outgoing",
        symbols,
    )
    .unwrap();
    assert_ne!(frozen, presence.canonical_template_bytes());
}

#[test]
fn row_scope_pattern_lists_rebind_anchors_and_keep_null_witnesses() {
    let template = PreparedGraphSetText::prepare(
        "UNWIND $items AS x RETURN x,[(x)-[:R]->(y) WHERE y.p >= $minimum | y.p] AS values,\
         [(x)-[:R]->() | null] AS missing",
        symbols,
    )
    .unwrap();
    let frozen = template.canonical_template_bytes();
    let properties = [1_i64, 2, 2, 3];
    for items in [
        vec![
            GraphValue::Vertex(VId(1)),
            GraphValue::Vertex(VId(1)),
            GraphValue::Vertex(VId(4)),
            GraphValue::Vertex(VId(u128::MAX)),
            GraphValue::Scalar(CanonicalScalar::Null),
            GraphValue::Scalar(CanonicalScalar::Int(1)),
            GraphValue::Edge(fgdb_types::EId(1)),
            GraphValue::List(Vec::new().into()),
        ],
        vec![GraphValue::Vertex(VId(2)), GraphValue::Vertex(VId(3))],
        Vec::new(),
    ] {
        for minimum in [1_i64, 2, 3] {
            let mut expected: Vec<_> = items
                .iter()
                .map(|item| {
                    let matched: Vec<_> = EDGES
                        .iter()
                        .filter(|&&(from, _, _)| *item == GraphValue::Vertex(from))
                        .map(|&(_, _, to)| properties[to.0 as usize - 1])
                        .collect();
                    GraphValueRow::from_owned_values(vec![
                        item.clone(),
                        GraphValue::List(
                            matched
                                .iter()
                                .copied()
                                .filter(|value| *value >= minimum)
                                .map(|value| GraphValue::Scalar(CanonicalScalar::Int(value)))
                                .collect(),
                        ),
                        GraphValue::List(
                            vec![GraphValue::Scalar(CanonicalScalar::Null); matched.len()].into(),
                        ),
                    ])
                })
                .collect();
            expected.sort();
            let arguments = GqlParameters::new()
                .with_list("items", items.clone())
                .unwrap()
                .with_int64("minimum", minimum)
                .unwrap();
            let query = template.bind_parameters(&arguments).unwrap();
            assert_eq!(query.columns(), ["x", "values", "missing"]);
            let mut actual = run(&query, wide(), &mut || Ok(())).unwrap().value;
            actual.sort();
            assert_eq!(actual, expected, "minimum {minimum}");
        }
    }
    assert_eq!(template.canonical_template_bytes(), frozen);
    assert!(template.bind_parameters(&GqlParameters::new()).is_err());
}

#[test]
fn renamed_exists_preserves_occurrences_nulls_and_body_scope() {
    let expected = vec![vertices(&[1]), vertices(&[2]), vertices(&[3])];
    for expression in [
        "EXISTS { MATCH (x)-[:R]->() }",
        "EXISTS { (x)-[:R]->() }",
        "EXISTS((x)-[:R]->())",
        "NOT NOT EXISTS { MATCH (x)-[:R]->() }",
        "size([(x)-[:R]->(y) | y]) > 0",
    ] {
        let text = format!("MATCH (n) WITH n AS x WHERE {expression} RETURN * ORDER BY x");
        let query = prepare(&text);
        assert_eq!(query.columns(), ["x"]);
        assert_eq!(
            run(&query, wide(), &mut || Ok(())).unwrap().value,
            expected,
            "{text}"
        );
    }

    // The two EXISTS inputs have different physical owners: `a` can be
    // gathered at the graph head, while `x` must use the projected identity.
    assert_eq!(
        execute(
            "MATCH (a), (b) WITH a,b AS x \
             WHERE EXISTS { MATCH (a)-[:R]->() } AND EXISTS { (x)-[:R]->() } \
             RETURN a,x ORDER BY a,x"
        ),
        (1..=3)
            .flat_map(|a| (1..=3).map(move |x| vertices(&[a, x])))
            .collect::<Vec<_>>()
    );

    let template = PreparedGraphSetText::prepare(
        "MATCH (n) WITH n AS x \
         WHERE EXISTS { (x)-[:R]->(m) WHERE m.p = $wanted } RETURN x ORDER BY x",
        symbols,
    )
    .unwrap();
    let definition = template.canonical_template_bytes();
    for (wanted, ids) in [(2, vec![1, 2]), (3, vec![3]), (99, vec![])] {
        let arguments = GqlParameters::new().with_int64("wanted", wanted).unwrap();
        let query = template.bind_parameters(&arguments).unwrap();
        assert_eq!(
            run(&query, wide(), &mut || Ok(())).unwrap().value,
            ids.into_iter()
                .map(|id| vertices(&[id]))
                .collect::<Vec<_>>()
        );
    }
    assert_eq!(template.canonical_template_bytes(), definition);

    // Relationship identities need the same deferral. Binding `edge` afresh
    // at the old graph head would make every row see relationship 5's match.
    assert_eq!(
        execute_identified(
            "MATCH ()-[r:R]->() WITH r AS edge \
             RETURN edge,EXISTS { ()-[edge:R]->(end) WHERE end.p=3 } AS present ORDER BY edge"
        ),
        (1..=5)
            .map(|id| GraphValueRow::from_owned_values(vec![
                GraphValue::Edge(fgdb_types::EId(id)),
                GraphValue::Scalar(CanonicalScalar::Bool(id == 5)),
            ]))
            .collect::<Vec<_>>()
    );

    // An OPTIONAL NULL anchor is not rebound to any vertex. Parallel outer
    // witnesses remain separate, but inner witnesses never multiply a row.
    let expected = [
        (1, Some(2), true),
        (1, Some(2), true),
        (1, Some(3), true),
        (2, Some(3), true),
        (3, Some(4), false),
        (4, None, false),
    ]
    .into_iter()
    .map(|(n, x, exists)| {
        GraphValueRow::from_owned_values(vec![
            GraphValue::Vertex(VId(n)),
            x.map_or(GraphValue::Scalar(CanonicalScalar::Null), |x| {
                GraphValue::Vertex(VId(x))
            }),
            GraphValue::Scalar(CanonicalScalar::Bool(exists)),
        ])
    })
    .collect::<Vec<_>>();
    assert_eq!(
        execute(
            "MATCH (n) OPTIONAL MATCH (n)-[:R]->(m) WITH n,m AS x \
             RETURN n,x,EXISTS { (x)-[:R]->() } AS present ORDER BY n,x"
        ),
        expected
    );

    for text in [
        "MATCH (n) WITH n AS x WHERE EXISTS { (x)-[:R]->(m) } RETURN m",
        "MATCH (n) WITH n AS x RETURN EXISTS { (x)-[:R]->(m) } AS present,m",
    ] {
        let calls = std::cell::Cell::new(0);
        assert!(
            PreparedGraphSetText::prepare(text, |kind, name: &str| {
                calls.set(calls.get() + 1);
                symbols(kind, name)
            })
            .is_err(),
            "{text}"
        );
        assert_eq!(calls.get(), 0, "existential locals cannot escape: {text}");
    }
}

#[test]
fn renamed_exists_admits_each_definition_atom_once() {
    use fgdb_gql::algebra::{MAX_PATTERN_EDGES, MAX_PATTERN_IDENTITIES, MAX_PATTERN_PREDICATES};

    // Each body is above half of its statement-wide cap. Charging the
    // speculative graph-head parse as well as its real row-owner parse would
    // reject these otherwise admitted definitions.
    let bodies = [
        (
            format!(
                "MATCH REPEATABLE ELEMENTS (x){}",
                "-[:R]->()".repeat(MAX_PATTERN_EDGES / 2 + 1)
            ),
            false,
        ),
        (
            format!(
                "MATCH (x)-[:R]->(m) WHERE {}",
                vec!["m.p >= $wanted"; MAX_PATTERN_PREDICATES / 2 + 1].join(" AND ")
            ),
            true,
        ),
        (
            format!(
                "MATCH (x)-[:R]->(m) WHERE {}",
                vec!["m <> x"; MAX_PATTERN_IDENTITIES / 2 + 1].join(" AND ")
            ),
            false,
        ),
    ];
    for (body, parameterized) in bodies {
        let statement = format!("MATCH (n) WITH n AS x WHERE EXISTS {{ {body} }} RETURN x");
        let template = PreparedGraphSetText::prepare(&statement, symbols)
            .unwrap_or_else(|error| panic!("one admitted body: {error}; {statement}"));
        assert_eq!(
            template.parameter_schema().len(),
            usize::from(parameterized)
        );
        let args = if parameterized {
            GqlParameters::new().with_int64("wanted", 2).unwrap()
        } else {
            GqlParameters::new()
        };
        template.bind_parameters(&args).unwrap();
    }
    let oversized = format!(
        "MATCH (n) WITH n AS x WHERE EXISTS {{ MATCH REPEATABLE ELEMENTS (x){} }} RETURN x",
        "-[:R]->()".repeat(MAX_PATTERN_EDGES + 1)
    );
    let catalog_calls = std::cell::Cell::new(0);
    assert!(
        PreparedGraphSetText::prepare(&oversized, |kind, name: &str| {
            catalog_calls.set(catalog_calls.get() + 1);
            symbols(kind, name)
        })
        .is_err()
    );
    assert_eq!(catalog_calls.get(), 0);
}

#[test]
fn renamed_exists_keeps_shared_work_row_and_cancellation_admission() {
    let query =
        prepare("MATCH (n) WITH n AS x RETURN x,EXISTS { (x)-[:R]->() } AS present ORDER BY x");
    let mut checkpoints = 0;
    let execution = run(&query, wide(), &mut || {
        checkpoints += 1;
        Ok(())
    })
    .unwrap();
    assert_eq!(execution.value.len(), 4);
    assert!(execution.evaluator.work_units > 1);
    assert!(checkpoints > 2);
    for policy in [
        GqlQueryPolicy::new(u64::MAX, 3, u64::MAX, u64::MAX),
        GqlQueryPolicy::new(
            u64::MAX,
            u64::MAX,
            execution.evaluator.work_units - 1,
            u64::MAX,
        ),
    ] {
        assert!(run(&query, policy, &mut || Ok(())).is_err());
    }
    for stop in [0, checkpoints / 2, checkpoints - 1] {
        let mut observed = 0;
        assert!(matches!(
            run(&query, wide(), &mut || {
                if observed == stop {
                    Err(stop)
                } else {
                    observed += 1;
                    Ok(())
                }
            }),
            Err(GqlQueryError::Interrupted(at)) if at == stop
        ));
    }
}

#[test]
fn row_scope_pattern_lists_evaluate_only_the_selected_anchor_projection() {
    // Vertex 3 has p=2 and reaches p=3. Either the anchor or its target can
    // make the projection divide by zero. That root must not affect valid
    // witnesses of vertex 1 or any absent anchor.
    let items = vec![
        GraphValue::Vertex(VId(1)),
        GraphValue::Vertex(VId(4)),
        GraphValue::Vertex(VId(u128::MAX)),
        GraphValue::Scalar(CanonicalScalar::Null),
        GraphValue::Scalar(CanonicalScalar::Int(3)),
    ];
    let arguments = GqlParameters::new()
        .with_list("items", items.clone())
        .unwrap();
    let mut expected: Vec<_> = items
        .into_iter()
        .enumerate()
        .map(|(index, item)| {
            GraphValueRow::from_owned_values(vec![
                item,
                GraphValue::List(
                    vec![
                        GraphValue::Scalar(CanonicalScalar::Int(-1));
                        if index == 0 { 3 } else { 0 }
                    ]
                    .into(),
                ),
            ])
        })
        .collect();
    expected.sort();
    let failing = GqlParameters::new()
        .with_list("items", vec![GraphValue::Vertex(VId(3))])
        .unwrap();
    for expression in ["1 / (y.p - 3)", "1 / (x.p - 2)"] {
        let template = PreparedGraphSetText::prepare(
            &format!("UNWIND $items AS x RETURN x,[(x)-[:R]->(y) | {expression}] AS values"),
            symbols,
        )
        .unwrap();
        let query = template.bind_parameters(&arguments).unwrap();
        let mut actual = run(&query, wide(), &mut || Ok(())).unwrap().value;
        actual.sort();
        assert_eq!(actual, expected, "{expression}");
        assert!(
            run(
                &template.bind_parameters(&failing).unwrap(),
                wide(),
                &mut || Ok(())
            )
            .is_err(),
            "{expression}"
        );
    }
}

#[test]
fn row_scope_pattern_lists_keep_paths_multiple_locals_and_row_boundaries() {
    // Named paths retain real relationship identities, including parallel
    // witnesses; the ordinary anonymous-edge fixture cannot supply them.
    let execute = |text: &str| {
        let values =
            [1, 2, 2, 3].map(|value| CanonicalPropertyValue::Scalar(CanonicalScalar::Int(value)));
        prepare(text)
            .execute_governed_with_elements(
                wide(),
                |pattern, remaining| {
                    pattern.plan().execute_governed_with_element_accessors(
                        (values.len() + EDGES.len()) as u64,
                        (1..=4).map(VId),
                        EDGES
                            .into_iter()
                            .enumerate()
                            .map(|(index, (from, relation, to))| {
                                (fgdb_types::EId(index as u128 + 1), from, relation, to)
                            }),
                        |vid, tests| {
                            Ok::<_, &'static str>(tests.iter().all(|test| {
                                test.matches(&[], &[(P, values[vid.0 as usize - 1].clone())])
                            }))
                        },
                        |vid, key| Ok((key == P).then(|| &values[vid.0 as usize - 1])),
                        |_, _| Ok(None),
                        |_| Ok(None),
                        |_| Ok(None),
                        remaining,
                        || Ok::<_, usize>(()),
                    )
                },
                |_, _, _| panic!("this fixture has no CALL source"),
                |element, key| {
                    Ok(match element {
                        ElementId::Vertex(vid) if key == P => {
                            Some(values[vid.0 as usize - 1].clone())
                        }
                        _ => None,
                    })
                },
                || Ok::<_, usize>(()),
            )
            .unwrap()
            .value
    };
    let prefix = "MATCH (n) WITH collect(n) AS nodes UNWIND nodes AS x";
    let list = |values: &[i64]| {
        GraphValue::List(
            values
                .iter()
                .map(|value| GraphValue::Scalar(CanonicalScalar::Int(*value)))
                .collect(),
        )
    };
    let expected: Vec<_> = [(1, vec![4, 4, 5]), (2, vec![5]), (3, vec![]), (4, vec![])]
        .into_iter()
        .map(|(id, values)| {
            GraphValueRow::from_owned_values(vec![GraphValue::Vertex(VId(id)), list(&values)])
        })
        .collect();
    assert_eq!(
        execute(&format!(
            "{prefix} RETURN x,[(x)-[:R]->(middle)-[:R]->(end) | middle.p+end.p] AS values ORDER BY x"
        )),
        expected
    );
    assert_eq!(
        execute(&format!(
            "{prefix} CALL (x) {{ RETURN [path=(x)-[:R]->() | length(path)] AS lengths }} \
             RETURN x,lengths ORDER BY x"
        )),
        [3_usize, 1, 1, 0]
            .into_iter()
            .enumerate()
            .map(|(index, count)| GraphValueRow::from_owned_values(vec![
                GraphValue::Vertex(VId(index as u128 + 1)),
                list(&vec![1; count]),
            ]))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        execute(&format!(
            "{prefix} WITH x WHERE size([(x)-[:R]->() | 1]) > 1 RETURN x"
        )),
        vec![vertices(&[1])]
    );
    assert_eq!(
        execute("MATCH (n) WITH n AS x RETURN x,[(x)-[:R]->(y) | y.p] AS values ORDER BY x"),
        [(1, vec![2, 2, 2]), (2, vec![2]), (3, vec![3]), (4, vec![])]
            .into_iter()
            .map(|(id, values)| GraphValueRow::from_owned_values(vec![
                GraphValue::Vertex(VId(id)),
                list(&values),
            ]))
            .collect::<Vec<_>>()
    );
    // A renamed element is no binding of the head's graph scope: both a count
    // and an existence predicate stay with the completed row's identity.
    assert_eq!(
        execute("MATCH (n) WITH n AS x RETURN x,COUNT { (x)-[:R]->() } AS c ORDER BY x"),
        [(1, 3), (2, 1), (3, 1), (4, 0)]
            .into_iter()
            .map(|(id, count)| GraphValueRow::from_owned_values(vec![
                GraphValue::Vertex(VId(id)),
                GraphValue::Scalar(CanonicalScalar::Int(count)),
            ]))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        execute("MATCH (n) WITH n AS x WHERE EXISTS { MATCH (x)-[:R]->() } RETURN x"),
        vec![vertices(&[1]), vertices(&[2]), vertices(&[3])]
    );
    assert_eq!(
        execute("MATCH (n) WITH n AS x RETURN x,EXISTS { (x)-[:R]->() } AS e ORDER BY x"),
        [(1, true), (2, true), (3, true), (4, false)]
            .into_iter()
            .map(|(id, exists)| GraphValueRow::from_owned_values(vec![
                GraphValue::Vertex(VId(id)),
                GraphValue::Scalar(CanonicalScalar::Bool(exists)),
            ]))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        execute(&format!(
            "{prefix} UNWIND nodes AS y RETURN x,y,[(x)-[:R]->(y) | x.p+y.p] AS values \
             ORDER BY x,y"
        )),
        (1..=4)
            .flat_map(|from| {
                (1..=4).map(move |to| {
                    let properties = [1_i64, 2, 2, 3];
                    let values: Vec<_> = EDGES
                        .iter()
                        .filter(|&&(left, _, right)| left == VId(from) && right == VId(to))
                        .map(|_| properties[from as usize - 1] + properties[to as usize - 1])
                        .collect();
                    GraphValueRow::from_owned_values(vec![
                        GraphValue::Vertex(VId(from)),
                        GraphValue::Vertex(VId(to)),
                        list(&values),
                    ])
                })
            })
            .collect::<Vec<_>>()
    );
    assert_eq!(
        execute(&format!(
            "{prefix} RETURN DISTINCT [(x)-[:R]->(y) | y.p] AS values \
             ORDER BY values DESC SKIP 1 LIMIT 2"
        )),
        vec![
            GraphValueRow::from_owned_values(vec![list(&[2, 2, 2])]),
            GraphValueRow::from_owned_values(vec![list(&[2])]),
        ]
    );
}

#[test]
fn row_scope_pattern_lists_keep_private_names_and_invalid_anchor_refusals() {
    let prefix = "MATCH (n) WITH collect(n) AS nodes UNWIND nodes AS x";
    for expression in [
        "[(fresh)-[:R]->() | 1] AS values",
        "[(x)-[:R]->(local) | local] AS values,local AS leaked",
        "[(x)-[:R]->() | 1] AS values,__fg_boundary_0 AS leaked",
        "[(nodes)-[:R]->() | 1] AS values",
    ] {
        let statement = format!("{prefix} RETURN {expression}");
        let resolutions = std::cell::Cell::new(0);
        assert!(
            PreparedGraphSetText::prepare(&statement, |kind, name| {
                resolutions.set(resolutions.get() + 1);
                symbols(kind, name)
            })
            .is_err(),
            "{statement}"
        );
        assert_eq!(resolutions.get(), 0, "scope admission before catalog reads");
    }
}

#[test]
fn row_scope_pattern_lists_keep_late_errors_and_one_cumulative_budget() {
    let prefix = "MATCH (n) WITH collect(n) AS nodes UNWIND nodes AS x";
    for suffix in ["", " LIMIT 0"] {
        let query = prepare(&format!(
            "{prefix} RETURN x,[(x)-[:R]->(y) | y.p] AS values{suffix}"
        ));
        let mut checkpoints = 0;
        let measured = run(&query, wide(), &mut || {
            checkpoints += 1;
            Ok(())
        })
        .unwrap();
        let caps = [
            measured.rows.snapshot_records,
            measured.rows.result_rows,
            measured.evaluator.work_units,
            measured.evaluator.scratch_entries,
        ];
        assert_eq!(
            run(
                &query,
                GqlQueryPolicy::new(caps[0], caps[1], caps[2], caps[3]),
                &mut || Ok(()),
            )
            .unwrap(),
            measured
        );
        for dimension in 0..4 {
            if caps[dimension] == 0 {
                continue;
            }
            let mut bounded = caps;
            bounded[dimension] -= 1;
            assert!(
                run(
                    &query,
                    GqlQueryPolicy::new(bounded[0], bounded[1], bounded[2], bounded[3]),
                    &mut || Ok(()),
                )
                .is_err(),
                "{suffix}, dimension {dimension}"
            );
        }
        for stop in [1, checkpoints / 2, checkpoints] {
            let mut seen = 0;
            assert!(matches!(
                run(&query, wide(), &mut || {
                    seen += 1;
                    if seen == stop { Err(stop) } else { Ok(()) }
                }),
                Err(GqlQueryError::Interrupted(at)) if at == stop
            ));
        }
        assert!(matches!(
            run_reading(&query, wide(), &mut || Ok(()), true),
            Err(GqlQueryError::Source(GraphSetExecutionError::Source(
                "element-read-error"
            )))
        ));
    }
    let query = prepare(&format!(
        "{prefix} RETURN CASE WHEN true THEN 7 ELSE [(x)-[:R]->() | 1] END AS values LIMIT 0"
    ));
    let mut calls = 0;
    let result = query.execute_governed(
        wide(),
        |pattern, remaining| {
            calls += 1;
            if calls == 3 {
                return Err(GqlQueryError::Source("later-comprehension-witness-error"));
            }
            pattern.plan().execute_governed_with_properties(
                (4 + EDGES.len()) as u64,
                (1..=4).map(VId),
                EDGES,
                |_, _| Ok::<_, &'static str>(true),
                |_, _| Ok(None),
                remaining,
                || Ok::<_, usize>(()),
            )
        },
        || Ok::<_, usize>(()),
    );
    assert_eq!(calls, 3);
    assert!(matches!(
        result,
        Err(GqlQueryError::Source(GraphSetExecutionError::Source(
            "later-comprehension-witness-error"
        )))
    ));
}

#[test]
fn row_scope_pattern_counts_compose_with_where_values_and_call_boundaries() {
    let prefix = "MATCH (n) WITH collect(n) AS nodes UNWIND nodes AS x";
    for (expression, counts) in [
        ("COUNT { (x)-[:R]->() }", [3, 1, 1, 0]),
        ("COUNT { MATCH (x)-[:R]->() } + 2", [5, 3, 3, 2]),
        (
            "CASE WHEN EXISTS((x)-[:R]->()) THEN COUNT { (x)-[:R]->() } ELSE 9 END",
            [3, 1, 1, 9],
        ),
        ("COALESCE(COUNT { (x)-[:R]->() }, 1 / 0)", [3, 1, 1, 0]),
    ] {
        assert_eq!(
            execute(&format!(
                "{prefix} RETURN x,{expression} AS degree ORDER BY x"
            )),
            counts
                .into_iter()
                .enumerate()
                .map(|(at, count)| GraphValueRow::from_owned_values(vec![
                    GraphValue::Vertex(VId(at as u128 + 1)),
                    GraphValue::Scalar(CanonicalScalar::Int(count)),
                ]))
                .collect::<Vec<_>>(),
            "{expression}"
        );
    }
    assert_eq!(
        execute(&format!(
            "{prefix} WITH x WHERE COUNT {{ (x)-[:R]->() }} > 1 \
             OR COUNT {{ (x)<-[:R]-() }} = 1 RETURN x ORDER BY x"
        )),
        vec![vertices(&[1]), vertices(&[4])]
    );
    assert_eq!(
        execute(&format!(
            "{prefix} WITH x,nodes[0] AS other \
             CALL (x) {{ RETURN COUNT {{ (x)-[:R]->() }} AS degree }} \
             WHERE EXISTS((other)-[:R]->()) RETURN x,degree ORDER BY x"
        )),
        [3, 1, 1, 0]
            .into_iter()
            .enumerate()
            .map(|(at, count)| GraphValueRow::from_owned_values(vec![
                GraphValue::Vertex(VId(at as u128 + 1)),
                GraphValue::Scalar(CanonicalScalar::Int(count)),
            ]))
            .collect::<Vec<_>>()
    );
    let query = prepare(&format!(
        "{prefix} RETURN DISTINCT COUNT {{ (x)-[:R]->() }} AS degree \
         ORDER BY degree DESC SKIP 1 LIMIT 2"
    ));
    assert_eq!(query.columns(), ["degree"]);
    assert_eq!(
        run(&query, wide(), &mut || Ok(())).unwrap().value,
        [1, 0]
            .into_iter()
            .map(
                |value| GraphValueRow::from_owned_values(vec![GraphValue::Scalar(
                    CanonicalScalar::Int(value)
                )])
            )
            .collect::<Vec<_>>()
    );
}

#[test]
fn row_scope_pattern_counts_keep_scopes_private_before_catalog_access() {
    let prefix = "MATCH (n) WITH collect(n) AS nodes UNWIND nodes AS x";
    for expression in [
        "COUNT { (fresh)-[:R]->() } AS total",
        "COUNT { (x)-[:R]->(local) } AS total,local.p AS leaked",
        "COUNT { (x)-[:R]->() } AS total,__fg_boundary_0 AS leaked",
        "COUNT { (x)-[:R]->() } AS total,COUNT { (nodes)-[:R]->() } AS invalid",
    ] {
        let statement = format!("{prefix} RETURN {expression}");
        let resolutions = std::cell::Cell::new(0);
        assert!(
            PreparedGraphSetText::prepare(&statement, |kind, name| {
                resolutions.set(resolutions.get() + 1);
                symbols(kind, name)
            })
            .is_err(),
            "{statement}"
        );
        assert_eq!(
            resolutions.get(),
            0,
            "scope admission precedes catalog reads"
        );
    }
    let query = prepare(&format!(
        "{prefix} WITH x AS __fg_boundary_0 \
         RETURN __fg_boundary_0,COUNT {{ (__fg_boundary_0)-[:R]->() }} AS total"
    ));
    assert_eq!(query.columns(), ["__fg_boundary_0", "total"]);
    assert_eq!(run(&query, wide(), &mut || Ok(())).unwrap().value.len(), 4);
}

#[test]
fn row_scope_pattern_counts_keep_late_source_failures_before_empty_pages() {
    for expression in [
        "COUNT { (x)-[:R]->() }",
        "CASE WHEN true THEN 7 ELSE COUNT { (x)-[:R]->() } END",
    ] {
        let query = prepare(&format!(
            "MATCH (n) WITH collect(n) AS nodes UNWIND nodes AS x \
             RETURN {expression} AS value LIMIT 0"
        ));
        let mut calls = 0;
        let result = query.execute_governed(
            wide(),
            |pattern, remaining| {
                calls += 1;
                if calls == 3 {
                    return Err(GqlQueryError::Source("later-count-witness-error"));
                }
                pattern.plan().execute_governed_with_properties(
                    (4 + EDGES.len()) as u64,
                    (1..=4).map(VId),
                    EDGES,
                    |_, _| Ok::<_, &'static str>(true),
                    |_, _| Ok(None),
                    remaining,
                    || Ok::<_, usize>(()),
                )
            },
            || Ok::<_, usize>(()),
        );
        assert_eq!(calls, 3, "{expression}");
        assert!(matches!(
            result,
            Err(GqlQueryError::Source(GraphSetExecutionError::Source(
                "later-count-witness-error"
            )))
        ));
    }
}

#[test]
fn row_scope_pattern_counts_share_exact_budgets_and_original_interruptions() {
    for suffix in ["", " LIMIT 0"] {
        let query = prepare(&format!(
            "MATCH (n) WITH collect(n) AS nodes UNWIND nodes AS x \
             RETURN x,COUNT {{ (x)-[:R]->() }} + COUNT {{ (x)<-[:R]-() }} AS degree{suffix}"
        ));
        let mut checkpoints = 0;
        let measured = run(&query, wide(), &mut || {
            checkpoints += 1;
            Ok(())
        })
        .unwrap();
        let caps = [
            measured.rows.snapshot_records,
            measured.rows.result_rows,
            measured.evaluator.work_units,
            measured.evaluator.scratch_entries,
        ];
        assert_eq!(
            run(
                &query,
                GqlQueryPolicy::new(caps[0], caps[1], caps[2], caps[3]),
                &mut || Ok(()),
            )
            .unwrap(),
            measured
        );
        for dimension in 0..4 {
            if caps[dimension] == 0 {
                continue;
            }
            let mut bounded = caps;
            bounded[dimension] -= 1;
            assert!(
                run(
                    &query,
                    GqlQueryPolicy::new(bounded[0], bounded[1], bounded[2], bounded[3]),
                    &mut || Ok(()),
                )
                .is_err(),
                "{suffix}, dimension {dimension}"
            );
        }
        for stop in [1, checkpoints / 2, checkpoints] {
            let mut seen = 0;
            let result = run(&query, wide(), &mut || {
                seen += 1;
                if seen == stop { Err(stop) } else { Ok(()) }
            });
            assert!(
                matches!(result, Err(GqlQueryError::Interrupted(found)) if found == stop),
                "checkpoint {stop}/{checkpoints}"
            );
            assert_eq!(seen, stop);
        }
    }
}

#[test]
fn carried_pattern_comprehensions_rebind_any_anchors_without_losing_nulls_or_order() {
    let text = "UNWIND $items AS x RETURN x, \
        [(x)-[:R]->(t) | t] AS targets,[(x)-[:R]->(t) | NULL] AS holes, \
        [(x)<-[:R]-(t) | -t.p] AS reverse, \
        [(x)-[:R]->(t) WHERE t.p >= $minimum | t.p + $bump] AS scores";
    let template = PreparedGraphSetText::prepare(text, symbols).unwrap();
    let frozen = template.canonical_template_bytes();
    let properties = [1_i64, 2, 2, 3];
    for items in [
        vec![
            GraphValue::Vertex(VId(1)),
            GraphValue::Vertex(VId(1)),
            GraphValue::Vertex(VId(3)),
            GraphValue::Vertex(VId(4)),
            GraphValue::Vertex(VId(u128::MAX)),
            GraphValue::Scalar(CanonicalScalar::Null),
            GraphValue::Scalar(CanonicalScalar::Int(1)),
            GraphValue::Edge(fgdb_types::EId(1)),
            GraphValue::List(vec![].into()),
        ],
        vec![],
        vec![GraphValue::Vertex(VId(2)), GraphValue::Vertex(VId(3))],
    ] {
        for (minimum, bump) in [(1_i64, 4_i64), (3, -2)] {
            let mut expected: Vec<_> = items
                .iter()
                .map(|item| {
                    let targets: Vec<_> = EDGES
                        .iter()
                        .filter(|&&(from, _, _)| *item == GraphValue::Vertex(from))
                        .map(|&(_, _, to)| GraphValue::Vertex(to))
                        .collect();
                    let holes = vec![GraphValue::Scalar(CanonicalScalar::Null); targets.len()];
                    let reverse: Vec<_> = EDGES
                        .iter()
                        .filter(|&&(_, _, to)| *item == GraphValue::Vertex(to))
                        .map(|&(from, _, _)| {
                            GraphValue::Scalar(CanonicalScalar::Int(
                                -properties[from.0 as usize - 1],
                            ))
                        })
                        .collect();
                    let scores: Vec<_> = EDGES
                        .iter()
                        .filter(|&&(from, _, to)| {
                            *item == GraphValue::Vertex(from)
                                && properties[to.0 as usize - 1] >= minimum
                        })
                        .map(|&(_, _, to)| {
                            GraphValue::Scalar(CanonicalScalar::Int(
                                properties[to.0 as usize - 1] + bump,
                            ))
                        })
                        .collect();
                    GraphValueRow::from_owned_values(vec![
                        item.clone(),
                        GraphValue::List(targets.into()),
                        GraphValue::List(holes.into()),
                        GraphValue::List(reverse.into()),
                        GraphValue::List(scores.into()),
                    ])
                })
                .collect();
            let arguments = GqlParameters::new()
                .with_list("items", items.clone())
                .unwrap()
                .with_int64("minimum", minimum)
                .unwrap()
                .with_int64("bump", bump)
                .unwrap();
            let query = template.bind_parameters(&arguments).unwrap();
            assert_eq!(
                query.columns(),
                ["x", "targets", "holes", "reverse", "scores"]
            );
            let mut actual = run(&query, wide(), &mut || Ok(())).unwrap().value;
            actual.sort();
            expected.sort();
            assert_eq!(actual, expected, "minimum {minimum}, bump {bump}");
        }
    }
    assert_eq!(template.canonical_template_bytes(), frozen);
    assert!(template.bind_parameters(&GqlParameters::new()).is_err());
}

#[test]
fn carried_pattern_comprehensions_respect_aliases_captured_scalars_and_map_scope() {
    let prefix = "MATCH (n) WITH collect(n) AS nodes UNWIND nodes AS x";
    let rows = execute(&format!(
        "{prefix} WITH x,10 AS bias \
         CALL (x,bias) {{ RETURN [(x)-[:R]->(t) | t.p+bias] AS targets }} \
         RETURN x,targets ORDER BY x"
    ));
    let expected: Vec<_> = (1..=4)
        .map(|id| {
            let values = EDGES
                .iter()
                .filter(|&&(from, _, _)| from == VId(id))
                .map(|&(_, _, to)| {
                    GraphValue::Scalar(CanonicalScalar::Int([1, 2, 2, 3][to.0 as usize - 1] + 10))
                })
                .collect::<Vec<_>>();
            GraphValueRow::from_owned_values(vec![
                GraphValue::Vertex(VId(id)),
                GraphValue::List(values.into()),
            ])
        })
        .collect();
    assert_eq!(rows, expected);

    let mut rebound = execute("MATCH (n)-->(k) WITH n AS k RETURN [(k)-->() | 1] AS values");
    let mut expected = EDGES
        .iter()
        .map(|&(from, _, _)| {
            let degree = EDGES
                .iter()
                .filter(|&&(source, _, _)| source == from)
                .count();
            GraphValueRow::from_owned_values(vec![GraphValue::List(
                vec![GraphValue::Scalar(CanonicalScalar::Int(1)); degree].into(),
            )])
        })
        .collect::<Vec<_>>();
    rebound.sort();
    expected.sort();
    assert_eq!(rebound, expected);

    // The raw graph collection sees other roots, but mapping belongs only
    // to this correlated row. Vertex3's target has p=3 and must not be mapped.
    assert_eq!(
        execute(
            "MATCH (n) WHERE n.p=1 WITH collect(n) AS nodes UNWIND nodes AS x \
             RETURN [(x)-[:R]->(t) | 1/(3-t.p)] AS values"
        ),
        vec![GraphValueRow::from_owned_values(vec![GraphValue::List(
            vec![GraphValue::Scalar(CanonicalScalar::Int(1)); 3].into(),
        )])]
    );
    let bad = prepare(
        "MATCH (n) WHERE n.p=2 WITH collect(n) AS nodes UNWIND nodes AS x \
         RETURN [(x)-[:R]->(t) | 1/(3-t.p)] AS values LIMIT 0",
    );
    assert!(run(&bad, wide(), &mut || Ok(())).is_err());

    let private_alias = prepare(&format!(
        "{prefix} WITH x AS __fg_boundary_0 \
         RETURN __fg_boundary_0,[(__fg_boundary_0)-[:R]->(t) | t] AS targets"
    ));
    assert_eq!(private_alias.columns(), ["__fg_boundary_0", "targets"]);
    assert_eq!(
        run(&private_alias, wide(), &mut || Ok(()))
            .unwrap()
            .value
            .len(),
        4
    );
}

#[test]
fn carried_pattern_comprehensions_refuse_uncaptured_and_shadowed_bindings_before_catalog() {
    let prefix = "MATCH (n) WITH collect(n) AS nodes UNWIND nodes AS x";
    for tail in [
        "RETURN [(fresh)-[:R]->(t) | t] AS values",
        "RETURN [(x)-[:R]->(local) | local] AS values,local.p AS leaked",
        "RETURN [(x)-[:R]->(t) | t] AS values,__fg_boundary_0 AS leaked",
        "WITH x,7 AS t RETURN [(x)-[:R]->(t) | t] AS values",
        "WITH x,7 AS r RETURN [(x)-[r:R]->(t) | t] AS values",
        "WITH x,7 AS path RETURN [path=(x)-[:R]->(t) | t] AS values",
        "WITH x,x AS y RETURN [(x)-[:R]->(t) WHERE EXISTS { MATCH (y)-[:R]->() } | t] AS nested_capture",
    ] {
        let text = format!("{prefix} {tail}");
        let mut calls = 0;
        assert!(
            PreparedGraphSetText::prepare(&text, |kind, name| {
                calls += 1;
                symbols(kind, name)
            })
            .is_err(),
            "{text}"
        );
        assert_eq!(calls, 0, "{text}");
    }
}

#[test]
fn carried_pattern_comprehensions_preserve_source_failures_budgets_and_empty_apply() {
    let query = prepare(
        "MATCH (n) WITH collect(n) AS nodes UNWIND nodes AS x \
         RETURN [(x)-[:R]->(t) | NULL] AS values LIMIT 0",
    );
    let mut calls = 0;
    let result = query.execute_governed(
        wide(),
        |pattern, remaining| {
            calls += 1;
            if calls == 3 {
                return Err(GqlQueryError::Source("later-comprehension-source"));
            }
            pattern.plan().execute_governed_with_properties(
                (4 + EDGES.len()) as u64,
                (1..=4).map(VId),
                EDGES,
                |_, _| Ok::<_, &'static str>(true),
                |_, _| Ok(None),
                remaining,
                || Ok::<_, usize>(()),
            )
        },
        || Ok::<_, usize>(()),
    );
    assert_eq!(calls, 3);
    assert!(matches!(
        result,
        Err(GqlQueryError::Source(GraphSetExecutionError::Source(
            "later-comprehension-source"
        )))
    ));

    let empty = PreparedGraphSetText::prepare(
        "UNWIND $items AS x RETURN [(x)-[:R]->(t) | 1/0] AS values LIMIT 0",
        symbols,
    )
    .unwrap()
    .bind_parameters(&GqlParameters::new().with_list("items", vec![]).unwrap())
    .unwrap();
    let mut calls = 0;
    let result = empty.execute_governed(
        wide(),
        |_, _| {
            calls += 1;
            Err(GqlQueryError::Source("unreachable-body"))
        },
        || Ok::<_, usize>(()),
    );
    assert!(result.unwrap().value.is_empty());
    assert_eq!(calls, 0);

    for suffix in ["", " LIMIT 0"] {
        let query = prepare(&format!(
            "MATCH (n) WITH collect(n) AS nodes UNWIND nodes AS x \
             RETURN x,[(x)-[:R]->(t) | t.p] AS values{suffix}"
        ));
        let mut checkpoints = 0;
        let measured = run(&query, wide(), &mut || {
            checkpoints += 1;
            Ok(())
        })
        .unwrap();
        let caps = [
            measured.rows.snapshot_records,
            measured.rows.result_rows,
            measured.evaluator.work_units,
            measured.evaluator.scratch_entries,
        ];
        assert_eq!(
            run(
                &query,
                GqlQueryPolicy::new(caps[0], caps[1], caps[2], caps[3]),
                &mut || Ok(())
            )
            .unwrap(),
            measured
        );
        for dimension in 0..4 {
            if caps[dimension] == 0 {
                continue;
            }
            let mut limited = caps;
            limited[dimension] -= 1;
            assert!(
                run(
                    &query,
                    GqlQueryPolicy::new(limited[0], limited[1], limited[2], limited[3]),
                    &mut || Ok(())
                )
                .is_err()
            );
        }
        assert!(checkpoints > 2);
        for stop in [1, checkpoints / 2, checkpoints] {
            let mut seen = 0;
            let result = run(&query, wide(), &mut || {
                seen += 1;
                if seen == stop { Err(stop) } else { Ok(()) }
            });
            assert!(matches!(result, Err(GqlQueryError::Interrupted(at)) if at == stop));
            assert_eq!(seen, stop);
        }
    }
}

#[test]
fn carried_pattern_comprehension_tuple_locals_keep_the_outer_row_namespace() {
    // Two overlapping :R atoms are DIFFERENT EDGES, which needs identities.
    let rows = execute_identified(
        "MATCH (n) WITH collect(n) AS nodes UNWIND nodes AS x WITH x,10 AS bias \
         CALL (x,bias) { \
           RETURN [(x)-[:R]->(middle)-[:R]->(target) | [middle.p+bias,target.p]] AS pairs \
         } RETURN x,pairs ORDER BY x",
    );
    let properties = [1_i64, 2, 2, 3];
    let expected: Vec<_> = (1..=4)
        .map(|id| {
            let mut pairs = Vec::new();
            for &(from, _, middle) in &EDGES {
                if from != VId(id) {
                    continue;
                }
                for &(next, _, target) in &EDGES {
                    if next == middle {
                        pairs.push(GraphValue::List(
                            vec![
                                GraphValue::Scalar(CanonicalScalar::Int(
                                    properties[middle.0 as usize - 1] + 10,
                                )),
                                GraphValue::Scalar(CanonicalScalar::Int(
                                    properties[target.0 as usize - 1],
                                )),
                            ]
                            .into(),
                        ));
                    }
                }
            }
            GraphValueRow::from_owned_values(vec![
                GraphValue::Vertex(VId(id)),
                GraphValue::List(pairs.into()),
            ])
        })
        .collect();
    assert_eq!(rows, expected);
}

#[test]
fn carried_pattern_comprehension_fields_are_admitted_before_a_write_chain() {
    let prefix = "CREATE (a) WITH a MATCH (b) \
                  WITH collect(b) AS nodes UNWIND nodes AS x";
    for tail in [
        "RETURN [(x)-[:R]->(t) | t.p] AS xs",
        "CALL (x) { RETURN [(x)-[:R]->(t) | t.p] AS xs } RETURN xs",
    ] {
        let text = format!("{prefix} {tail}");
        let chain = fgdb_gql::PreparedGraphWriteChainText::prepare(&text, R, symbols)
            .unwrap()
            .bind_parameters(&GqlParameters::new())
            .unwrap();
        assert_eq!(chain.read_element_keys(), &[P], "{text}");
    }
}

#[test]
fn list_local_graph_expressions_preserve_order_multiplicity_and_null_maps() {
    let statements = [
        "WITH $items AS items,$bias AS bias RETURN \
         [x IN items WHERE EXISTS((x)-[:R]->()) | x.p+bias] AS filtered, \
         [x IN items | COUNT { (x)-[:R]->() }] AS degrees",
        "WITH $items AS items,$bias AS bias RETURN \
         [x IN items | CASE WHEN (x)-[:R]->() THEN null ELSE 9 END] AS holes, \
         [x IN items | [(x)-[:R]->(t) | t]] AS targets",
    ];
    let templates = statements.map(prepare_items);
    let frozen = templates
        .each_ref()
        .map(|template| template.canonical_template_bytes());
    let properties = [1_i64, 2, 2, 3];
    for items in [
        vec![
            GraphValue::Vertex(VId(3)),
            GraphValue::Vertex(VId(1)),
            GraphValue::Vertex(VId(1)),
            GraphValue::Vertex(VId(4)),
            GraphValue::Scalar(CanonicalScalar::Null),
            GraphValue::Scalar(CanonicalScalar::Int(17)),
            GraphValue::Edge(fgdb_types::EId(1)),
            GraphValue::List(vec![].into()),
            GraphValue::Vertex(VId(u128::MAX)),
            GraphValue::Vertex(VId(2)),
        ],
        vec![],
        vec![GraphValue::Vertex(VId(2)), GraphValue::Vertex(VId(1))],
    ] {
        let targets = |item: &GraphValue| {
            EDGES
                .iter()
                .filter(|&&(from, _, _)| *item == GraphValue::Vertex(from))
                .map(|&(_, _, to)| GraphValue::Vertex(to))
                .collect::<Vec<_>>()
        };
        for bias in [7_i64, -3] {
            let filtered: Vec<_> = items
                .iter()
                .filter(|item| !targets(item).is_empty())
                .map(|item| {
                    let GraphValue::Vertex(id) = item else {
                        panic!("only vertices can have outgoing graph witnesses");
                    };
                    GraphValue::Scalar(CanonicalScalar::Int(properties[id.0 as usize - 1] + bias))
                })
                .collect();
            let degrees: Vec<_> = items
                .iter()
                .map(|item| GraphValue::Scalar(CanonicalScalar::Int(targets(item).len() as i64)))
                .collect();
            let holes: Vec<_> = items
                .iter()
                .map(|item| {
                    GraphValue::Scalar(if targets(item).is_empty() {
                        CanonicalScalar::Int(9)
                    } else {
                        CanonicalScalar::Null
                    })
                })
                .collect();
            let nested: Vec<_> = items
                .iter()
                .map(|item| GraphValue::List(targets(item).into()))
                .collect();
            let arguments = GqlParameters::new()
                .with_list("items", items.clone())
                .unwrap()
                .with_int64("bias", bias)
                .unwrap();
            let expected = [
                GraphValue::List(filtered.into()),
                GraphValue::List(degrees.into()),
                GraphValue::List(holes.into()),
                GraphValue::List(nested.into()),
            ];
            for (index, template) in templates.iter().enumerate() {
                let query = template.bind_parameters(&arguments).unwrap();
                assert_eq!(
                    query.columns(),
                    if index == 0 {
                        ["filtered", "degrees"]
                    } else {
                        ["holes", "targets"]
                    }
                );
                assert_eq!(
                    run(&query, wide(), &mut || Ok(())).unwrap().value,
                    vec![GraphValueRow::from_owned_values(
                        expected[index * 2..index * 2 + 2].to_vec()
                    )],
                    "bias {bias}, items {items:?}, statement {}",
                    statements[index]
                );
            }
        }
    }
    assert_eq!(
        templates
            .each_ref()
            .map(|template| template.canonical_template_bytes()),
        frozen
    );
}

#[test]
fn list_local_graph_predicates_correlate_both_local_and_outer_identities() {
    let text = "MATCH (person) WITH person,$posts AS posts \
        RETURN person,[p IN posts WHERE NOT p=person AND \
        (p)-[:R]->()<-[:R]-(person)] AS shared, \
        size([p IN posts WHERE NOT p=person AND (p)-[:R]->()<-[:R]-(person)]) AS score \
        ORDER BY person";
    let posts = [4, 2, 3, 2, 1]
        .map(|id| GraphValue::Vertex(VId(id)))
        .to_vec();
    let query = PreparedGraphSetText::prepare_with_parameter_types(
        text,
        &[("posts", GqlParameterType::List)],
        symbols,
    )
    .unwrap()
    .bind_parameters(
        &GqlParameters::new()
            .with_list("posts", posts.clone())
            .unwrap(),
    )
    .unwrap();
    let expected: Vec<_> = (1..=4)
        .map(|id| {
            let shared: Vec<_> = posts
                .iter()
                .filter(|post| {
                    EDGES.iter().any(|&(from, _, tag)| {
                        **post != GraphValue::Vertex(VId(id))
                            && **post == GraphValue::Vertex(from)
                            && EDGES
                                .iter()
                                .any(|&(person, _, interest)| person == VId(id) && interest == tag)
                    })
                })
                .cloned()
                .collect();
            let count = shared.len() as i64;
            GraphValueRow::from_owned_values(vec![
                GraphValue::Vertex(VId(id)),
                GraphValue::List(shared.into()),
                GraphValue::Scalar(CanonicalScalar::Int(count)),
            ])
        })
        .collect();
    // Two :R atoms are DIFFERENT EDGES, which needs each edge's identity.
    assert_eq!(run_identified(&query), expected);
}

#[test]
fn list_local_graph_maps_keep_null_inputs_and_empty_domains_source_free() {
    for (list, expected) in [
        ("null", GraphValue::Scalar(CanonicalScalar::Null)),
        ("[]", GraphValue::List(vec![].into())),
    ] {
        let query = prepare(&format!(
            "WITH {list} AS xs RETURN \
             [x IN xs WHERE (x)-[:R]->() | 1/0] AS mapped"
        ));
        let mut calls = 0;
        let result = query
            .execute_governed(
                wide(),
                |_, _| {
                    calls += 1;
                    Err(GqlQueryError::Source("empty-list-body"))
                },
                || Ok::<_, usize>(()),
            )
            .unwrap();
        assert_eq!(calls, 0, "{list}");
        assert_eq!(
            result.value,
            vec![GraphValueRow::from_owned_values(vec![expected])],
            "{list}"
        );
    }
    let query = prepare(
        "MATCH (n) WHERE n.p=3 WITH collect(n) AS xs \
         RETURN [x IN xs WHERE (x)-[:R]->() | 1/0] AS mapped",
    );
    assert_eq!(
        run(&query, wide(), &mut || Ok(())).unwrap().value,
        vec![GraphValueRow::from_owned_values(vec![GraphValue::List(
            vec![].into()
        )])]
    );
}

#[test]
fn list_local_graph_scopes_restore_outer_names_and_compose_with_calls() {
    let items = vec![GraphValue::Vertex(VId(3)), GraphValue::Vertex(VId(1))];
    let parameters = GqlParameters::new().with_list("items", items).unwrap();
    let expected = vec![GraphValueRow::from_owned_values(vec![
        GraphValue::Scalar(CanonicalScalar::Int(7)),
        GraphValue::List(
            [12, 11]
                .map(|value| GraphValue::Scalar(CanonicalScalar::Int(value)))
                .to_vec()
                .into(),
        ),
    ])];
    for statement in [
        "WITH 7 AS x,$items AS xs,10 AS bias \
         RETURN x,[x IN xs WHERE (x)-[:R]->() | x.p+bias] AS values",
        "WITH 7 AS x,$items AS xs,10 AS bias \
         CALL (xs,bias) { RETURN [x IN xs WHERE (x)-[:R]->() | x.p+bias] AS values } \
         RETURN x,values",
    ] {
        let query = prepare_items(statement)
            .bind_parameters(&parameters)
            .unwrap();
        assert_eq!(
            run(&query, wide(), &mut || Ok(())).unwrap().value,
            expected,
            "{statement}"
        );
    }

    let with_only = prepare_items(
        "UNWIND [$items,[]] AS xs \
         WITH [x IN xs WHERE (x)-[:R]->() | x] AS kept RETURN kept",
    )
    .bind_parameters(&parameters)
    .unwrap();
    assert_eq!(
        run(&with_only, wide(), &mut || Ok(())).unwrap().value,
        vec![
            GraphValueRow::from_owned_values(vec![GraphValue::List(
                vec![GraphValue::Vertex(VId(3)), GraphValue::Vertex(VId(1))].into()
            )]),
            GraphValueRow::from_owned_values(vec![GraphValue::List(vec![].into())]),
        ]
    );

    let skipped = prepare_items(
        "WITH $items AS xs RETURN [x IN xs WHERE false | [(x)-[:R]->() | 1/0]] AS values",
    )
    .bind_parameters(&parameters)
    .unwrap();
    let mut calls = 0;
    let result = skipped
        .execute_governed(
            wide(),
            |_, _| {
                calls += 1;
                Err(GqlQueryError::Source("filtered-out-graph-map"))
            },
            || Ok::<_, usize>(()),
        )
        .unwrap();
    assert_eq!(calls, 0);
    assert_eq!(
        result.value,
        vec![GraphValueRow::from_owned_values(vec![GraphValue::List(
            vec![].into()
        )])]
    );

    for statement in [
        "WITH $items AS xs RETURN [x IN xs WHERE (x)-[:R]->() | x] AS values,x",
        "WITH $items AS xs RETURN [x IN xs WHERE (x)-[:R]->(inside) | inside] AS values",
        "WITH $items AS xs RETURN [x IN xs WHERE (fresh)-[:R]->() | x] AS values",
        "WITH $items AS xs RETURN [x IN xs WHERE (x)-[:R]->() | x] AS values,__fg_boundary_0",
    ] {
        let mut calls = 0;
        assert!(
            PreparedGraphSetText::prepare(statement, |kind, name| {
                calls += 1;
                symbols(kind, name)
            })
            .is_err(),
            "{statement}"
        );
        assert_eq!(
            calls, 0,
            "scope refusal precedes catalog reads: {statement}"
        );
    }
}

#[test]
fn list_local_graph_evaluation_preserves_late_errors_and_cumulative_limits() {
    let parameters = GqlParameters::new()
        .with_list(
            "items",
            [3, 1, 2, 4].map(|id| GraphValue::Vertex(VId(id))).to_vec(),
        )
        .unwrap();
    let bind = |suffix: &str| {
        prepare_items(&format!(
            "WITH $items AS xs RETURN \
             [x IN xs WHERE (x)-[:R]->() | x.p] AS values{suffix}"
        ))
        .bind_parameters(&parameters)
        .unwrap()
    };
    let query = bind(" LIMIT 0");
    let mut calls = 0;
    let result = query.execute_governed(
        wide(),
        |pattern, remaining| {
            calls += 1;
            if calls == 3 {
                return Err(GqlQueryError::Source("later-list-local-source"));
            }
            pattern.plan().execute_governed_with_properties(
                (4 + EDGES.len()) as u64,
                (1..=4).map(VId),
                EDGES,
                |_, _| Ok::<_, &'static str>(true),
                |_, _| Ok(None),
                remaining,
                || Ok::<_, usize>(()),
            )
        },
        || Ok::<_, usize>(()),
    );
    assert_eq!(calls, 3);
    assert!(matches!(
        result,
        Err(GqlQueryError::Source(GraphSetExecutionError::Source(
            "later-list-local-source"
        )))
    ));

    for suffix in ["", " LIMIT 0"] {
        let query = bind(suffix);
        let mut checkpoints = 0;
        let measured = run(&query, wide(), &mut || {
            checkpoints += 1;
            Ok(())
        })
        .unwrap();
        let caps = [
            measured.rows.snapshot_records,
            measured.rows.result_rows,
            measured.evaluator.work_units,
            measured.evaluator.scratch_entries,
        ];
        assert_eq!(
            run(
                &query,
                GqlQueryPolicy::new(caps[0], caps[1], caps[2], caps[3]),
                &mut || Ok(())
            )
            .unwrap(),
            measured
        );
        for dimension in 0..4 {
            if caps[dimension] == 0 {
                continue;
            }
            let mut limited = caps;
            limited[dimension] -= 1;
            assert!(
                run(
                    &query,
                    GqlQueryPolicy::new(limited[0], limited[1], limited[2], limited[3]),
                    &mut || Ok(())
                )
                .is_err(),
                "budget dimension {dimension}, suffix {suffix}"
            );
        }
        assert!(checkpoints > 2);
        for stop in [1, checkpoints / 2, checkpoints] {
            let mut seen = 0;
            let result = run(&query, wide(), &mut || {
                seen += 1;
                if seen == stop { Err(stop) } else { Ok(()) }
            });
            assert!(matches!(result, Err(GqlQueryError::Interrupted(at)) if at == stop));
            assert_eq!(seen, stop);
            assert_eq!(run(&query, wide(), &mut || Ok(())).unwrap(), measured);
        }
    }
}

#[test]
fn list_local_graph_expressions_cross_graph_with_and_grouped_boundaries() {
    let degree = |id| {
        EDGES
            .iter()
            .filter(|&&(from, _, _)| from == VId(id))
            .count() as i64
    };
    let integer = |value| GraphValue::Scalar(CanonicalScalar::Int(value));
    let singleton = |value| GraphValue::List(vec![integer(value)].into());
    let mut expected: Vec<_> = (1..=4)
        .map(|id| GraphValueRow::from_owned_values(vec![singleton(degree(id))]))
        .collect();
    expected.sort();
    for statement in [
        "MATCH (n) RETURN [x IN [n] | size([(x)-->() | 1])] AS l",
        "MATCH (n) WITH n RETURN [x IN [n] | size([(x)-->() | 1])] AS l",
        "MATCH (n) WITH [x IN [n] | size([(x)-->() | 1])] AS l RETURN l",
    ] {
        let mut actual = execute(statement);
        actual.sort();
        assert_eq!(actual, expected, "{statement}");
    }
    let mut grouped =
        execute("MATCH (n) WITH [x IN [n] | size([(x)-->() | 1])] AS l,count(*) AS c RETURN l,c");
    grouped.sort();
    assert_eq!(
        grouped,
        [(0, 1), (1, 2), (3, 1)]
            .map(|(out, count)| GraphValueRow::from_owned_values(vec![
                singleton(out),
                integer(count)
            ]))
            .to_vec()
    );

    let mut mixed = execute(
        "MATCH (n) WITH collect(n) AS nodes UNWIND nodes AS x \
         RETURN x,COUNT { (x)-[:R]->() } AS total, \
         [local IN nodes | COUNT { (local)-[:R]->() }] AS nested ORDER BY x",
    );
    mixed.sort();
    let degrees = GraphValue::List((1..=4).map(|id| integer(degree(id))).collect());
    let expected: Vec<_> = (1..=4)
        .map(|id| {
            GraphValueRow::from_owned_values(vec![
                GraphValue::Vertex(VId(id)),
                integer(degree(id)),
                degrees.clone(),
            ])
        })
        .collect();
    assert_eq!(mixed, expected);
}

#[test]
fn list_local_graph_map_fields_are_admitted_before_chained_writes() {
    let prefix = "CREATE (a) WITH a MATCH (b) WITH collect(b) AS nodes";
    for tail in [
        "RETURN [x IN nodes WHERE (x)-[:R]->() | x.p] AS values",
        "CALL (nodes) { RETURN [x IN nodes WHERE (x)-[:R]->() | x.p] AS values } RETURN values",
    ] {
        let statement = format!("{prefix} {tail}");
        let chain = fgdb_gql::PreparedGraphWriteChainText::prepare(&statement, R, symbols)
            .unwrap()
            .bind_parameters(&GqlParameters::new())
            .unwrap();
        assert_eq!(chain.read_element_keys(), &[P], "{statement}");
    }
}

mod local_path_length_values {
    use fgdb_gql::algebra::{GraphPath, GraphValue, GraphValueRow};
    use fgdb_gql::{
        GqlParameterType, GqlParameters, GqlQueryError, GqlQueryExecution, GqlQueryPolicy,
        GraphIntegerErrorKind, GraphIntegerExpression, GraphIntegerOp, GraphSetExecutionError,
        PreparedGraphSet, PreparedGraphSetText,
    };
    use fgdb_types::{CanonicalScalar, EId, VId};

    fn prepare(text: &str, items: Vec<GraphValue>) -> PreparedGraphSet {
        PreparedGraphSetText::prepare_with_parameter_types(
            text,
            &[("items", GqlParameterType::List)],
            |_: fgdb_gql::GraphSymbolKind, _: &str| None,
        )
        .unwrap()
        .bind_parameters(&GqlParameters::new().with_list("items", items).unwrap())
        .unwrap()
    }

    fn run(
        query: &PreparedGraphSet,
    ) -> Result<
        GqlQueryExecution<GraphValueRow>,
        GqlQueryError<GraphSetExecutionError<&'static str>, usize>,
    > {
        query.execute_governed(
            GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX),
            |_, _| Err(GqlQueryError::Source("unexpected graph read")),
            || Ok::<_, usize>(()),
        )
    }

    fn integer(value: i64) -> GraphValue {
        GraphValue::Scalar(CanonicalScalar::Int(value))
    }

    fn null() -> GraphValue {
        GraphValue::Scalar(CanonicalScalar::Null)
    }

    fn path() -> GraphValue {
        GraphValue::Path(GraphPath::new(
            VId(u128::MAX),
            vec![
                (EId(u128::MAX), VId(2)),
                (EId(u128::MAX - 1), VId(u128::MAX)),
            ]
            .into_boxed_slice(),
        ))
    }

    #[test]
    fn dynamic_length_counts_path_hops_text_characters_and_nulls_in_both_row_scopes() {
        let items = vec![
            path(),
            GraphValue::Path(GraphPath::new(VId(u128::MAX), vec![].into_boxed_slice())),
            GraphValue::Scalar(CanonicalScalar::ucs_basic_text("é🙂").unwrap()),
            null(),
        ];
        let lengths = vec![integer(2), integer(0), integer(2), null()];
        let incremented = vec![integer(3), integer(1), integer(3), null()];
        let local = prepare(
            "WITH $items AS xs RETURN [p IN xs | length(p)] AS lengths, \
             [p IN xs | length(p)+1] AS incremented",
            items.clone(),
        );
        assert_eq!(
            run(&local).unwrap().value,
            vec![GraphValueRow::from_owned_values(vec![
                GraphValue::List(lengths.clone().into_boxed_slice()),
                GraphValue::List(incremented.clone().into_boxed_slice()),
            ])]
        );
        let row = prepare(
            "UNWIND $items AS p RETURN length(p) AS length,length(p)+1 AS incremented",
            items,
        );
        assert_eq!(
            run(&row).unwrap().value,
            lengths
                .into_iter()
                .zip(incremented)
                .map(|(length, incremented)| {
                    GraphValueRow::from_owned_values(vec![length, incremented])
                })
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn dynamic_length_keeps_wrong_types_lazy_and_char_length_text_only() {
        let wrong = vec![
            integer(17),
            GraphValue::Vertex(VId(u128::MAX)),
            GraphValue::Edge(EId(u128::MAX)),
            GraphValue::List(vec![].into_boxed_slice()),
        ];
        let local = prepare(
            "WITH $items AS xs RETURN \
             [p IN xs | CASE WHEN false THEN length(p) ELSE 7 END] AS chosen, \
             [p IN xs | coalesce(9,length(p))] AS present",
            wrong.clone(),
        );
        assert_eq!(
            run(&local).unwrap().value,
            vec![GraphValueRow::from_owned_values(vec![
                GraphValue::List(vec![integer(7); wrong.len()].into_boxed_slice()),
                GraphValue::List(vec![integer(9); wrong.len()].into_boxed_slice()),
            ])]
        );
        let row = prepare(
            "UNWIND $items AS p RETURN \
             CASE WHEN false THEN length(p) ELSE 7 END AS chosen,coalesce(9,length(p)) AS present",
            wrong.clone(),
        );
        assert_eq!(
            run(&row).unwrap().value,
            vec![GraphValueRow::from_owned_values(vec![integer(7), integer(9)]); wrong.len()]
        );
        for (value, kind) in [
            (integer(17), GraphIntegerErrorKind::NonText),
            (GraphValue::Vertex(VId(1)), GraphIntegerErrorKind::NonScalar),
            (GraphValue::Edge(EId(1)), GraphIntegerErrorKind::NonScalar),
            (
                GraphValue::List(vec![].into_boxed_slice()),
                GraphIntegerErrorKind::NonText,
            ),
        ] {
            for text in [
                "WITH $items AS xs RETURN [p IN xs | length(p)] AS lengths LIMIT 0",
                "UNWIND $items AS p RETURN length(p) AS length LIMIT 0",
            ] {
                assert!(
                    matches!(
                        run(&prepare(text, vec![value.clone()])),
                        Err(GqlQueryError::Source(GraphSetExecutionError::Projection { error, .. }))
                            if error.kind == kind
                    ),
                    "{text}: {kind:?}"
                );
            }
        }
        assert!(matches!(
            run(&prepare(
                "WITH $items AS xs RETURN [p IN xs | char_length(p)] AS lengths",
                vec![path()],
            )),
            Err(GqlQueryError::Source(GraphSetExecutionError::Projection { error, .. }))
                if error.kind == GraphIntegerErrorKind::NonText
        ));
        let length = GraphIntegerExpression::prepare_scalar(&[
            GraphIntegerOp::ScalarColumn(0),
            GraphIntegerOp::Length,
        ])
        .unwrap();
        let char_length = GraphIntegerExpression::prepare_scalar(&[
            GraphIntegerOp::ScalarColumn(0),
            GraphIntegerOp::CharLength,
        ])
        .unwrap();
        assert_ne!(length.canonical_bytes(), char_length.canonical_bytes());
    }
}

mod graph_reading_quantifiers {
    use super::*;
    use fgdb_gql::GraphIntegerErrorKind;

    fn truth(value: Option<bool>) -> GraphValue {
        GraphValue::Scalar(value.map_or(CanonicalScalar::Null, CanonicalScalar::Bool))
    }

    fn arguments(items: &[Option<u128>]) -> GqlParameters {
        GqlParameters::new()
            .with_list(
                "items",
                items
                    .iter()
                    .map(|id| id.map_or_else(|| truth(None), |id| GraphValue::Vertex(VId(id))))
                    .collect(),
            )
            .unwrap()
    }

    fn bind(text: &str, parameters: &GqlParameters) -> PreparedGraphSet {
        prepare_items(text).bind_parameters(parameters).unwrap()
    }

    #[test]
    fn graph_quantifiers_keep_three_valued_truth_and_duplicate_members_on_rebind() {
        let templates = ["ANY", "ALL", "NONE", "SINGLE"].map(|kind| {
            prepare_items(&format!(
                "WITH $items AS xs,17 AS marker RETURN marker,\
                 {kind}(x IN xs WHERE CASE WHEN x IS NULL THEN NULL \
                 ELSE EXISTS((x)-[:R]->()) END) AS present"
            ))
        });
        let frozen = templates
            .each_ref()
            .map(|template| template.canonical_template_bytes());
        // Independent truth-table oracles; vertex 1 has three witnesses,
        // while vertex 4 has none. Duplicate members remain distinct.
        type QuantifierCase<'a> = (&'a [Option<u128>], [Option<bool>; 4]);
        let cases: &[QuantifierCase<'_>] = &[
            (&[], [Some(false), Some(true), Some(true), Some(false)]),
            (&[None], [None, None, None, None]),
            (&[Some(1), None], [Some(true), None, Some(false), None]),
            (&[Some(4), None], [None, Some(false), None, None]),
            (
                &[Some(1), Some(4), None, Some(1)],
                [Some(true), Some(false), Some(false), Some(false)],
            ),
            (
                &[Some(1), Some(1)],
                [Some(true), Some(true), Some(false), Some(false)],
            ),
            (
                &[Some(1), Some(4)],
                [Some(true), Some(false), Some(false), Some(true)],
            ),
            (
                &[Some(4), Some(4)],
                [Some(false), Some(false), Some(true), Some(false)],
            ),
        ];
        for (items, expected) in cases {
            let parameters = arguments(items);
            for (index, template) in templates.iter().enumerate() {
                let query = template.bind_parameters(&parameters).unwrap();
                assert_eq!(query.columns(), ["marker", "present"]);
                assert_eq!(
                    run(&query, wide(), &mut || Ok(())).unwrap().value,
                    vec![GraphValueRow::from_owned_values(vec![
                        GraphValue::Scalar(CanonicalScalar::Int(17)),
                        truth(expected[index]),
                    ])],
                    "quantifier {index}, members {items:?}"
                );
            }
        }
        assert_eq!(
            templates
                .each_ref()
                .map(|template| template.canonical_template_bytes()),
            frozen
        );
    }

    #[test]
    fn graph_quantifiers_share_graph_forms_and_skip_empty_or_null_sources() {
        for predicate in [
            "(x)-[:R]->()",
            "EXISTS((x)-[:R]->())",
            "EXISTS { MATCH (x)-[:R]->() }",
            "COUNT { (x)-[:R]->() } > 0",
            "size([(x)-[:R]->(target) | target.p]) > 0",
        ] {
            let statement =
                format!("WITH $items AS xs RETURN SINGLE(x IN xs WHERE {predicate}) AS present");
            let template = prepare_items(&statement);
            for (items, expected) in [
                (vec![Some(1), Some(4)], true),
                (vec![Some(1), Some(1)], false),
            ] {
                let query = template.bind_parameters(&arguments(&items)).unwrap();
                assert_eq!(
                    run(&query, wide(), &mut || Ok(())).unwrap().value,
                    vec![GraphValueRow::from_owned_values(vec![truth(Some(
                        expected
                    ))])],
                    "{statement}, members {items:?}"
                );
            }
        }
        for (kind, empty) in [
            ("ANY", false),
            ("ALL", true),
            ("NONE", true),
            ("SINGLE", false),
        ] {
            for (source, expected) in [("NULL", None), ("[]", Some(empty))] {
                let query = prepare(&format!(
                    "WITH {source} AS xs RETURN {kind}(x IN xs WHERE \
                     CASE WHEN (x)-[:R]->() THEN 1/0=0 ELSE false END) AS present"
                ));
                let mut calls = 0;
                let result = query
                    .execute_governed(
                        wide(),
                        |_, _| {
                            calls += 1;
                            Err(GqlQueryError::Source("empty-quantifier-body"))
                        },
                        || Ok::<_, usize>(()),
                    )
                    .unwrap();
                assert_eq!(calls, 0, "{kind}, {source}");
                assert_eq!(
                    result.value,
                    vec![GraphValueRow::from_owned_values(vec![truth(expected)])],
                    "{kind}, {source}"
                );
            }
        }
    }

    #[test]
    fn graph_quantifiers_cross_match_with_grouping_and_call_boundaries() {
        let expected: Vec<_> = (1..=4)
            .map(|id| {
                GraphValueRow::from_owned_values(vec![
                    GraphValue::Vertex(VId(id)),
                    truth(Some(id != 4)),
                ])
            })
            .collect();
        for statement in [
            "MATCH (n) RETURN n,ANY(x IN [n] WHERE (x)-[:R]->()) AS present ORDER BY n",
            "MATCH (n) WITH n,ANY(x IN [n] WHERE (x)-[:R]->()) AS present \
             RETURN n,present ORDER BY n",
        ] {
            assert_eq!(execute(statement), expected, "{statement}");
        }
        assert_eq!(
            execute(
                "MATCH (n) WHERE ANY(x IN [n] WHERE EXISTS((x)-[:R]->())) \
                 RETURN n ORDER BY n"
            ),
            [1, 2, 3].map(|id| vertices(&[id])).to_vec()
        );
        let mut grouped = execute(
            "MATCH (n) WITH ANY(x IN [n] WHERE size([(x)-->() | 1]) > 0) AS l,\
             count(*) AS c RETURN l,c",
        );
        grouped.sort();
        let mut expected = vec![
            GraphValueRow::from_owned_values(vec![
                truth(Some(false)),
                GraphValue::Scalar(CanonicalScalar::Int(1)),
            ]),
            GraphValueRow::from_owned_values(vec![
                truth(Some(true)),
                GraphValue::Scalar(CanonicalScalar::Int(3)),
            ]),
        ];
        expected.sort();
        assert_eq!(grouped, expected);

        let parameters = arguments(&[Some(1), Some(4)]);
        let with_only = bind(
            "UNWIND [$items,[]] AS xs \
             WITH ANY(x IN xs WHERE EXISTS((x)-[:R]->())) AS present RETURN present",
            &parameters,
        );
        assert_eq!(
            run(&with_only, wide(), &mut || Ok(())).unwrap().value,
            [true, false]
                .map(|present| GraphValueRow::from_owned_values(vec![truth(Some(present))]))
                .to_vec()
        );
        for statement in [
            "WITH 7 AS x,$items AS xs,2 AS minimum \
             RETURN x,SINGLE(x IN xs WHERE COUNT { (x)-[:R]->() } > minimum) AS present",
            "WITH 7 AS x,$items AS xs,2 AS minimum \
             CALL (xs,minimum) { \
               RETURN SINGLE(x IN xs WHERE COUNT { (x)-[:R]->() } > minimum) AS present \
             } RETURN x,present",
        ] {
            let query = bind(statement, &parameters);
            assert_eq!(
                run(&query, wide(), &mut || Ok(())).unwrap().value,
                vec![GraphValueRow::from_owned_values(vec![
                    GraphValue::Scalar(CanonicalScalar::Int(7)),
                    truth(Some(true)),
                ])],
                "{statement}"
            );
        }

        assert_eq!(
            execute(
                "MATCH (n) WITH collect(n) AS nodes UNWIND nodes AS x \
                 WHERE EXISTS((x)-[:R]->()) OR ANY(z IN nodes WHERE EXISTS((z)-[:R]->())) \
                 RETURN x ORDER BY x"
            ),
            [1, 2, 3, 4].map(|id| vertices(&[id])).to_vec()
        );
        assert_eq!(
            execute(
                "MATCH (n) WITH collect(n) AS nodes UNWIND nodes AS x \
                 RETURN x,EXISTS((x)-[:R]->()) AS present,\
                 ANY(local IN nodes WHERE EXISTS((local)-[:R]->())) AS nested ORDER BY x"
            ),
            (1..=4)
                .map(|id| GraphValueRow::from_owned_values(vec![
                    GraphValue::Vertex(VId(id)),
                    truth(Some(id != 4)),
                    truth(Some(true)),
                ]))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn graph_quantifiers_keep_element_names_private_and_require_boolean_results() {
        for statement in [
            "WITH $items AS xs RETURN ANY(x IN xs WHERE EXISTS((x)-[:R]->())) AS present,x",
            "WITH $items AS xs RETURN \
             ANY(x IN xs WHERE EXISTS((x)-[:R]->(inside)) AND inside.p=2) AS present",
            "WITH $items AS xs RETURN ANY(x IN xs WHERE EXISTS((fresh)-[:R]->())) AS present",
            "WITH $items AS xs RETURN \
             ANY(x IN xs WHERE EXISTS((x)-[:R]->())) AS present,__fg_boundary_0",
        ] {
            let mut resolutions = 0;
            assert!(
                PreparedGraphSetText::prepare(statement, |kind, name| {
                    resolutions += 1;
                    symbols(kind, name)
                })
                .is_err(),
                "{statement}"
            );
            assert_eq!(resolutions, 0, "{statement}");
        }
        let template = prepare_items(
            "UNWIND $results AS result WITH result,$items AS xs \
             RETURN ANY(x IN xs WHERE CASE WHEN EXISTS((x)-[:R]->()) \
             THEN result ELSE false END) AS present LIMIT 0",
        );
        let scalar = prepare_items(
            "UNWIND $results AS result WITH result,$items AS xs \
             RETURN ANY(x IN xs WHERE CASE WHEN true THEN result ELSE false END) \
             AS present LIMIT 0",
        );
        for result in [
            GraphValue::Scalar(CanonicalScalar::Int(1)),
            GraphValue::List(vec![].into()),
            GraphValue::Vertex(VId(4)),
            GraphValue::Edge(fgdb_types::EId(1)),
        ] {
            let parameters = arguments(&[Some(1)])
                .with_list("results", vec![result.clone()])
                .unwrap();
            let reference = scalar.bind_parameters(&parameters).unwrap();
            let expected = match run(&reference, wide(), &mut || Ok(())) {
                Err(GqlQueryError::Source(GraphSetExecutionError::Projection {
                    error, ..
                })) => error.kind,
                other => panic!("non-Boolean scalar quantifier accepted {result:?}: {other:?}"),
            };
            if matches!(result, GraphValue::Scalar(CanonicalScalar::Int(_))) {
                assert_eq!(expected, GraphIntegerErrorKind::NonBoolean);
            }
            let query = template.bind_parameters(&parameters).unwrap();
            assert!(
                matches!(
                    run(&query, wide(), &mut || Ok(())),
                    Err(GqlQueryError::Source(GraphSetExecutionError::Projection { error, .. }))
                        if error.kind == expected
                ),
                "{result:?}: {expected:?}"
            );
        }
    }

    #[test]
    fn graph_quantifiers_keep_late_errors_after_decisive_values_and_before_empty_pages() {
        for (kind, items, fail_at) in [
            ("ANY", vec![Some(1), Some(4)], 2),
            ("NONE", vec![Some(1), Some(4)], 2),
            ("ALL", vec![Some(4), Some(1)], 2),
            ("SINGLE", vec![Some(1), Some(1), Some(4)], 3),
        ] {
            for suffix in ["", " LIMIT 0"] {
                let query = bind(
                    &format!(
                        "WITH $items AS xs RETURN \
                         {kind}(x IN xs WHERE EXISTS((x)-[:R]->())) AS present{suffix}"
                    ),
                    &arguments(&items),
                );
                let mut calls = 0;
                let result = query.execute_governed(
                    wide(),
                    |pattern, remaining| {
                        calls += 1;
                        if calls == fail_at {
                            return Err(GqlQueryError::Source("late-quantifier-source"));
                        }
                        pattern.plan().execute_governed_with_properties(
                            (4 + EDGES.len()) as u64,
                            (1..=4).map(VId),
                            EDGES,
                            |_, _| Ok::<_, &'static str>(true),
                            |_, _| Ok(None),
                            remaining,
                            || Ok::<_, usize>(()),
                        )
                    },
                    || Ok::<_, usize>(()),
                );
                assert_eq!(calls, fail_at, "{kind}{suffix}");
                assert!(matches!(
                    result,
                    Err(GqlQueryError::Source(GraphSetExecutionError::Source(
                        "late-quantifier-source"
                    )))
                ));

                let predicate = if kind == "ALL" {
                    "CASE WHEN EXISTS((x)-[:R]->()) THEN 1/0=0 ELSE false END"
                } else {
                    "CASE WHEN EXISTS((x)-[:R]->()) THEN true ELSE 1/0=0 END"
                };
                let query = bind(
                    &format!(
                        "WITH $items AS xs RETURN \
                         {kind}(x IN xs WHERE {predicate}) AS present{suffix}"
                    ),
                    &arguments(&items),
                );
                assert!(matches!(
                    run(&query, wide(), &mut || Ok(())),
                    Err(GqlQueryError::Source(GraphSetExecutionError::Projection { error, .. }))
                        if error.kind == GraphIntegerErrorKind::DivisionByZero
                ));
            }
        }
        let lazy = bind(
            "WITH $items AS xs RETURN ANY(x IN xs WHERE \
             CASE WHEN EXISTS((x)-[:R]->()) THEN true ELSE 1/0=0 END) AS present",
            &arguments(&[Some(1)]),
        );
        assert_eq!(
            run(&lazy, wide(), &mut || Ok(())).unwrap().value,
            vec![GraphValueRow::from_owned_values(vec![truth(Some(true))])]
        );
    }

    #[test]
    fn graph_quantifiers_validate_a_member_before_reading_the_next_one() {
        let query = bind(
            "WITH $items AS xs UNWIND [17] AS result RETURN \
             ANY(x IN xs WHERE CASE WHEN EXISTS((x)-[:R]->()) \
             THEN result ELSE false END) AS present LIMIT 0",
            &arguments(&[Some(1), Some(4)]),
        );
        let mut calls = 0;
        let result = query.execute_governed(
            wide(),
            |pattern, remaining| {
                calls += 1;
                if calls > 1 {
                    return Err(GqlQueryError::Source("later-member-source"));
                }
                pattern.plan().execute_governed_with_properties(
                    (4 + EDGES.len()) as u64,
                    (1..=4).map(VId),
                    EDGES,
                    |_, _| Ok::<_, &'static str>(true),
                    |_, _| Ok(None),
                    remaining,
                    || Ok::<_, usize>(()),
                )
            },
            || Ok::<_, usize>(()),
        );
        assert_eq!(calls, 1);
        assert!(matches!(
            result,
            Err(GqlQueryError::Source(GraphSetExecutionError::Projection { error, .. }))
                if error.kind == GraphIntegerErrorKind::NonBoolean
        ));
    }

    #[test]
    fn graph_quantifiers_keep_one_cumulative_budget_and_restore_frames_after_cancellation() {
        for suffix in ["", " LIMIT 0"] {
            let query = bind(
                &format!(
                    "WITH $items AS xs RETURN \
                     ANY(x IN xs WHERE EXISTS((x)-[:R]->())) AS present{suffix}"
                ),
                &arguments(&[Some(1), Some(4), Some(1)]),
            );
            let mut checkpoints = 0;
            let measured = run(&query, wide(), &mut || {
                checkpoints += 1;
                Ok(())
            })
            .unwrap();
            assert_eq!(
                measured.value,
                if suffix.is_empty() {
                    vec![GraphValueRow::from_owned_values(vec![truth(Some(true))])]
                } else {
                    vec![]
                }
            );
            let caps = [
                measured.rows.snapshot_records,
                measured.rows.result_rows,
                measured.evaluator.work_units,
                measured.evaluator.scratch_entries,
            ];
            assert_eq!(
                run(
                    &query,
                    GqlQueryPolicy::new(caps[0], caps[1], caps[2], caps[3]),
                    &mut || Ok(())
                )
                .unwrap(),
                measured
            );
            for dimension in 0..4 {
                if caps[dimension] == 0 {
                    continue;
                }
                let mut limited = caps;
                limited[dimension] -= 1;
                assert!(
                    run(
                        &query,
                        GqlQueryPolicy::new(limited[0], limited[1], limited[2], limited[3]),
                        &mut || Ok(())
                    )
                    .is_err(),
                    "{suffix}, dimension {dimension}"
                );
            }
            assert!(checkpoints > 2);
            for stop in [checkpoints / 2, checkpoints] {
                let mut seen = 0;
                assert!(matches!(
                    run(&query, wide(), &mut || {
                        seen += 1;
                        if seen == stop { Err(stop) } else { Ok(()) }
                    }),
                    Err(GqlQueryError::Interrupted(at)) if at == stop
                ));
                assert_eq!(seen, stop);
                assert_eq!(run(&query, wide(), &mut || Ok(())).unwrap(), measured);
            }
        }
    }
}

mod graph_reading_list_sources {
    use super::*;
    use fgdb_gql::GraphIntegerErrorKind;

    type Outcome = Result<
        GqlQueryExecution<GraphValueRow>,
        GqlQueryError<GraphSetExecutionError<&'static str>, usize>,
    >;

    fn null() -> GraphValue {
        GraphValue::Scalar(CanonicalScalar::Null)
    }

    fn list(values: impl IntoIterator<Item = GraphValue>) -> GraphValue {
        GraphValue::List(values.into_iter().collect())
    }

    fn parameters(roots: &[Option<u128>]) -> GqlParameters {
        GqlParameters::new()
            .with_list(
                "items",
                roots
                    .iter()
                    .map(|id| id.map_or_else(null, |id| GraphValue::Vertex(VId(id))))
                    .collect(),
            )
            .unwrap()
    }

    fn targets(root: Option<u128>) -> Vec<u128> {
        EDGES
            .iter()
            .filter(|&&(from, _, _)| Some(from.0) == root)
            .map(|&(_, _, to)| to.0)
            .collect()
    }

    fn bind(statement: &str, arguments: &GqlParameters) -> PreparedGraphSet {
        prepare_items(statement).bind_parameters(arguments).unwrap()
    }

    fn counted(query: &PreparedGraphSet, fail_at: Option<usize>) -> (Outcome, usize) {
        let values = [1, 2, 2, 3].map(|n| CanonicalPropertyValue::Scalar(CanonicalScalar::Int(n)));
        let mut calls = 0;
        let result = query.execute_governed_with_elements(
            wide(),
            |pattern, remaining| {
                calls += 1;
                if Some(calls) == fail_at {
                    return Err(GqlQueryError::Source("list-source-error"));
                }
                pattern.plan().execute_governed_with_properties(
                    (values.len() + EDGES.len()) as u64,
                    (1..=4).map(VId),
                    EDGES,
                    |vid, tests| {
                        Ok::<_, &'static str>(tests.iter().all(|test| {
                            test.matches(&[], &[(P, values[vid.0 as usize - 1].clone())])
                        }))
                    },
                    |vid, key| Ok((key == P).then(|| &values[vid.0 as usize - 1])),
                    remaining,
                    || Ok::<_, usize>(()),
                )
            },
            |_, _, _| panic!("this fixture has no CALL source"),
            |element, key| {
                Ok(match element {
                    ElementId::Vertex(vid) if key == P => Some(values[vid.0 as usize - 1].clone()),
                    _ => None,
                })
            },
            || Ok::<_, usize>(()),
        );
        (result, calls)
    }

    #[test]
    fn nested_graph_list_sources_keep_order_duplicates_null_maps_and_filtered_graph_maps() {
        let statements = [
            "WITH $items AS roots RETURN [root IN roots | \
             [x IN [(root)-[:R]->(target) | target] | \
              CASE WHEN (x)-[:R]->() THEN x ELSE NULL END]] AS values",
            "WITH $items AS roots RETURN [root IN roots | \
             [x IN [(root)-[:R]->(target) | target] WHERE (x)-[:R]->() | \
              [(x)-[:R]->(last) | last]]] AS values",
        ];
        let templates = statements.map(prepare_items);
        for roots in [
            vec![Some(3), Some(1), Some(4), None, Some(1), Some(2)],
            vec![Some(2), Some(3)],
            vec![],
        ] {
            let holes = list(roots.iter().map(|&root| {
                list(targets(root).into_iter().map(|id| {
                    if targets(Some(id)).is_empty() {
                        null()
                    } else {
                        GraphValue::Vertex(VId(id))
                    }
                }))
            }));
            let mapped = list(roots.iter().map(|&root| {
                list(
                    targets(root)
                        .into_iter()
                        .filter(|&id| !targets(Some(id)).is_empty())
                        .map(|id| {
                            list(
                                targets(Some(id))
                                    .into_iter()
                                    .map(|id| GraphValue::Vertex(VId(id))),
                            )
                        }),
                )
            }));
            let arguments = parameters(&roots);
            for (index, expected) in [holes, mapped].into_iter().enumerate() {
                let query = templates[index].bind_parameters(&arguments).unwrap();
                assert_eq!(
                    run(&query, wide(), &mut || Ok(())).unwrap().value,
                    vec![GraphValueRow::from_owned_values(vec![expected])],
                    "{}, roots {roots:?}",
                    statements[index]
                );
            }
        }
        // The source reaches vertex 4, but its false graph predicate prevents
        // the map's graph source and arithmetic from being evaluated.
        let filtered = bind(
            "UNWIND $items AS root RETURN \
             [x IN [(root)-[:R]->(target) | target] WHERE (x)-[:R]->() | \
              [(x)-[:R]->() | 1/0]] AS values",
            &parameters(&[Some(3)]),
        );
        let (result, calls) = counted(&filtered, Some(3));
        assert_eq!(calls, 2);
        assert_eq!(
            result.unwrap().value,
            vec![GraphValueRow::from_owned_values(vec![list([])])]
        );
    }

    #[test]
    fn graph_list_sources_resolve_outer_anchors_before_same_named_local_bindings() {
        let roots = [Some(3), Some(1), Some(1), Some(4), None];
        let arguments = parameters(&roots);
        let expected: Vec<_> = roots
            .iter()
            .map(|&root| {
                GraphValueRow::from_owned_values(vec![
                    root.map_or_else(null, |id| GraphValue::Vertex(VId(id))),
                    list(
                        targets(root)
                            .into_iter()
                            .map(|id| GraphValue::Vertex(VId(id))),
                    ),
                ])
            })
            .collect();
        for statement in [
            "UNWIND $items AS x RETURN x,\
             [x IN [(x)-[:R]->(target) | target] | x] AS targets",
            "UNWIND $items AS x CALL (x) { \
               RETURN [x IN [(x)-[:R]->(target) | target] | x] AS targets \
             } RETURN x,targets",
        ] {
            assert_eq!(
                run(&bind(statement, &arguments), wide(), &mut || Ok(()))
                    .unwrap()
                    .value,
                expected,
                "{statement}"
            );
        }
        let query = bind(
            "UNWIND $items AS x RETURN x,\
             ANY(x IN [(x)-[:R]->(target) | target] WHERE (x)-[:R]->()) AS present",
            &arguments,
        );
        assert_eq!(
            run(&query, wide(), &mut || Ok(())).unwrap().value,
            roots
                .iter()
                .map(|&root| GraphValueRow::from_owned_values(vec![
                    root.map_or_else(null, |id| GraphValue::Vertex(VId(id))),
                    GraphValue::Scalar(CanonicalScalar::Bool(
                        targets(root)
                            .into_iter()
                            .any(|id| !targets(Some(id)).is_empty())
                    )),
                ]))
                .collect::<Vec<_>>()
        );

        let expected: Vec<_> = (1..=4)
            .map(|id| {
                GraphValueRow::from_owned_values(vec![
                    GraphValue::Vertex(VId(id)),
                    list(
                        targets(Some(id))
                            .into_iter()
                            .map(|id| GraphValue::Vertex(VId(id))),
                    ),
                ])
            })
            .collect();
        for statement in [
            "MATCH (n) RETURN n,[x IN [(n)-[:R]->(target) | target] | x] AS targets ORDER BY n",
            "MATCH (n) WITH n RETURN n,\
             [x IN [(n)-[:R]->(target) | target] | x] AS targets ORDER BY n",
        ] {
            assert_eq!(execute(statement), expected, "{statement}");
        }
        // This ancestor-owned COUNT counts matching x vertices; parallel
        // edges are only EXISTS witnesses, so vertex 1 contributes two.
        for statement in [
            "MATCH (a) RETURN a,[v IN [a] | COUNT { MATCH (x) \
             WHERE EXISTS { MATCH (a)-[:R]->(x) } }] AS values ORDER BY a",
            "MATCH (a) WITH a RETURN a,[v IN [a] | COUNT { MATCH (x) \
             WHERE EXISTS { MATCH (a)-[:R]->(x) } }] AS values ORDER BY a",
        ] {
            assert_eq!(
                execute(statement),
                [2, 1, 1, 0]
                    .into_iter()
                    .enumerate()
                    .map(|(index, count)| GraphValueRow::from_owned_values(vec![
                        GraphValue::Vertex(VId(index as u128 + 1)),
                        list([GraphValue::Scalar(CanonicalScalar::Int(count))]),
                    ]))
                    .collect::<Vec<_>>(),
                "{statement}"
            );
        }
        let query = bind(
            "UNWIND $items AS root RETURN EXISTS((root)-[:R]->()) AS present,\
             [x IN [(root)-[:R]->(target) | target] | x] AS targets",
            &arguments,
        );
        assert_eq!(
            run(&query, wide(), &mut || Ok(())).unwrap().value,
            roots
                .iter()
                .map(|&root| GraphValueRow::from_owned_values(vec![
                    GraphValue::Scalar(CanonicalScalar::Bool(!targets(root).is_empty())),
                    list(
                        targets(root)
                            .into_iter()
                            .map(|id| GraphValue::Vertex(VId(id))),
                    ),
                ]))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn graph_list_sources_run_once_per_outer_row_and_rebind_source_parameters() {
        let template = PreparedGraphSetText::prepare(
            "UNWIND $items AS anchor RETURN anchor,\
             [x IN [(anchor)-[:R]->(target) WHERE target.p >= $minimum | target] | x] AS targets",
            symbols,
        )
        .unwrap();
        let frozen = template.canonical_template_bytes();
        for roots in [
            vec![Some(1), Some(1), Some(3)],
            vec![Some(2), Some(4)],
            vec![],
        ] {
            for minimum in [2_i64, 3] {
                let arguments = parameters(&roots).with_int64("minimum", minimum).unwrap();
                let query = template.bind_parameters(&arguments).unwrap();
                let (result, calls) = counted(&query, None);
                assert_eq!(calls, roots.len(), "roots {roots:?}, minimum {minimum}");
                assert_eq!(
                    result.unwrap().value,
                    roots
                        .iter()
                        .map(|&root| GraphValueRow::from_owned_values(vec![
                            root.map_or_else(null, |id| GraphValue::Vertex(VId(id))),
                            list(
                                targets(root)
                                    .into_iter()
                                    .filter(|&id| [1_i64, 2, 2, 3][id as usize - 1] >= minimum)
                                    .map(|id| GraphValue::Vertex(VId(id))),
                            ),
                        ]))
                        .collect::<Vec<_>>(),
                    "roots {roots:?}, minimum {minimum}"
                );
            }
        }
        assert_eq!(template.canonical_template_bytes(), frozen);
        assert!(template.bind_parameters(&parameters(&[Some(1)])).is_err());
    }

    #[test]
    fn graph_derived_list_sources_keep_null_empty_and_nested_literal_semantics() {
        let arguments = parameters(&[Some(1), Some(4)]);
        for (statement, expected) in [
            (
                "UNWIND $items AS root RETURN \
                 [x IN CASE WHEN EXISTS((root)-[:R]->()) THEN NULL ELSE [] END | 1/0] AS values",
                vec![null(), list([])],
            ),
            (
                "UNWIND $items AS root RETURN \
                 ANY(x IN CASE WHEN EXISTS((root)-[:R]->()) THEN NULL ELSE [] END \
                     WHERE 1/0=0) AS present",
                vec![null(), GraphValue::Scalar(CanonicalScalar::Bool(false))],
            ),
        ] {
            let (result, calls) = counted(&bind(statement, &arguments), None);
            assert_eq!(calls, 2, "{statement}");
            assert_eq!(
                result.unwrap().value,
                expected
                    .into_iter()
                    .map(|value| GraphValueRow::from_owned_values(vec![value]))
                    .collect::<Vec<_>>(),
                "{statement}"
            );
        }
        for (roots, expected, present) in [
            (
                vec![Some(3), Some(1), Some(4), Some(1), None],
                vec![3, 1, 1],
                true,
            ),
            (vec![], vec![], false),
        ] {
            let arguments = parameters(&roots);
            let query = bind(
                "WITH $items AS xs RETURN \
                 [x IN [[y IN xs WHERE (y)-[:R]->() | y]][0] | x] AS values",
                &arguments,
            );
            assert_eq!(
                run(&query, wide(), &mut || Ok(())).unwrap().value,
                vec![GraphValueRow::from_owned_values(vec![list(
                    expected.iter().map(|&id| GraphValue::Vertex(VId(id)))
                )])]
            );
            let query = bind(
                "WITH $items AS xs RETURN \
                 [x IN [ANY(y IN xs WHERE (y)-[:R]->()),NULL,false] \
                  WHERE x IS NOT NULL | x] AS values",
                &arguments,
            );
            assert_eq!(
                run(&query, wide(), &mut || Ok(())).unwrap().value,
                vec![GraphValueRow::from_owned_values(vec![list([
                    GraphValue::Scalar(CanonicalScalar::Bool(present)),
                    GraphValue::Scalar(CanonicalScalar::Bool(false)),
                ])])]
            );
        }
    }

    #[test]
    fn graph_list_source_and_member_failures_survive_filters_and_empty_pages() {
        for suffix in ["", " LIMIT 0"] {
            for (statement, roots, fail_at) in [
                (
                    "UNWIND $items AS root RETURN \
                     [x IN [(root)-[:R]->(target) | target] | x] AS values",
                    vec![Some(1), Some(3)],
                    2,
                ),
                (
                    "UNWIND $items AS root RETURN \
                     ANY(x IN [(root)-[:R]->(target) | target] \
                         WHERE EXISTS((x)-[:R]->())) AS present",
                    vec![Some(1)],
                    3,
                ),
            ] {
                let query = bind(&format!("{statement}{suffix}"), &parameters(&roots));
                let (result, calls) = counted(&query, Some(fail_at));
                assert_eq!(calls, fail_at, "{statement}{suffix}");
                assert!(matches!(
                    result,
                    Err(GqlQueryError::Source(GraphSetExecutionError::Source(
                        "list-source-error"
                    )))
                ));
            }
            let query = bind(
                &format!(
                    "UNWIND $items AS root RETURN \
                     [x IN [(root)-[:R]->(target) | 1/0] WHERE false | x] AS values{suffix}"
                ),
                &parameters(&[Some(1)]),
            );
            assert!(matches!(
                run(&query, wide(), &mut || Ok(())),
                Err(GqlQueryError::Source(GraphSetExecutionError::Projection { error, .. }))
                    if error.kind == GraphIntegerErrorKind::DivisionByZero
            ));
        }
    }

    #[test]
    fn graph_list_source_helpers_remain_private_without_reserving_public_aliases() {
        for statement in [
            "UNWIND $items AS a WITH a,{} AS m RETURN \
             [x IN [(a)-[:R]->(b) | b] + __fg_boundary_0 | x] AS values",
            "UNWIND $items AS a WITH a,{} AS m RETURN \
             [x IN [(a)-[:R]->(b) | b] + (m{__fg_boundary_0}).__fg_boundary_0 | x] AS values",
            "UNWIND $items AS a RETURN \
             [x IN CASE WHEN EXISTS((a)-[:R]->()) \
                   THEN [__fg_boundary_0{p:1}] ELSE [] END | x] AS values",
        ] {
            let mut resolutions = 0;
            assert!(
                PreparedGraphSetText::prepare(statement, |kind, name| {
                    resolutions += 1;
                    symbols(kind, name)
                })
                .is_err(),
                "{statement}"
            );
            assert_eq!(resolutions, 0, "private scope before catalog: {statement}");
        }

        let roots = [Some(1), Some(4)];
        let arguments = parameters(&roots);
        let expected: Vec<_> = roots
            .iter()
            .map(|&root| {
                GraphValueRow::from_owned_values(vec![list(
                    targets(root)
                        .into_iter()
                        .map(|id| GraphValue::Vertex(VId(id)))
                        .chain([GraphValue::Scalar(CanonicalScalar::Int(9))]),
                )])
            })
            .collect();
        // A public binding with this spelling is legal. Hidden names must
        // avoid it, and both bare lookup and map shorthand retain its value.
        for source in [
            "[(a)-[:R]->(b) | b] + __fg_boundary_0",
            "[(a)-[:R]->(b) | b] + (m{__fg_boundary_0,p:__fg_boundary_0}).p",
        ] {
            let statement = format!(
                "WITH $items AS roots,[9] AS __fg_boundary_0,{{}} AS m \
                 UNWIND roots AS a RETURN [x IN {source} | x] AS values"
            );
            let query = bind(&statement, &arguments);
            assert_eq!(
                run(&query, wide(), &mut || Ok(())).unwrap().value,
                expected,
                "{statement}"
            );
        }
        let query = bind(
            "WITH $items AS roots,{p:7} AS __fg_boundary_0 UNWIND roots AS a RETURN \
             [x IN CASE WHEN EXISTS((a)-[:R]->()) \
                   THEN [__fg_boundary_0{.*}] ELSE [] END | x.p] AS values",
            &arguments,
        );
        assert_eq!(
            run(&query, wide(), &mut || Ok(())).unwrap().value,
            vec![
                GraphValueRow::from_owned_values(vec![list([GraphValue::Scalar(
                    CanonicalScalar::Int(7)
                ),])]),
                GraphValueRow::from_owned_values(vec![list([])]),
            ]
        );
    }
}
