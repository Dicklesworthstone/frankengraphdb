//! Native WITH stages use one GLA source and the existing relational executor.

use fgdb_delta_types::{ElementId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::{
    GraphColumn, GraphPatternBuilder, GraphValueOrder, GraphValueRow, IntegerComparison,
    PreparedGraphPattern,
};
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryExecution, GqlQueryPolicy, GqlScalarParameter,
    GraphIntegerBinary, GraphIntegerExpression, GraphIntegerOp, GraphSetExecutionError,
    GraphSetOperand as Arg, GraphSetPredicateOp as Op, GraphSetProjection, GraphSetQuantifier,
    GraphSetTextErrorKind, GraphSetValue, GraphSymbol, GraphSymbolKind, PreparedGraphSet,
    PreparedGraphSetText,
};
use fgdb_types::{CanonicalPropertyValue, CanonicalScalar, VId};
use std::cell::{Cell, RefCell};
use std::collections::BTreeSet;

const P: PropertyKeyId = PropertyKeyId(1);
const R: RelationId = RelationId(1);
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
fn run(
    query: &PreparedGraphSet,
    values: &[CanonicalScalar],
    policy: GqlQueryPolicy,
    checkpoint: &mut impl FnMut() -> Result<(), usize>,
) -> Result<
    GqlQueryExecution<GraphValueRow>,
    GqlQueryError<GraphSetExecutionError<&'static str>, usize>,
> {
    run_reading(query, values, policy, checkpoint, false)
}
/// As `run`, plus the host's element reader when `elements` (fgdb-xnd7x):
/// `x.p` of a vertex a stage produced reads the value the scan sees.
fn run_reading(
    query: &PreparedGraphSet,
    values: &[CanonicalScalar],
    policy: GqlQueryPolicy,
    checkpoint: &mut impl FnMut() -> Result<(), usize>,
    elements: bool,
) -> Result<
    GqlQueryExecution<GraphValueRow>,
    GqlQueryError<GraphSetExecutionError<&'static str>, usize>,
> {
    let checkpoint = RefCell::new(checkpoint);
    let stored: Vec<CanonicalPropertyValue> = values
        .iter()
        .map(|value| CanonicalPropertyValue::from(value.clone()))
        .collect();
    let patterns = |pattern: &PreparedGraphPattern<GraphValueRow>, remaining| {
        pattern.plan().execute_governed_with_properties(
            values.len() as u64,
            (0..values.len()).map(|at| VId(at as u128)),
            [],
            |vid, predicates| {
                Ok::<_, &'static str>(
                    predicates
                        .iter()
                        .all(|test| test.matches(&[], &[(P, stored[vid.0 as usize].clone())])),
                )
            },
            |vid, _| Ok(Some(&stored[vid.0 as usize])),
            remaining,
            || (checkpoint.borrow_mut())(),
        )
    };
    if !elements {
        return query.execute_governed(policy, patterns, || (checkpoint.borrow_mut())());
    }
    query.execute_governed_with_elements(
        policy,
        patterns,
        |_, _, _| panic!("these laws run no CALL source"),
        |element, key| {
            Ok(match element {
                ElementId::Vertex(vid) if key == P => Some(stored[vid.0 as usize].clone()),
                _ => None,
            })
        },
        || (checkpoint.borrow_mut())(),
    )
}
fn ints(rows: &[GraphValueRow], column: usize) -> Vec<Option<i64>> {
    rows.iter()
        .map(|row| match row.get(column).unwrap().as_scalar() {
            Some(CanonicalScalar::Int(n)) => Some(*n),
            Some(CanonicalScalar::Null) => None,
            _ => panic!("expected nullable integer output"),
        })
        .collect()
}

#[test]
fn imported_nested_map_correlations_share_read_equality_and_preserve_occurrences() {
    let prefix = "UNWIND [{key:{p:2},ordinal:8},{key:{p:1},ordinal:9},\
                         {key:{p:2},ordinal:10},{key:null},{key:{}},\
                         {key:{p:'2'}}] AS row ";
    let output = "RETURN row.ordinal AS ordinal,n.p AS value";
    let map_text = format!("{prefix} MATCH (n {{p:row.key.p}}) {output}");
    let where_text = format!("{prefix} MATCH (n) WHERE row.key.p=n.p {output}");
    let map_template = PreparedGraphSetText::prepare(&map_text, symbols).unwrap();
    let where_template = PreparedGraphSetText::prepare(&where_text, symbols).unwrap();
    assert_eq!(
        map_template.canonical_template_bytes(),
        where_template.canonical_template_bytes()
    );
    let query = map_template.bind_parameters(&GqlParameters::new()).unwrap();
    assert_eq!(query.columns(), &["ordinal", "value"]);
    let result = run(
        &query,
        &[
            CanonicalScalar::Int(1),
            CanonicalScalar::Int(2),
            CanonicalScalar::Int(2),
        ],
        wide(),
        &mut || Ok(()),
    )
    .unwrap();
    assert_eq!(
        ints(&result.value, 0),
        vec![Some(8), Some(8), Some(9), Some(10), Some(10)]
    );
    assert_eq!(
        ints(&result.value, 1),
        vec![Some(2), Some(2), Some(1), Some(2), Some(2)]
    );
    let collision =
        prepare("UNWIND [{p:2}] AS __fg_match_key_1 MATCH (n {p:__fg_match_key_1.p}) RETURN *");
    assert_eq!(collision.columns(), &["__fg_match_key_1", "n"]);
    assert_eq!(
        run(&collision, &[CanonicalScalar::Int(2)], wide(), &mut || Ok(
            ()
        ))
        .unwrap()
        .value
        .len(),
        1
    );
}

#[test]
fn imported_map_field_errors_and_cancellation_do_not_become_empty_answers() {
    let invalid =
        prepare("UNWIND [{key:{p:1}},{key:2}] AS row MATCH (n {p:row.key.p}) RETURN n LIMIT 0");
    assert!(run(&invalid, &[CanonicalScalar::Int(1)], wide(), &mut || Ok(())).is_err());
    let query = prepare("UNWIND [{p:1},{p:1}] AS row MATCH (n {p:row.p}) RETURN n");
    assert!(matches!(
        run(&query, &[CanonicalScalar::Int(1)], wide(), &mut || Err(41)),
        Err(GqlQueryError::Interrupted(41))
    ));
    let calls = Cell::new(0);
    let continued = PreparedGraphSetText::prepare(
        "UNWIND [{p:1}] AS row WITH row MATCH (n {p:row.p}) RETURN n",
        |kind, name| {
            calls.set(calls.get() + 1);
            symbols(kind, name)
        },
    )
    .unwrap()
    .bind_parameters(&GqlParameters::new())
    .unwrap();
    assert_eq!(calls.get(), 1, "one shared property symbol contract");
    assert_eq!(
        run(
            &continued,
            &[CanonicalScalar::Int(1), CanonicalScalar::Int(2)],
            wide(),
            &mut || Ok(())
        )
        .unwrap()
        .value,
        vec![GraphValueRow::from_owned_values(vec![
            fgdb_gql::algebra::GraphValue::Vertex(VId(0))
        ])]
    );
    let continued_invalid = prepare(
        "UNWIND [{key:{p:1}},{key:2}] AS row WITH row MATCH (n {p:row.key.p}) RETURN n LIMIT 0",
    );
    assert!(
        run(
            &continued_invalid,
            &[CanonicalScalar::Int(1)],
            wide(),
            &mut || Ok(())
        )
        .is_err()
    );
    // The existing continuation owner remains available after an explicit
    // projection, including its original ordering and scope boundary.
    let projected = prepare(
        "UNWIND [{p:2},{p:1}] AS row WITH row.p AS wanted MATCH (n {p:wanted}) RETURN wanted",
    );
    assert_eq!(
        ints(
            &run(
                &projected,
                &[CanonicalScalar::Int(1), CanonicalScalar::Int(2)],
                wide(),
                &mut || Ok(())
            )
            .unwrap()
            .value,
            0
        ),
        vec![Some(1), Some(2)]
    );
    let fields = (0..32)
        .map(|key| format!("p{key}:row.p{key}"))
        .collect::<Vec<_>>()
        .join(",");
    let text = format!("UNWIND [{{p0:1}}] AS row MATCH (n {{{fields}}}) RETURN n");
    let calls = Cell::new(0);
    assert!(
        PreparedGraphSetText::prepare(&text, |kind, name| {
            calls.set(calls.get() + 1);
            symbols(kind, name)
        })
        .is_err()
    );
    assert_eq!(calls.get(), 0);
}

#[test]
fn imported_map_keys_keep_the_selective_cross_execution_path() {
    let rows = (0..512)
        .map(|key| {
            fgdb_gql::algebra::GraphValue::map(vec![(
                "key".into(),
                fgdb_gql::algebra::GraphValue::Scalar(CanonicalScalar::Int(key)),
            )])
            .unwrap()
        })
        .collect();
    let arguments = GqlParameters::new().with_list("rows", rows).unwrap();
    let query = PreparedGraphSetText::prepare(
        "UNWIND $rows AS row MATCH (n {p:row.key}) RETURN row.key AS input,n.p AS matched",
        symbols,
    )
    .unwrap()
    .bind_parameters(&arguments)
    .unwrap();
    let source = (0..1024).map(CanonicalScalar::Int).collect::<Vec<_>>();
    let result = run(&query, &source, wide(), &mut || Ok(())).unwrap();
    assert_eq!(
        ints(&result.value, 0),
        (0..512).map(Some).collect::<Vec<_>>()
    );
    assert_eq!(
        ints(&result.value, 1),
        (0..512).map(Some).collect::<Vec<_>>()
    );
    // The existing ordered-probe execution copies selected pairs only. A
    // materialized 512 x 1024 product would exceed this by a wide margin.
    assert!(
        result.evaluator.work_units < 1_000_000,
        "{:?}",
        result.evaluator
    );
}
// The native WITH/RETURN pipeline reads its inputs as scalar columns and
// prepares the expressions as scalars. Since 31408b2c a dynamic column under
// arithmetic stays a scalar load (it may hold a Float), so the typed mirror
// spells it the same way.
fn integer(column: usize, n: i64, op: GraphIntegerBinary) -> GraphSetValue {
    GraphSetValue::Integer(
        GraphIntegerExpression::prepare_scalar(&[
            GraphIntegerOp::ScalarColumn(column),
            GraphIntegerOp::Literal(Some(n)),
            GraphIntegerOp::Binary(op),
        ])
        .unwrap(),
    )
}

#[test]
fn native_pipeline_matches_typed_projection_filter_and_page_composition() {
    let text = "MATCH (n) WITH n AS owner,n.p+1 AS score ORDER BY score DESC LIMIT $top WHERE score > $min WITH owner,score%2 AS bucket RETURN owner,bucket ORDER BY owner";
    let mut calls = BTreeSet::new();
    let template = PreparedGraphSetText::prepare(text, |kind, name| {
        assert!(calls.insert((kind, name.to_owned())));
        symbols(kind, name)
    })
    .unwrap();
    let args = GqlParameters::new()
        .with_uint64("top", 3)
        .unwrap()
        .with_int64("min", 2)
        .unwrap();
    let actual = template.bind_parameters(&args).unwrap();
    let mut builder = GraphPatternBuilder::new();
    builder.vertex("n").unwrap();
    let input: PreparedGraphSet = builder
        .prepare_values(
            &[
                GraphColumn::vertex("n", "n"),
                GraphColumn::property("p", "n", P),
            ],
            0,
            None,
        )
        .unwrap()
        .with_duplicates()
        .into();
    let expected = input
        .project(
            vec![
                GraphSetProjection::new("owner", GraphSetValue::Column(0)),
                GraphSetProjection::new("score", integer(1, 1, GraphIntegerBinary::Add)),
            ],
            GraphSetQuantifier::All,
        )
        .unwrap()
        .with_order_by(&[GraphValueOrder::descending(1)])
        .unwrap()
        .with_page(0, Some(3))
        .filter(&[Op::Compare {
            left: Arg::Column(1),
            comparison: IntegerComparison::Greater,
            right: Arg::Literal(GqlScalarParameter::new(CanonicalScalar::Int(2)).unwrap()),
        }])
        .unwrap()
        .project(
            vec![
                GraphSetProjection::new("owner", GraphSetValue::Column(0)),
                GraphSetProjection::new("bucket", integer(1, 2, GraphIntegerBinary::Remainder)),
            ],
            GraphSetQuantifier::All,
        )
        .unwrap()
        .project(
            vec![
                GraphSetProjection::new("owner", GraphSetValue::Column(0)),
                GraphSetProjection::new("bucket", GraphSetValue::Column(1)),
            ],
            GraphSetQuantifier::All,
        )
        .unwrap()
        .with_order_by(&[GraphValueOrder::ascending(0)])
        .unwrap();
    // Private source aliases are not public schema or logical plan identity.
    assert_eq!(actual.canonical_bytes(), expected.canonical_bytes());
    assert_eq!(template.bind_parameters(&args).unwrap(), actual);
    assert_eq!(calls.len(), 1);
    assert_eq!(template.columns(), &["owner", "bucket"]);
    let values = [1, 2, 3, 4].map(CanonicalScalar::Int);
    let rows = run(&actual, &values, wide(), &mut || Ok(())).unwrap().value;
    assert_eq!(
        rows.iter()
            .map(|row| row.get(0).unwrap().as_vertex().unwrap())
            .collect::<Vec<_>>(),
        vec![VId(1), VId(2), VId(3)]
    );
    assert_eq!(ints(&rows, 1), vec![Some(1), Some(0), Some(1)]);
}

#[test]
fn each_stage_preserves_bags_and_its_page_before_the_next_filter() {
    let values = [1, 2, 2, 3, 4].map(CanonicalScalar::Int);
    for (text, expected) in [
        ("MATCH (n) WITH n.p AS p RETURN p", vec![1, 2, 2, 3, 4]),
        (
            "MATCH (n) WITH DISTINCT n.p AS p RETURN p",
            vec![1, 2, 3, 4],
        ),
        (
            "MATCH (n) WITH n.p AS p ORDER BY p DESC LIMIT 2 WHERE p < 4 RETURN p",
            vec![3],
        ),
        (
            "MATCH (n) WITH n.p AS p WHERE p < 4 WITH p ORDER BY p DESC LIMIT 2 RETURN p",
            vec![2, 3],
        ),
        (
            "MATCH (n) WITH n.p AS p WITH 7 AS x RETURN x",
            vec![7, 7, 7, 7, 7],
        ),
        (
            "MATCH (n) WITH n.p AS p WITH DISTINCT 7 AS x RETURN x",
            vec![7],
        ),
    ] {
        let query = prepare(text);
        let result = run(&query, &values, wide(), &mut || Ok(())).unwrap();
        assert_eq!(result.rows.snapshot_records, values.len() as u64);
        assert_eq!(
            ints(&result.value, 0),
            expected.into_iter().map(Some).collect::<Vec<_>>(),
            "{text}"
        );
    }
}

#[test]
fn with_expressions_require_explicit_names_before_catalog_access() {
    for (text, expression) in [
        ("MATCH (n) WITH n.p RETURN p", "n.p"),
        ("MATCH (n) WITH ((n.p)) RETURN p", "((n.p))"),
        ("MATCH (n) WITH labels(n) RETURN n", "labels(n)"),
        ("MATCH (n) WITH properties(n) RETURN n", "properties(n)"),
        (
            "MATCH (n) WITH labels /* function trivia */ (n) RETURN n",
            "labels",
        ),
        ("MATCH (a)-[e:R]->(b) WITH type(e) RETURN e", "type(e)"),
        (
            "MATCH p = (a)-[:R]->{1,2}(b) WITH nodes(p) RETURN p",
            "nodes(p)",
        ),
        ("UNWIND [1] AS k MATCH (n) WITH n.p RETURN p", "n.p"),
        ("MATCH (n) WITH n MATCH (n) WITH n.p RETURN p", "n.p"),
        (
            "MATCH (n) WITH n MATCH (n)-[e:R]->(m) WITH type(e) RETURN e",
            "type(e)",
        ),
        ("MATCH (n) WITH n.p, count(*) AS c RETURN p,c", "n.p"),
        (
            "MATCH (n) WITH properties(n),count(*) AS c RETURN n,c",
            "properties(n)",
        ),
        ("MATCH (n) WITH count(*) RETURN count", "count(*)"),
        (
            "MATCH (n) WITH n.p AS x WITH x,count(*) RETURN x,count",
            "count(*)",
        ),
        ("UNWIND [1,2] AS x WITH sum(x) RETURN sum", "sum(x)"),
        ("WITH count(*) RETURN count", "count(*)"),
        (
            "\u{2003}MATCH (private:PrivateLabel) WITH private.secret /* λ */ RETURN secret LIMIT 0",
            "private.secret",
        ),
    ] {
        let calls = Cell::new(0);
        let error = PreparedGraphSetText::prepare(text, |kind, name| {
            calls.set(calls.get() + 1);
            symbols(kind, name)
        })
        .unwrap_err();
        assert!(
            matches!(
                error.kind,
                GraphSetTextErrorKind::Expected(
                    "AS alias for a computed WITH value" | "AS alias for a computed row value"
                )
            ),
            "{text}: {error:?}"
        );
        assert_eq!(error.offset, text.find(expression).unwrap(), "{text}");
        assert_eq!(calls.get(), 0, "{text}");
        let diagnostic = format!("{error:?} {error}");
        assert!(!diagnostic.contains("PrivateLabel"));
        assert!(!diagnostic.contains("private.secret"));
    }
}

#[test]
fn named_with_expressions_and_parenthesized_variables_preserve_results() {
    let values = [1, 2, 2, 3].map(CanonicalScalar::Int);
    for text in [
        "MATCH (n) WITH n.p AS value RETURN value ORDER BY value",
        "MATCH (n) WITH ((n)) RETURN n.p AS value ORDER BY value",
        "MATCH (n) WITH n.p AS value WITH ((value)) RETURN value ORDER BY value",
        "MATCH (n) WITH n.p AS value WITH value /* λ */ RETURN value ORDER BY value",
        "UNWIND [1] AS k MATCH (n) WITH n.p AS value,k RETURN value ORDER BY value",
        "MATCH (n) WITH n MATCH (n) WITH n.p AS value RETURN value ORDER BY value",
    ] {
        let query = prepare(text);
        assert_eq!(query.columns(), ["value"], "{text}");
        let rows = run_reading(&query, &values, wide(), &mut || Ok(()), true)
            .unwrap()
            .value;
        assert_eq!(
            ints(&rows, 0),
            [Some(1), Some(2), Some(2), Some(3)],
            "{text}"
        );
    }
    let grouped =
        prepare("MATCH (n) WITH n.p AS value,count(*) AS count RETURN value,count ORDER BY value");
    let rows = run(&grouped, &values, wide(), &mut || Ok(()))
        .unwrap()
        .value;
    assert_eq!(ints(&rows, 0), [Some(1), Some(2), Some(3)]);
    assert_eq!(ints(&rows, 1), [Some(1), Some(2), Some(1)]);
    // A named WITH item after UNWIND ... MATCH is a whole expression: a
    // comparison against the unwound column is the item's Boolean.
    let compared = prepare("UNWIND [1] AS k MATCH (n) WITH n.p = k + 1 AS two RETURN two");
    let rows = run(&compared, &values, wide(), &mut || Ok(()))
        .unwrap()
        .value;
    let flags: Vec<_> = rows
        .iter()
        .map(|row| match row.get(0).unwrap().as_scalar() {
            Some(CanonicalScalar::Bool(flag)) => *flag,
            other => panic!("expected a Boolean, found {other:?}"),
        })
        .collect();
    assert_eq!(flags.len(), 4);
    assert_eq!(flags.iter().filter(|flag| **flag).count(), 2, "{flags:?}");
    let implicit = prepare("UNWIND [1,2] AS x WITH ((x)) RETURN x");
    let explicit = prepare("UNWIND [1,2] AS x WITH x AS x RETURN x");
    assert_eq!(implicit.canonical_bytes(), explicit.canonical_bytes());
}

/// IS [NOT] NULL over a vertex binding, in a graph RETURN and over a carried
/// WITH column, observes only whether the element is NULL (TCK Return3 [1]).
#[test]
fn a_vertex_binding_answers_is_null() {
    let values = [1, 2, 2, 3].map(CanonicalScalar::Int);
    for (text, expected) in [
        ("MATCH (n) RETURN n IS NOT NULL AS present", true),
        ("MATCH (n) RETURN n IS NULL AS absent", false),
        (
            "MATCH (n) WITH n RETURN n IS NULL OR n.p > 9 AS absent",
            false,
        ),
    ] {
        let query = prepare(text);
        let rows = run(&query, &values, wide(), &mut || Ok(())).unwrap().value;
        assert_eq!(rows.len(), 4, "{text}");
        for row in &rows {
            assert_eq!(
                row.get(0).unwrap().as_scalar(),
                Some(&CanonicalScalar::Bool(expected)),
                "{text}"
            );
        }
    }
}

/// An UNWIND right after MATCH or WITH reads the carried vertex's properties
/// through the same hidden boundary columns as a WITH's own scope. They end
/// right after the UNWIND, which keeps its unwound column (TCK Graph8 reads
/// `UNWIND keys(n)` the same way).
#[test]
fn an_unwind_after_match_or_with_reads_carried_properties() {
    let values = [1, 2, 2, 3].map(CanonicalScalar::Int);
    for text in [
        "MATCH (n) UNWIND [n.p, n.p + 1] AS v RETURN v ORDER BY v",
        "MATCH (n) WITH n UNWIND [n.p, n.p + 1] AS v RETURN v ORDER BY v",
    ] {
        let query = prepare(text);
        assert_eq!(query.columns(), ["v"], "{text}");
        let rows = run(&query, &values, wide(), &mut || Ok(())).unwrap().value;
        assert_eq!(ints(&rows, 0), [1, 2, 2, 2, 3, 3, 3, 4].map(Some), "{text}");
    }
    // A binding a plain WITH drops stays out of the UNWIND's reach.
    assert!(
        PreparedGraphSetText::prepare(
            "MATCH (n) WITH n.p AS p UNWIND [n.p] AS v RETURN v",
            symbols
        )
        .is_err()
    );
}

/// A list-typed WITH column compared with `=` or an ordering operator is
/// one value-language comparison, as a list literal's is (fgdb-9cawv): TCK
/// WithOrderBy1 [45] ends `RETURN orderedX = range(0, numOfValues - 1)`.
#[test]
fn list_columns_compare_in_the_value_language() {
    let values = [1, 2].map(CanonicalScalar::Int);
    let query = prepare(
        "MATCH (n) WITH n.p AS p, [n.p, n.p + 1] AS l \
         RETURN p, l = range(p, p + 1) AS same, l < [p, p + 2] AS before, \
         l > [p, p + 2] AS after ORDER BY p",
    );
    let rows = run(&query, &values, wide(), &mut || Ok(())).unwrap().value;
    assert_eq!(rows.len(), 2);
    for row in &rows {
        let flags: Vec<_> = (1..4)
            .map(|at| row.get(at).unwrap().as_scalar().cloned())
            .collect();
        assert_eq!(
            flags,
            [true, true, false].map(|flag| Some(CanonicalScalar::Bool(flag)))
        );
    }
    // The TCK sort-consistency shape: x counts the values below each value,
    // so collecting x in value order gives 0..n-1.
    let equal = prepare(
        "WITH [3, 1, 2] AS values WITH values, size(values) AS numOfValues \
         UNWIND values AS value \
         WITH size([x IN values WHERE x < value]) AS x, value, numOfValues ORDER BY value \
         WITH numOfValues, collect(x) AS orderedX \
         RETURN orderedX = range(0, numOfValues - 1) AS equal",
    );
    let rows = run(&equal, &values, wide(), &mut || Ok(())).unwrap().value;
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].get(0).unwrap().as_scalar(),
        Some(&CanonicalScalar::Bool(true))
    );
}

/// `+` concatenates list-typed WITH columns (openCypher Unwind1 [3],
/// fgdb-c4i0d), in a RETURN item and as an UNWIND source.
/// A NULL slice bound makes the slice NULL (openCypher List2 [9]). A lower
/// NULL bound is the literal before `..`, not a property read of a variable
/// named `null`.
#[test]
fn a_null_slice_bound_reads_null() {
    let values = [1].map(CanonicalScalar::Int);
    for bounds in ["null..null", "1..null", "null..3", "..null", "null.."] {
        let text = format!("WITH [1, 2, 3] AS list RETURN list[{bounds}] AS r");
        let rows = run(&prepare(&text), &values, wide(), &mut || Ok(()))
            .unwrap()
            .value;
        assert_eq!(rows.len(), 1, "{text}");
        assert_eq!(
            rows[0].get(0).unwrap().as_scalar(),
            Some(&CanonicalScalar::Null),
            "{text}"
        );
    }
    let rows = run(
        &prepare("WITH [1, 2, 3] AS list RETURN list[1..] AS r"),
        &values,
        wide(),
        &mut || Ok(()),
    )
    .unwrap()
    .value;
    assert_eq!(rows[0].get(0).unwrap().as_list().map(<[_]>::len), Some(2));
}

#[test]
fn plus_concatenates_list_columns_after_with() {
    use fgdb_gql::algebra::GraphValue;
    let values = [1, 2].map(CanonicalScalar::Int);
    let int = |n: i64| GraphValue::Scalar(CanonicalScalar::Int(n));
    let query = prepare(
        "MATCH (n) WITH [n.p] AS first, [n.p, 9] AS second \
         RETURN first + second AS joined ORDER BY joined",
    );
    let rows = run(&query, &values, wide(), &mut || Ok(())).unwrap().value;
    let joined: Vec<_> = rows.iter().map(|row| row.get(0).unwrap().clone()).collect();
    assert_eq!(
        joined,
        [
            GraphValue::List(vec![int(1), int(1), int(9)].into()),
            GraphValue::List(vec![int(2), int(2), int(9)].into()),
        ]
    );
    let query = prepare(
        "MATCH (n) WITH [n.p] AS first, [n.p + 10] AS second \
         UNWIND (first + second) AS x RETURN x ORDER BY x",
    );
    let rows = run(&query, &values, wide(), &mut || Ok(())).unwrap().value;
    assert_eq!(ints(&rows, 0), [1, 2, 11, 12].map(Some));
    // The Quantifier9 to 12 shape (fgdb-13l85): a CASE choosing between a
    // list column and its reverse, then appending a scalar column.
    let query = prepare(
        "MATCH (n) WITH [n.p, n.p + 1] AS list, n.p AS x \
         RETURN CASE WHEN x > 1 THEN reverse(list) ELSE list END + x AS joined \
         ORDER BY joined",
    );
    let rows = run(&query, &values, wide(), &mut || Ok(())).unwrap().value;
    let joined: Vec<_> = rows.iter().map(|row| row.get(0).unwrap().clone()).collect();
    assert_eq!(
        joined,
        [
            GraphValue::List(vec![int(1), int(2), int(1)].into()),
            GraphValue::List(vec![int(3), int(2), int(2)].into()),
        ]
    );
}

/// A CASE after WITH may return a graph element: the arm's vertex itself,
/// not a scalar, in both the searched and the simple form. A missing ELSE is
/// NULL. Each row is compared with a plain `RETURN x, n` over the same rows.
#[test]
fn case_returns_the_vertex_a_with_row_carries() {
    use fgdb_gql::algebra::GraphValue;
    let values = [1, 2, 3].map(CanonicalScalar::Int);
    let rows = |text: &str| {
        run(&prepare(text), &values, wide(), &mut || Ok(()))
            .unwrap()
            .value
    };
    let plain = rows("MATCH (n) WITH n, n.p AS x RETURN x, n ORDER BY x");
    assert_eq!(ints(&plain, 0), [1, 2, 3].map(Some));
    for text in [
        "MATCH (n) WITH n, n.p AS x RETURN x, CASE WHEN x > 1 THEN n END AS chosen ORDER BY x",
        "MATCH (n) WITH n, n.p AS x RETURN x, CASE WHEN x > 1 THEN n ELSE null END AS chosen ORDER BY x",
        "MATCH (n) WITH n, n.p AS x RETURN x, CASE x WHEN 1 THEN null ELSE n END AS chosen ORDER BY x",
    ] {
        let chosen = rows(text);
        assert_eq!(ints(&chosen, 0), [1, 2, 3].map(Some), "{text}");
        assert!(chosen[0].get(1).unwrap().is_null(), "{text}");
        for (row, expected) in chosen.iter().zip(&plain).skip(1) {
            let value = row.get(1).unwrap();
            assert!(matches!(value, GraphValue::Vertex(_)), "{text}: {value:?}");
            assert_eq!(value, expected.get(1).unwrap(), "{text}");
        }
    }
}

/// openCypher rand() (fgdb-13l85): a float in [0, 1), drawn as a pure
/// function of its call site, the row's ordinal in its projection stage and
/// the comprehension elements in force. Rows, sites and elements draw
/// differently, a rerun draws the same, and a context that numbers no rows
/// (an aggregate output) refuses instead of guessing a key.
#[test]
fn rand_draws_unit_floats_keyed_by_row_site_and_element() {
    use fgdb_gql::algebra::GraphValue;
    let values = [1, 2, 3, 4].map(CanonicalScalar::Int);
    let float = |value: &GraphValue| match value {
        GraphValue::Scalar(CanonicalScalar::Float(value)) => value.get(),
        _ => panic!("rand() is a float"),
    };
    let rows = |text: &str| {
        run(&prepare(text), &values, wide(), &mut || Ok(()))
            .unwrap()
            .value
    };
    let unit = |draw: f64| (0.0..1.0).contains(&draw);

    let text = "MATCH (n) RETURN n.p AS p, rand() AS a, rand() AS b ORDER BY p";
    let first = rows(text);
    assert_eq!(first.len(), 4);
    assert_eq!(rows(text), first, "a rerun draws the same");
    let draws: Vec<(f64, f64)> = first
        .iter()
        .map(|row| (float(row.get(1).unwrap()), float(row.get(2).unwrap())))
        .collect();
    assert!(draws.iter().all(|&(a, b)| unit(a) && unit(b)), "{draws:?}");
    assert!(
        draws.windows(2).any(|pair| pair[0].0 != pair[1].0),
        "rows differ"
    );
    assert!(draws.iter().any(|&(a, b)| a != b), "sites differ");

    // Inside a comprehension each element draws apart.
    for row in
        rows("MATCH (n) WITH n.p AS p RETURN p, [y IN range(1, 16) | rand()] AS d ORDER BY p")
    {
        let GraphValue::List(list) = row.get(1).unwrap() else {
            panic!("a list of draws");
        };
        let list: Vec<f64> = list.iter().map(float).collect();
        assert_eq!(list.len(), 16);
        assert!(list.iter().copied().all(unit), "{list:?}");
        assert!(list.windows(2).any(|pair| pair[0] != pair[1]), "{list:?}");
    }
    assert_eq!(
        ints(
            &rows(
                "MATCH (n) WITH n.p AS p RETURN size([y IN range(1, 16) WHERE rand() < 2.0 | y]) \
                 AS kept, size([y IN range(1, 16) WHERE rand() < 0.0 | y]) AS dropped ORDER BY kept"
            ),
            0
        ),
        [16, 16, 16, 16].map(Some)
    );

    let refused =
        PreparedGraphSetText::prepare("MATCH (n) RETURN rand() AS r, count(*) AS c", symbols)
            .ok()
            .and_then(|template| template.bind_parameters(&GqlParameters::new()).ok())
            .map(|query| run(&query, &values, wide(), &mut || Ok(())));
    assert!(
        !matches!(refused, Some(Ok(_))),
        "an aggregate output numbers no rows"
    );
    // openCypher refuses rand() inside an aggregate's argument at compile
    // time (Return6 [15], NonConstantExpression); an aggregate over a value
    // drawn by an earlier stage is ordinary. As the native dispatcher does,
    // a statement is accepted when any read front end accepts it, and a
    // refused one names the non-constant argument as the reason.
    let fronts = |text: &str| -> (bool, String) {
        let results = [
            PreparedGraphSetText::prepare(text, symbols)
                .map(drop)
                .map_err(|error| format!("{error:?}")),
            fgdb_gql::PreparedGraphAggregateText::prepare(text, symbols)
                .map(drop)
                .map_err(|error| format!("{error:?}")),
            fgdb_gql::PreparedGraphPipelineAggregateText::prepare(text, symbols)
                .map(drop)
                .map_err(|error| format!("{error:?}")),
        ];
        let accepted = results.iter().any(Result::is_ok);
        let errors: Vec<_> = results.into_iter().filter_map(Result::err).collect();
        (accepted, errors.join(" | "))
    };
    for text in [
        "RETURN count(rand()) AS c",
        "MATCH (n) RETURN count(rand()) AS c",
        "MATCH (n) RETURN sum(n.p * rand()) AS s",
        "MATCH (n) WITH count(rand()) AS c RETURN c",
        "UNWIND [1, 2] AS x WITH sum(x * rand()) AS s RETURN s",
    ] {
        let (accepted, errors) = fronts(text);
        assert!(!accepted, "{text}");
        assert!(
            errors.contains("non-constant expression"),
            "{text}: {errors}"
        );
    }
    for text in [
        "RETURN count(1) AS c",
        "MATCH (n) RETURN sum(n.p) AS s",
        "MATCH (n) WITH rand() AS r RETURN count(r) AS c",
        "UNWIND [1, 2] AS x WITH sum(x) AS s RETURN s",
    ] {
        assert!(fronts(text).0, "{text}: {}", fronts(text).1);
    }
}

/// A grouping or DISTINCT WITH's own ORDER BY may repeat one of its items'
/// expressions, which then is that item's output (openCypher, TCK
/// WithOrderBy2 [22] to [24] and WithOrderBy4 [16] to [18]): `n.p` after
/// `n.p AS p`, inside a larger key too, and an aggregate call after the same
/// call. A repeat is a name, `name.key` or one call; any other expression,
/// and the WITH's WHERE, still see only the projected names.
#[test]
fn a_grouping_or_distinct_with_orders_by_a_repeat_of_its_item() {
    let values = [1, 2, 2, 3].map(CanonicalScalar::Int);
    for (text, expected) in [
        (
            "MATCH (n) WITH n.p AS p, count(*) AS c ORDER BY n.p DESC LIMIT 1 RETURN p, c",
            vec![(Some(3), Some(1))],
        ),
        (
            "MATCH (n) WITH n.p AS p, count(*) AS c ORDER BY n.p + 10 LIMIT 1 RETURN p, c",
            vec![(Some(1), Some(1))],
        ),
        (
            "MATCH (n) WITH n.p AS p, count(*) AS c ORDER BY count(*) DESC, p LIMIT 1 \
             RETURN p, c",
            vec![(Some(2), Some(2))],
        ),
        // RETURN without ORDER BY gives canonical order, so a WITH's ORDER BY
        // shows in the rows its LIMIT keeps: p - c is 2, 0, 0 for p = 3, 1, 2.
        (
            "MATCH (n) WITH n.p AS p, count(*) AS c ORDER BY p - count(*) DESC, p LIMIT 2 \
             RETURN p, c",
            vec![(Some(1), Some(1)), (Some(3), Some(1))],
        ),
        (
            "MATCH (n) WITH DISTINCT n.p AS p, 0 AS c ORDER BY n.p DESC LIMIT 2 RETURN p, c",
            vec![(Some(2), Some(0)), (Some(3), Some(0))],
        ),
    ] {
        let query = prepare(text);
        let rows = run(&query, &values, wide(), &mut || Ok(())).unwrap().value;
        let found: Vec<_> = ints(&rows, 0).into_iter().zip(ints(&rows, 1)).collect();
        assert_eq!(found, expected, "{text}");
    }
    for text in [
        // Not an item: the dropped binding stays out of scope.
        "MATCH (n) WITH n.p AS p, count(*) AS c ORDER BY n.q RETURN p",
        "MATCH (n) WITH n.p + 1 AS q, count(*) AS c ORDER BY n.p RETURN q",
        // The WITH's WHERE reads its projected names only.
        "MATCH (n) WITH n.p AS p, count(*) AS c WHERE n.p > 1 RETURN p",
        // A later stage sees the projected row, not the item expressions.
        "MATCH (n) WITH n.p AS p, count(*) AS c WITH p, c ORDER BY n.p RETURN p",
    ] {
        let calls = Cell::new(0);
        assert!(
            PreparedGraphSetText::prepare(text, |kind, name| {
                calls.set(calls.get() + 1);
                symbols(kind, name)
            })
            .is_err(),
            "{text}"
        );
        assert_eq!(calls.get(), 0, "{text}");
    }
}

#[test]
fn alias_arithmetic_case_and_coalesce_reuse_checked_lazy_execution() {
    let values = [
        CanonicalScalar::Int(0),
        CanonicalScalar::Int(2),
        CanonicalScalar::Null,
    ];
    let query = prepare(
        "MATCH (n) WITH n,n.p AS value WITH n,CASE WHEN value IS NULL THEN 7 WHEN value=0 THEN 99 ELSE 10/value END AS score RETURN n,COALESCE(score,1/0) AS result ORDER BY n",
    );
    assert_eq!(
        ints(
            &run(&query, &values, wide(), &mut || Ok(())).unwrap().value,
            1
        ),
        vec![Some(99), Some(5), Some(7)]
    );
    let safe = prepare("MATCH (n) WITH n.p AS value WHERE value <> 0 RETURN 10/value AS result");
    assert_eq!(
        ints(
            &run(&safe, &values, wide(), &mut || Ok(())).unwrap().value,
            0
        ),
        vec![Some(5)]
    );
    let unsafe_page = prepare("MATCH (n) WITH n.p AS value RETURN 10/value AS result LIMIT 0");
    assert!(matches!(
        run(&unsafe_page, &values, wide(), &mut || Ok(())),
        Err(GqlQueryError::Source(
            GraphSetExecutionError::Projection { .. }
        ))
    ));
}

#[test]
fn nullable_aliases_and_boolean_filters_do_not_turn_unknown_into_true() {
    let values = [
        CanonicalScalar::Null,
        CanonicalScalar::Int(1),
        CanonicalScalar::Int(2),
        CanonicalScalar::Bool(true),
    ];
    let query =
        prepare("MATCH (n) WITH n,n.p AS value WHERE NOT (value=1 OR value IS NULL) RETURN n");
    let result = run(&query, &values, wide(), &mut || Ok(())).unwrap();
    // TRUE is of another domain than 1 (fgdb-v6ku6): `value=1` is FALSE
    // there, not UNKNOWN, so NOT keeps vertex 3 as it keeps vertex 2.
    let mut kept = result
        .value
        .iter()
        .map(|row| row.get(0).unwrap().as_vertex())
        .collect::<Vec<_>>();
    kept.sort();
    assert_eq!(kept, vec![Some(VId(2)), Some(VId(3))]);
    let absent =
        prepare("MATCH (n) OPTIONAL MATCH (n)-[:R]->(m) WITH n,m WHERE m IS NULL RETURN n,m");
    let result = run(&absent, &values, wide(), &mut || Ok(())).unwrap();
    assert_eq!(result.value.len(), 4);
    assert!(result.value.iter().all(|row| row.get(1).unwrap().is_null()));
}

#[test]
fn computed_with_predicates_filter_the_projected_rows() {
    // 15c37d16 made computed WITH predicates and Boolean row aliases legal;
    // both forms used to be on the refusal list below. A NULL input makes the
    // comparison unknown, and unknown is filtered out, never kept.
    let values = [
        CanonicalScalar::Int(-3),
        CanonicalScalar::Int(-1),
        CanonicalScalar::Int(0),
        CanonicalScalar::Int(4),
        CanonicalScalar::Null,
    ];
    let query = prepare("MATCH (n) WITH n.p AS p WHERE p+1>0 RETURN p");
    let result = run(&query, &values, wide(), &mut || Ok(())).unwrap();
    let mut kept = ints(&result.value, 0);
    kept.sort();
    assert_eq!(kept, vec![Some(0), Some(4)]);

    // A Boolean row alias is a predicate too: only true survives, and a
    // non-Boolean value is a typed execution error, never coerced.
    let alias = prepare("MATCH (n) WITH n.p AS p WHERE p RETURN p");
    let booleans = [
        CanonicalScalar::Bool(true),
        CanonicalScalar::Bool(false),
        CanonicalScalar::Null,
    ];
    let result = run(&alias, &booleans, wide(), &mut || Ok(())).unwrap();
    assert_eq!(result.value.len(), 1);
    assert_eq!(
        result.value[0].get(0).unwrap().as_scalar(),
        Some(&CanonicalScalar::Bool(true))
    );
    assert!(run(&alias, &[CanonicalScalar::Int(1)], wide(), &mut || Ok(())).is_err());
}

/// `WITH n WHERE n.p ...` reads the property through a hidden column of the
/// graph-to-row boundary. Its rows equal the MATCH-level WHERE under the
/// binding's own name, an alias, DISTINCT and a page after the WHERE, and the
/// hidden column never reaches RETURN *, a later WITH *, or a lookup by its
/// private name. A later stage reads through the host's element reader.
#[test]
fn a_with_where_reads_properties_of_projected_vertices_through_hidden_boundary_columns() {
    let values = [1, 2, 2, 3, 4].map(CanonicalScalar::Int);
    let rows = |text: &str| {
        run(&prepare(text), &values, wide(), &mut || Ok(()))
            .unwrap()
            .value
    };
    let vertex = |vid: u128| {
        GraphValueRow::from_owned_values(vec![fgdb_gql::algebra::GraphValue::Vertex(VId(vid))])
    };
    assert_eq!(
        rows("MATCH (n) WITH n WHERE n.p = 2 RETURN n ORDER BY n"),
        vec![vertex(1), vertex(2)]
    );
    for (boundary, pattern) in [
        (
            "MATCH (n) WITH n WHERE n.p = 2 RETURN n ORDER BY n",
            "MATCH (n) WHERE n.p = 2 RETURN n ORDER BY n",
        ),
        (
            "MATCH (n) WITH n AS m WHERE m.p > 1 AND m.p < 4 RETURN m ORDER BY m",
            "MATCH (n) WHERE n.p > 1 AND n.p < 4 WITH n AS m RETURN m ORDER BY m",
        ),
        (
            "MATCH (n) WITH DISTINCT n WHERE n.p >= 2 RETURN n ORDER BY n",
            "MATCH (n) WHERE n.p >= 2 RETURN n ORDER BY n",
        ),
        (
            "MATCH (n) WITH n WHERE n.p = 2 OR n.p = 4 LIMIT 2 RETURN n ORDER BY n",
            "MATCH (n) WHERE n.p = 2 OR n.p = 4 WITH n LIMIT 2 RETURN n ORDER BY n",
        ),
    ] {
        let found = rows(boundary);
        assert!(!found.is_empty(), "{boundary}");
        assert_eq!(found, rows(pattern), "{boundary}");
    }
    for text in [
        "MATCH (n) WITH n WHERE n.p = 2 RETURN *",
        "MATCH (n) WITH n WHERE n.p = 2 WITH * RETURN *",
    ] {
        let template = PreparedGraphSetText::prepare(text, symbols).unwrap();
        assert_eq!(template.columns(), &["n"], "{text}");
        assert!(rows(text).iter().all(|row| row.len() == 1), "{text}");
    }
    for text in [
        "MATCH (n) WITH n WHERE n.p = 2 AND __fg_boundary_0 = 2 RETURN n",
        "MATCH (n) WITH n WHERE n.p = 2 RETURN __fg_boundary_0",
    ] {
        assert!(
            PreparedGraphSetText::prepare(text, symbols).is_err(),
            "{text}"
        );
    }
    // A later stage reads `n.p` from the hidden column the plain WITH carries
    // through (fgdb-iznzx), so it needs no host element reader (fgdb-xnd7x);
    // with or without one, the rows equal the MATCH-level WHERE (openCypher
    // With7 [2] reads this way).
    let later = prepare("MATCH (n) WITH n WITH n WHERE n.p = 2 RETURN n ORDER BY n");
    let expected = rows("MATCH (n) WHERE n.p = 2 RETURN n ORDER BY n");
    assert_eq!(
        run(&later, &values, wide(), &mut || Ok(())).unwrap().value,
        expected
    );
    assert_eq!(
        run_reading(&later, &values, wide(), &mut || Ok(()), true)
            .unwrap()
            .value,
        expected
    );
}

/// fgdb-1tgko: the boundary's scope also covers the WITH's pages, a WHERE
/// after a page, and a terminal RETURN. `WITH n ... RETURN n.p` equals the
/// same query with `n.p` projected as an explicit WITH column, or read before
/// the WITH, and an unaliased `n.p` is named `p` as in a graph RETURN. Each
/// UNION arm owns its own scope. A later plain WITH that projects the element
/// under its own name keeps it in scope, items, WHERE and pages included
/// (TCK WithOrderBy4 [7], [8]). A later stage that drops or renames it, an
/// UNWIND, and the private names stay out of scope, including a later row
/// that happens to have the boundary row's width. A required later MATCH part
/// re-matches the carried vertex instead (fgdb-dhcfo).
#[test]
fn a_with_scope_reads_carried_properties_in_its_pages_and_terminal_return() {
    let values = [1, 2, 2, 3, 4].map(CanonicalScalar::Int);
    let rows = |text: &str| {
        let query = PreparedGraphSetText::prepare(text, symbols)
            .map(|template| template.bind_parameters(&GqlParameters::new()).unwrap())
            .unwrap_or_else(|error| panic!("{text}: {error:?}"));
        run(&query, &values, wide(), &mut || Ok(())).unwrap().value
    };
    for (boundary, explicit) in [
        (
            "MATCH (n) WITH n RETURN n.p AS v ORDER BY v",
            "MATCH (n) RETURN n.p AS v ORDER BY v",
        ),
        (
            "MATCH (n) WITH n WHERE n.p >= 2 RETURN n.p AS v, n ORDER BY n",
            "MATCH (n) WHERE n.p >= 2 RETURN n.p AS v, n ORDER BY n",
        ),
        (
            "MATCH (n) WITH n AS m ORDER BY m.p DESC LIMIT 2 RETURN m.p AS v",
            "MATCH (n) WITH n.p AS v ORDER BY v DESC LIMIT 2 RETURN v",
        ),
        (
            "MATCH (n) WITH n LIMIT 3 WHERE n.p = 2 RETURN n ORDER BY n",
            "MATCH (n) WITH n, n.p AS h LIMIT 3 WHERE h = 2 RETURN n ORDER BY n",
        ),
        (
            "MATCH (n) WITH n RETURN DISTINCT n.p AS v ORDER BY v",
            "MATCH (n) WITH n.p AS v RETURN DISTINCT v ORDER BY v",
        ),
        (
            "MATCH (n) WITH DISTINCT n WHERE n.p > 1 RETURN n.p * 10 AS v, n ORDER BY n",
            "MATCH (n) WHERE n.p > 1 RETURN n.p * 10 AS v, n ORDER BY n",
        ),
        (
            "MATCH (n) WITH n WITH n RETURN n.p AS v ORDER BY v",
            "MATCH (n) RETURN n.p AS v ORDER BY v",
        ),
        (
            "MATCH (n) WITH n ORDER BY n.p WITH n, 1 AS a RETURN n.p AS v, a ORDER BY v",
            "MATCH (n) RETURN n.p AS v, 1 AS a ORDER BY v",
        ),
        (
            "MATCH (n) WITH n WITH n, n.p * 10 AS q WHERE n.p >= 2 RETURN n.p AS v, q ORDER BY v",
            "MATCH (n) WHERE n.p >= 2 RETURN n.p AS v, n.p * 10 AS q ORDER BY v",
        ),
        (
            "MATCH (n) WITH n AS m WITH m ORDER BY m.p DESC LIMIT 2 WITH m \
             RETURN m.p AS v ORDER BY v",
            "MATCH (n) WITH n.p AS v ORDER BY v DESC LIMIT 2 RETURN v ORDER BY v",
        ),
        (
            "MATCH (n) WITH n WITH DISTINCT n RETURN n.p AS v ORDER BY v",
            "MATCH (n) RETURN n.p AS v ORDER BY v",
        ),
        (
            "MATCH (n) WITH n WITH * RETURN n.p AS v ORDER BY v",
            "MATCH (n) RETURN n.p AS v ORDER BY v",
        ),
    ] {
        let found = rows(boundary);
        assert!(!found.is_empty(), "{boundary}");
        assert_eq!(found, rows(explicit), "{boundary}");
    }
    // The WITH page keeps the two largest; only a terminal ORDER BY orders
    // the result.
    assert_eq!(
        ints(
            &rows(
                "MATCH (n) WITH n AS m ORDER BY m.p DESC LIMIT 2 RETURN m.p AS v ORDER BY v DESC"
            ),
            0
        ),
        vec![Some(4), Some(3)]
    );
    let mut arms = ints(
        &rows("MATCH (n) WITH n WHERE n.p = 2 RETURN n.p AS v UNION ALL MATCH (m) RETURN m.p AS v"),
        0,
    );
    arms.sort();
    assert_eq!(
        arms,
        [1, 2, 2, 2, 2, 3, 4].map(Some).to_vec(),
        "a UNION arm after a boundary RETURN"
    );
    for (text, columns) in [
        ("MATCH (n) WITH n RETURN n.p", &["n.p"][..]),
        ("MATCH (n) WITH n AS m RETURN m, m.p AS q", &["m", "q"][..]),
        ("MATCH (n) WITH n ORDER BY n.p RETURN *", &["n"][..]),
    ] {
        let template = PreparedGraphSetText::prepare(text, symbols).unwrap();
        assert_eq!(template.columns(), columns, "{text}");
        assert!(
            rows(text).iter().all(|row| row.len() == columns.len()),
            "{text}"
        );
    }
    for text in [
        "MATCH (n) WITH n RETURN n.p AS v, __fg_boundary_0",
        "MATCH (n) WITH n ORDER BY __fg_boundary_0 RETURN n.p",
        "MATCH (n) WITH n RETURN n.p, n.p",
        "MATCH (n) WITH n UNWIND [1] AS x RETURN n.p",
        // A later WITH that renames or drops the element ends its scope.
        "MATCH (n) WITH n WITH n AS m RETURN n.p",
        "MATCH (n) WITH n WITH n.p AS q RETURN n.p",
        // Two columns again after the second WITH, as the boundary row had
        // (n plus the hidden n.p), but without n: the hidden read must not
        // resolve to `a` or `b`.
        "MATCH (n) WITH n ORDER BY n.p WITH 1 AS a, 2 AS b RETURN n.p",
    ] {
        assert!(
            PreparedGraphSetText::prepare(text, symbols).is_err(),
            "{text}"
        );
    }
    // fgdb-dhcfo: a RETURN-only carried read uses the host's pinned element
    // reader and does not add a graph binding merely to read a property.
    // The pattern-only adapter cannot perform that read: its documented
    // typed refusal must not become a guessed NULL, even beneath LIMIT 0.
    for suffix in ["", " LIMIT 0"] {
        let carried = prepare(&format!(
            "MATCH (n) WITH n AS kept MATCH (m) RETURN kept.p AS v ORDER BY v{suffix}"
        ));
        assert!(matches!(
            run(&carried, &values, wide(), &mut || Ok(())),
            Err(GqlQueryError::Source(GraphSetExecutionError::Projection {
                error: fgdb_gql::GraphIntegerError {
                    kind: fgdb_gql::GraphIntegerErrorKind::IncompatibleOperands,
                    ..
                },
                ..
            }))
        ));
    }
    assert_eq!(
        run_reading(
            &prepare("MATCH (n) WITH n AS kept MATCH (m) RETURN kept.p AS v ORDER BY v"),
            &values,
            wide(),
            &mut || Ok(()),
            true
        )
        .unwrap()
        .value,
        rows("MATCH (n), (m) RETURN n.p AS v ORDER BY v")
    );
}

/// openCypher (TCK WithWhere7 [1], WithOrderBy2 [21], TriadicSelection1):
/// a WITH that neither groups nor deduplicates lets its own WHERE and pages
/// read the MATCH bindings it drops, bare or by property. Each such query
/// equals the one that projects the binding explicitly. The dropped binding
/// stays out of the RETURN, `*`, a later stage, and a DISTINCT WITH, where a
/// hidden column would change which rows are duplicates.
#[test]
fn a_plain_with_reads_the_bindings_it_drops_in_its_own_where_and_pages() {
    let values = [1, 2, 2, 3, 4].map(CanonicalScalar::Int);
    let rows = |text: &str| {
        let query = PreparedGraphSetText::prepare(text, symbols)
            .map(|template| template.bind_parameters(&GqlParameters::new()).unwrap())
            .map_err(|error| format!("{text}: {error:?}"))
            .unwrap();
        run(&query, &values, wide(), &mut || Ok(())).unwrap().value
    };
    for (dropped, explicit, len) in [
        (
            "MATCH (n) WITH n.p AS v WHERE n.p >= 2 RETURN v ORDER BY v",
            "MATCH (n) WITH n.p AS v WHERE v >= 2 RETURN v ORDER BY v",
            4,
        ),
        (
            "MATCH (n) WITH n.p AS v ORDER BY n.p DESC LIMIT 2 RETURN v ORDER BY v",
            "MATCH (n) WITH n.p AS v ORDER BY v DESC LIMIT 2 RETURN v ORDER BY v",
            2,
        ),
        (
            "MATCH (n) WITH n.p AS v ORDER BY n.p + 1 DESC LIMIT 2 RETURN v ORDER BY v",
            "MATCH (n) WITH n.p AS v ORDER BY v DESC LIMIT 2 RETURN v ORDER BY v",
            2,
        ),
        // This harness executes no identified edges, so the optional binding
        // the WHERE reads is the far vertex (NULL on every row here).
        (
            "MATCH (n) OPTIONAL MATCH (n)-[:R]->(m) WITH n WHERE m IS NULL \
             RETURN n ORDER BY n",
            "MATCH (n) OPTIONAL MATCH (n)-[:R]->(m) WITH n, m WHERE m IS NULL \
             RETURN n ORDER BY n",
            5,
        ),
        (
            "MATCH (n) OPTIONAL MATCH (n)-[:R]->(m) WITH n WHERE m IS NOT NULL OR n.p = 3 \
             RETURN n ORDER BY n",
            "MATCH (n) OPTIONAL MATCH (n)-[:R]->(m) WITH n, m WHERE m IS NOT NULL OR n.p = 3 \
             RETURN n ORDER BY n",
            1,
        ),
        // A reduce accumulator named like a dropped binding is the local,
        // not the binding: 0 + 1 + 2 = 3 holds on every row.
        (
            "MATCH (n) OPTIONAL MATCH (n)-[:R]->(m) WITH n.p AS v \
             WHERE reduce(m = 0, x IN [1, 2] | m + x) = 3 RETURN v ORDER BY v",
            "MATCH (n) WITH n.p AS v RETURN v ORDER BY v",
            5,
        ),
    ] {
        let found = rows(dropped);
        assert_eq!(found.len(), len, "{dropped}");
        assert_eq!(found, rows(explicit), "{dropped}");
    }
    let template =
        PreparedGraphSetText::prepare("MATCH (n) WITH n.p AS v WHERE n.p > 1 RETURN *", symbols)
            .unwrap();
    assert_eq!(template.columns(), &["v"]);
    for text in [
        "MATCH (n) WITH n.p AS v WHERE n.p > 1 RETURN n",
        "MATCH (n) WITH n.p AS v WHERE n.p > 1 RETURN n.p",
        "MATCH (n) WITH n.p AS v ORDER BY n.p RETURN v, n.p",
        "MATCH (n) WITH n.p AS v WITH v WHERE n.p > 1 RETURN v",
        "MATCH (n) OPTIONAL MATCH (n)-[r:R]->(m) WITH DISTINCT n WHERE r IS NULL RETURN n",
        "MATCH (n) WITH DISTINCT n.p AS v ORDER BY n LIMIT 1 RETURN v",
    ] {
        assert!(
            PreparedGraphSetText::prepare(text, symbols).is_err(),
            "{text}"
        );
    }
}

/// openCypher Temporal5: a stored temporal property carried through WITH
/// keeps its fields. On a scalar column only temporal field names can answer
/// with a value: NULL on NULL, the field on a temporal value, and an
/// execution error on any other scalar. Any other key could only be NULL or
/// an error and stays refused at preparation.
#[test]
fn a_scalar_column_reads_the_fields_of_the_temporal_value_it_holds() {
    let date = CanonicalScalar::Date(fgdb_types::CanonicalDate::from_civil(1984, 10, 11).unwrap());
    let values = [date, CanonicalScalar::Null];
    let found = run(
        &prepare("MATCH (n) WITH n.p AS d RETURN d.year AS y, d.dayOfQuarter AS q"),
        &values,
        wide(),
        &mut || Ok(()),
    )
    .unwrap()
    .value;
    let mut cells: Vec<_> = ints(&found, 0).into_iter().zip(ints(&found, 1)).collect();
    cells.sort();
    assert_eq!(cells, vec![(None, None), (Some(1984), Some(11))]);
    // Inside a scalar expression the field is an operand like any other.
    let found = run(
        &prepare("MATCH (n) WITH n.p AS d RETURN d.year + 1 AS y"),
        &values,
        wide(),
        &mut || Ok(()),
    )
    .unwrap()
    .value;
    let mut years = ints(&found, 0);
    years.sort();
    assert_eq!(years, vec![None, Some(1985)]);
    assert!(
        run(
            &prepare("MATCH (n) WITH n.p AS d RETURN d.year AS y"),
            &[CanonicalScalar::Int(3)],
            wide(),
            &mut || Ok(()),
        )
        .is_err()
    );
    for text in [
        "MATCH (n) WITH n.p AS d RETURN d.missing",
        "MATCH (n) WITH n.p AS d RETURN d.p AS v",
    ] {
        assert!(
            PreparedGraphSetText::prepare(text, symbols).is_err(),
            "{text}"
        );
    }
}

#[test]
fn discarded_names_sibling_aliases_and_unsupported_forms_refuse_before_catalog() {
    for text in [
        "MATCH (n) WITH n.p AS score RETURN n",
        "MATCH (n) WITH n.p AS score RETURN score.p",
        "MATCH (n) WITH n.p AS score WITH score AS first,first+1 AS second RETURN second",
        "MATCH (n) WITH n RETURN n+1 AS bad",
        "MATCH (n) WITH n.p AS p WITH p AS x,p AS x RETURN x",
        // Vertex-valued WITH imports now continue into MATCH; scalar imports
        // still cannot become vertices. multipart_reads tests the positive form.
        "MATCH (n) WITH n.p AS n MATCH (n)-[:R]->(m) RETURN m",
        "MATCH (n) WITH n WHERE n > n RETURN n",
        "MATCH (n) WITH n.p AS p RETURN missing LIMIT 0",
        "MATCH (n) WITH n.p AS p ORDER BY missing RETURN p",
        "MATCH (n) WITH COUNT(DISTINCT *) AS count RETURN count",
    ] {
        let calls = Cell::new(0);
        assert!(
            PreparedGraphSetText::prepare(text, |kind, name| {
                calls.set(calls.get() + 1);
                symbols(kind, name)
            })
            .is_err(),
            "{text}"
        );
        assert_eq!(calls.get(), 0, "{text}");
    }
    // A repeated ORDER BY key is not a refusal: the first key's direction
    // stands (openCypher WithOrderBy3 [7]; the WITH page laws in fgdb's
    // embedded_query_entrypoint pin the rows).
    assert!(
        PreparedGraphSetText::prepare("MATCH (n) WITH n.p AS p ORDER BY p,p RETURN p", symbols)
            .is_ok()
    );
    let valid = format!("MATCH (n){} RETURN n", " WITH n".repeat(62));
    assert!(PreparedGraphSetText::prepare(&valid, symbols).is_ok());
    let invalid = format!("MATCH (n){} RETURN n", " WITH n".repeat(63));
    assert!(matches!(
        PreparedGraphSetText::prepare(&invalid, symbols)
            .unwrap_err()
            .kind,
        GraphSetTextErrorKind::SetBuild(_)
    ));
}

#[test]
fn formerly_refused_with_count_executes_over_real_nonempty_and_empty_graph_inputs() {
    // fgdb-ezgeq deliberately changes the grammar, not the refusal gate.
    // COUNT(DISTINCT *) above is still rejected before any catalog call.
    let query = prepare("MATCH (n) WITH COUNT(*) AS count RETURN count");
    for size in [0, 1, 5] {
        let values = vec![CanonicalScalar::Null; size];
        let result = run(&query, &values, wide(), &mut || Ok(())).unwrap();
        assert_eq!(ints(&result.value, 0), vec![Some(size as i64)]);
        assert_eq!(result.rows.snapshot_records, size as u64);
    }
}

#[test]
fn a_single_reads_order_by_ranks_by_input_columns_it_does_not_return() {
    // fgdb-ujuga: ORDER BY reads any variable the RETURN can see. A key that
    // is no output column ranks by the read's own scope and is not returned.
    // Only a compound set's tail is limited to its output columns.
    let values = [3, 1, 2].map(CanonicalScalar::Int);
    for (text, expected) in [
        (
            "MATCH (n) WITH n.p AS k, n.p * 10 AS v RETURN v ORDER BY k",
            vec![10, 20, 30],
        ),
        (
            "MATCH (n) WITH n.p AS k, n.p * 10 AS v RETURN v ORDER BY k DESC LIMIT $top",
            vec![30, 20],
        ),
        (
            "MATCH (n) WITH n.p AS k RETURN k * 10 AS v ORDER BY k DESC SKIP 1",
            vec![20, 10],
        ),
        (
            "MATCH (n) WITH n.p AS k, 0 - n.p AS v RETURN v ORDER BY v, k",
            vec![-3, -2, -1],
        ),
        (
            "UNWIND [3, 1, 2] AS k RETURN k * 10 AS v ORDER BY k DESC",
            vec![30, 20, 10],
        ),
    ] {
        let template = PreparedGraphSetText::prepare(text, symbols).unwrap();
        assert_eq!(template.columns(), ["v"], "{text}");
        let arguments = if text.contains("$top") {
            GqlParameters::new().with_uint64("top", 2).unwrap()
        } else {
            GqlParameters::new()
        };
        let query = template.bind_parameters(&arguments).unwrap();
        let result = run(&query, &values, wide(), &mut || Ok(())).unwrap();
        let expected: Vec<_> = expected.into_iter().map(Some).collect();
        assert_eq!(ints(&result.value, 0), expected, "{text}");
    }
    for text in [
        // openCypher: under DISTINCT a key must be projected.
        "MATCH (n) WITH n.p AS k, n.p * 10 AS v RETURN DISTINCT v ORDER BY k",
        // A compound set's ORDER BY ranks the set's output columns only.
        "MATCH (n) WITH n.p AS k RETURN k AS v UNION MATCH (n) WITH n.p AS k RETURN k AS v ORDER BY k",
        "MATCH (n) WITH n.p AS k RETURN k AS v ORDER BY missing",
    ] {
        let calls = Cell::new(0);
        assert!(
            PreparedGraphSetText::prepare(text, |kind, name| {
                calls.set(calls.get() + 1);
                symbols(kind, name)
            })
            .is_err(),
            "{text}"
        );
        assert_eq!(calls.get(), 0, "{text}");
    }
}

/// fgdb-7xqxb: a set that leads with its one vertex scan's identity, then
/// columns and constants, pulls row by row. Projecting the pattern's own
/// rows in its visitation order reproduces the eager relation exactly.
/// Every other shape has no pull.
#[test]
fn a_vertex_rooted_projection_pulls_in_its_canonical_order() {
    let values = [3, 1, 2].map(CanonicalScalar::Int);
    let set = PreparedGraphSetText::prepare("MATCH (n) RETURN n AS id, $v AS v, n.p AS p", symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new().with_int64("v", 9).unwrap())
        .unwrap();
    let pull = set.row_pull().expect("one vertex scan, identity first");
    let eager = run(&set, &values, wide(), &mut || Ok(())).unwrap().value;
    let source = run(
        &PreparedGraphSet::from(pull.pattern().clone()),
        &values,
        wide(),
        &mut || Ok(()),
    )
    .unwrap()
    .value;
    let pulled: Vec<_> = source
        .iter()
        .enumerate()
        .map(|(at, row)| {
            pull.project(row, at as u64, &mut |_| Ok::<(), ()>(()), |_, _| ())
                .unwrap()
        })
        .collect();
    assert_eq!(eager.len(), values.len());
    assert_eq!(pulled, eager);
    for text in [
        "MATCH (n) RETURN 9 AS v, n AS id",
        "MATCH (n) RETURN DISTINCT n AS id, 9 AS v",
        "MATCH (n) RETURN n AS id, 9 AS v LIMIT 2",
        "MATCH (n) RETURN n AS id, 9 AS v ORDER BY v",
        "MATCH (n)-[:R]->(m) RETURN n AS id, 9 AS v",
        "MATCH (n), (m) RETURN n AS id, 9 AS v",
    ] {
        assert!(prepare(text).row_pull().is_none(), "{text}");
    }
}

#[test]
fn parameter_contracts_and_original_utf8_offsets_span_all_stages() {
    let text = "\u{2003}MATCH (n) WITH n.p+$add AS score LIMIT $top WHERE score>$min RETURN score*$scale AS answer";
    let template = PreparedGraphSetText::prepare(text, symbols).unwrap();
    assert_eq!(template.parameter_schema().len(), 4);
    let args = GqlParameters::new()
        .with_int64("add", 1)
        .unwrap()
        .with_uint64("top", 5)
        .unwrap()
        .with_int64("min", 0)
        .unwrap();
    let missing = template.bind_parameters(&args).unwrap_err();
    assert_eq!(missing.offset, text.find("$scale").unwrap());
    let bound = template
        .bind_parameters(&args.clone().with_int64("scale", 2).unwrap())
        .unwrap();
    assert_ne!(
        bound.canonical_bytes(),
        template
            .bind_parameters(&args.with_int64("scale", 3).unwrap())
            .unwrap()
            .canonical_bytes()
    );
    let calls = Cell::new(0);
    let conflict = "MATCH (n) WITH n.p+$x AS score LIMIT $x RETURN score";
    assert!(
        PreparedGraphSetText::prepare(conflict, |kind, name| {
            calls.set(calls.get() + 1);
            symbols(kind, name)
        })
        .is_err()
    );
    assert_eq!(calls.get(), 0);
    for at in (0..=text.len()).filter(|at| text.is_char_boundary(*at)) {
        let _ = PreparedGraphSetText::prepare(&text[..at], symbols);
    }
    assert!(!format!("{template:?}").contains("score"));
}

#[test]
fn pipeline_leaves_compose_with_union_and_preserve_literal_token_boundaries() {
    let text = "(MATCH (n) WITH n.p AS p WHERE p>1 RETURN p LIMIT 1) UNION ALL (MATCH (n) WITH n.p AS p WHERE p<2 RETURN p) ORDER BY p DESC";
    let rows = run(
        &prepare(text),
        &[1, 2, 3].map(CanonicalScalar::Int),
        wide(),
        &mut || Ok(()),
    )
    .unwrap();
    assert_eq!(ints(&rows.value, 0), vec![Some(2), Some(1)]);
    assert_eq!(rows.rows.snapshot_records, 6);
    let literal = prepare("MATCH (n) WITH 'WITH $missing RETURN' AS text RETURN text");
    assert_eq!(
        run(
            &literal,
            &[CanonicalScalar::Null, CanonicalScalar::Null],
            wide(),
            &mut || Ok(())
        )
        .unwrap()
        .value
        .len(),
        2
    );
}

#[test]
fn full_pipeline_shares_exact_resource_limits_and_all_interruption_points() {
    let query = prepare("MATCH (n) WITH n.p+1 AS x WHERE x>1 WITH x*2 AS y RETURN y");
    let values = [1, 2, 3].map(CanonicalScalar::Int);
    let calls = Cell::new(0);
    let measured = run(&query, &values, wide(), &mut || {
        calls.set(calls.get() + 1);
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
            &values,
            GqlQueryPolicy::new(caps[0], caps[1], caps[2], caps[3]),
            &mut || Ok(())
        )
        .unwrap(),
        measured
    );
    for dimension in 0..4 {
        let mut limit = caps;
        limit[dimension] -= 1;
        assert!(
            run(
                &query,
                &values,
                GqlQueryPolicy::new(limit[0], limit[1], limit[2], limit[3]),
                &mut || Ok(())
            )
            .is_err()
        );
    }
    for stop in 1..=calls.get() {
        let mut at = 0;
        assert!(
            matches!(run(&query, &values, wide(), &mut || { at += 1; if at==stop {Err(stop)} else {Ok(())} }),
            Err(GqlQueryError::Interrupted(value)) if value==stop)
        );
        assert_eq!(at, stop);
    }
}

/// fgdb-2t30i. After a WITH, `ORDER BY v.p` names the RETURN item that is
/// exactly `v.p`, aliased or not (an unaliased one is the column "v.p"). With
/// an alias it is the same statement as ordering by that alias: identical
/// template bytes and rows, including DESC and NULLS placement. Before, the key stopped at `v` and the statement refused
/// with Expected("end of compound statement").
#[test]
fn order_by_a_returned_property_after_with_is_its_output_column() {
    let values = [
        CanonicalScalar::Int(3),
        CanonicalScalar::Null,
        CanonicalScalar::Int(1),
        CanonicalScalar::Int(2),
        CanonicalScalar::Null,
    ];
    let template = |text: &str| {
        PreparedGraphSetText::prepare(text, symbols)
            .map_err(|error| format!("{text}: {error:?}"))
            .unwrap()
    };
    for (qualified, named, column, expected) in [
        (
            "MATCH (n) WITH n RETURN n.p AS p ORDER BY n.p",
            "MATCH (n) WITH n RETURN n.p AS p ORDER BY p",
            0,
            vec![Some(1), Some(2), Some(3), None, None],
        ),
        (
            "MATCH (n) WITH n RETURN n.p AS p ORDER BY n.p DESC NULLS FIRST",
            "MATCH (n) WITH n RETURN n.p AS p ORDER BY p DESC NULLS FIRST",
            0,
            vec![None, None, Some(3), Some(2), Some(1)],
        ),
        (
            "MATCH (n) WITH n AS m RETURN m, m.p AS v ORDER BY m.p DESC",
            "MATCH (n) WITH n AS m RETURN m, m.p AS v ORDER BY v DESC",
            1,
            vec![Some(3), Some(2), Some(1), None, None],
        ),
    ] {
        let (a, b) = (template(qualified), template(named));
        assert_eq!(
            a.canonical_template_bytes(),
            b.canonical_template_bytes(),
            "{qualified}"
        );
        let bind =
            |text: &PreparedGraphSetText| text.bind_parameters(&GqlParameters::new()).unwrap();
        let rows = run(&bind(&a), &values, wide(), &mut || Ok(()))
            .unwrap()
            .value;
        assert_eq!(ints(&rows, column), expected, "{qualified}");
        let named_rows = run(&bind(&b), &values, wide(), &mut || Ok(()))
            .unwrap()
            .value;
        assert_eq!(rows, named_rows, "{qualified}");
    }
}

/// Planted negatives for fgdb-2t30i. They refuse before any catalog call: a
/// property the RETURN did not project, another variable's property, a
/// computed item, and a key after a set operator, where ORDER BY names only
/// output columns.
#[test]
fn order_by_a_property_the_return_did_not_project_still_refuses() {
    // fgdb-ujuga: ORDER BY reads any property of an element the RETURN can
    // see, projected or not. `n.q` passes every structural check and fails
    // only at the catalog, which knows no `q`.
    let resolved =
        PreparedGraphSetText::prepare("MATCH (n) WITH n RETURN n.p ORDER BY n.q", symbols);
    assert!(
        matches!(
            &resolved,
            Err(error) if matches!(
                error.kind,
                GraphSetTextErrorKind::Pattern(fgdb_gql::GraphPatternTextErrorKind::UnknownSymbol(_))
            )
        ),
        "{:?}",
        resolved.as_ref().err()
    );
    let values = [3, 1, 2].map(CanonicalScalar::Int);
    let ordered = run(
        &prepare("MATCH (n) WITH n RETURN n.p + 1 AS p ORDER BY n.p DESC"),
        &values,
        wide(),
        &mut || Ok(()),
    )
    .unwrap();
    assert_eq!(ints(&ordered.value, 0), vec![Some(4), Some(3), Some(2)]);
    // A binding the WITH renamed away, and a compound set's tail, which
    // ranks output columns only, still refuse before any catalog call.
    for text in [
        "MATCH (n) WITH n AS m RETURN m.p ORDER BY n.p",
        "MATCH (n) WITH n RETURN n.p UNION MATCH (n) WITH n RETURN n.p ORDER BY n.p",
    ] {
        let calls = Cell::new(0);
        let prepared = PreparedGraphSetText::prepare(text, |kind, name| {
            calls.set(calls.get() + 1);
            symbols(kind, name)
        });
        assert!(prepared.is_err(), "{text}");
        assert_eq!(calls.get(), 0, "{text}: refused after a catalog call");
    }
}

/// openCypher coalesce over a list (TCK Quantifier9-12 read
/// `coalesce(fixedList, list)`): the first non-NULL argument, in order, and
/// no later argument runs; all NULL is NULL. A quantifier reads the result
/// as a list. An all-scalar coalesce keeps the scalar COALESCE.
#[test]
fn coalesce_over_lists_is_the_first_non_null_argument() {
    use fgdb_gql::algebra::GraphValue;
    let values = [CanonicalScalar::Int(2)];
    let first = |text: &str| {
        let rows = run(&prepare(text), &values, wide(), &mut || Ok(()))
            .unwrap_or_else(|error| panic!("{text}: {error:?}"))
            .value;
        rows[0].get(0).unwrap().clone()
    };
    let list = |items: &[i64]| {
        GraphValue::List(
            items
                .iter()
                .map(|&n| GraphValue::Scalar(CanonicalScalar::Int(n)))
                .collect(),
        )
    };
    for (text, expected) in [
        (
            "MATCH (n) WITH [1, 2] AS a, null AS b RETURN coalesce(b, a) AS r",
            list(&[1, 2]),
        ),
        (
            "MATCH (n) WITH [1, 2] AS a RETURN coalesce(null, [3], a) AS r",
            list(&[3]),
        ),
        // The second argument never runs: n.p / (n.p - 2) divides by zero.
        (
            "MATCH (n) WITH n, [1] AS a RETURN coalesce(a, [n.p / (n.p - 2)]) AS r",
            list(&[1]),
        ),
        (
            "MATCH (n) WITH n, null AS b RETURN coalesce(b, [n.p]) AS r",
            list(&[2]),
        ),
        (
            "MATCH (n) WITH null AS b RETURN coalesce(b, [1][5]) AS r",
            GraphValue::Scalar(CanonicalScalar::Null),
        ),
        (
            "MATCH (n) WITH [1, 2] AS a, null AS b WITH coalesce(b, a) AS list \
             RETURN single(x IN list WHERE x = 2) AS r",
            GraphValue::Scalar(CanonicalScalar::Bool(true)),
        ),
        (
            "MATCH (n) RETURN coalesce(null, n.p) AS r",
            GraphValue::Scalar(CanonicalScalar::Int(2)),
        ),
    ] {
        assert_eq!(first(text), expected, "{text}");
    }
}

/// A parenthesized operand IN a list value (TCK Precedence1: `(a = b) IN c`
/// with `c` a list column) is one membership in the value language, as a
/// name IN a list column is: three-valued, NULL members included.
#[test]
fn a_parenthesized_operand_in_a_list_column_is_a_membership() {
    use fgdb_gql::algebra::GraphValue;
    let query = prepare(
        "UNWIND [true] AS a UNWIND [true] AS b UNWIND [[true], [false], [], [null]] AS c \
         RETURN (a = b) IN c AS r",
    );
    let mut rows = run(&query, &[], wide(), &mut || Ok(())).unwrap().value;
    // RETURN without ORDER BY is canonical order, so compare as a bag.
    let count = |rows: &[GraphValueRow], value: Option<bool>| {
        let expected =
            GraphValue::Scalar(value.map_or(CanonicalScalar::Null, CanonicalScalar::Bool));
        rows.iter()
            .filter(|row| row.get(0) == Some(&expected))
            .count()
    };
    assert_eq!(
        [Some(true), Some(false), None].map(|value| count(&rows, value)),
        [1, 2, 1]
    );
    // The same answer as reading the comparison first through a WITH.
    let named = prepare(
        "UNWIND [true] AS a UNWIND [true] AS b UNWIND [[true], [false], [], [null]] AS c \
         WITH a = b AS x, c RETURN x IN c AS r",
    );
    let mut named_rows = run(&named, &[], wide(), &mut || Ok(())).unwrap().value;
    rows.sort();
    named_rows.sort();
    assert_eq!(rows, named_rows);
}

/// A scalar column can carry NULL, and a list operation over NULL is NULL
/// (TCK Map2 [3], `expr[idx]` over a NULL `expr`). Any nonnull scalar in a
/// list position fails, when evaluated or, for a literal, at preparation, and
/// never answers rows.
#[test]
fn a_list_operation_over_a_null_scalar_column_is_null() {
    use fgdb_gql::algebra::GraphValue;
    let values = [CanonicalScalar::Int(2)];
    let first = |text: &str| {
        let rows = run(&prepare(text), &values, wide(), &mut || Ok(()))
            .unwrap_or_else(|error| panic!("{text}: {error:?}"))
            .value;
        assert_eq!(rows.len(), 1, "{text}");
        rows[0].get(0).unwrap().clone()
    };
    for text in [
        "MATCH (n) WITH null AS e, 'x' AS k RETURN e[k] AS v",
        "MATCH (n) WITH null AS e RETURN e[0] AS v",
        "MATCH (n) WITH null AS l RETURN [x IN l | x] AS v",
        "MATCH (n) WITH null AS l RETURN any(x IN l WHERE true) AS v",
        "MATCH (n) WITH null AS l RETURN l[1..] AS v",
        "MATCH (n) WITH null AS l RETURN reduce(s = 0, x IN l | s + x) AS v",
    ] {
        assert_eq!(
            first(text),
            GraphValue::Scalar(CanonicalScalar::Null),
            "{text}"
        );
    }
    let fails = |text: &str| {
        PreparedGraphSetText::prepare(text, symbols)
            .ok()
            .and_then(|query| query.bind_parameters(&GqlParameters::new()).ok())
            .is_none_or(|query| run(&query, &values, wide(), &mut || Ok(())).is_err())
    };
    for text in [
        "MATCH (n) RETURN n.p[0] AS v",
        "MATCH (n) WITH 1 AS l RETURN l[0] AS v",
        "MATCH (n) WITH n.p AS l RETURN [x IN l | x] AS v",
        "MATCH (n) RETURN [x IN 3 | x] AS v",
        "MATCH (n) RETURN 1[0] AS v",
    ] {
        assert!(fails(text), "{text}");
    }
}

/// A CASE whose result reads a dynamically typed column (`input.list` over
/// an UNWIND of maps) is read in the value language, typed per row: its
/// result may be a list, where the scalar program would fail when it ran
/// (TCK Quantifier9-12). A CASE with only scalar results keeps its answer.
#[test]
fn a_case_returning_a_dynamic_entry_is_typed_per_row() {
    use fgdb_gql::algebra::GraphValue;
    let int = |n: i64| GraphValue::Scalar(CanonicalScalar::Int(n));
    let list = |items: &[i64]| GraphValue::List(items.iter().map(|&n| int(n)).collect());
    let null = GraphValue::Scalar(CanonicalScalar::Null);
    let bag = |text: &str| {
        let mut values: Vec<_> = run(&prepare(text), &[], wide(), &mut || Ok(()))
            .unwrap_or_else(|error| panic!("{text}: {error:?}"))
            .value
            .iter()
            .map(|row| row.values().to_vec())
            .collect();
        values.sort();
        values
    };
    let mut expected = vec![vec![list(&[1])], vec![null.clone()]];
    expected.sort();
    assert_eq!(
        bag(
            "UNWIND [{list: [1], f: true}, {list: [2], f: false}] AS input \
             RETURN CASE WHEN input.f THEN input.list ELSE null END AS l"
        ),
        expected
    );
    let mut expected = vec![vec![int(1)], vec![list(&[2, 3])]];
    expected.sort();
    assert_eq!(
        bag("UNWIND [{v: 1}, {v: [2, 3]}] AS input RETURN CASE WHEN true THEN input.v END AS v"),
        expected
    );
    // The TCK's input stage: both CASEs, through a WITH.
    let mut expected = vec![vec![list(&[2]), list(&[1])], vec![null, list(&[1, 2, 3])]];
    expected.sort();
    assert_eq!(
        bag(
            "UNWIND [{list: [2], fixed: true}, {list: [1, 2, 3], fixed: false}] AS input \
             WITH CASE WHEN input.fixed THEN input.list ELSE null END AS fixedList, \
                  CASE WHEN NOT input.fixed THEN input.list ELSE [1] END AS inputList \
             RETURN fixedList, inputList"
        ),
        expected
    );
    // Scalar results keep the scalar CASE's answers.
    let mut expected = vec![vec![int(1)], vec![int(2)]];
    expected.sort();
    assert_eq!(
        bag("UNWIND [{f: true}, {f: false}] AS input \
             RETURN CASE WHEN input.f THEN 1 ELSE 2 END AS v"),
        expected
    );
}

/// A list quantifier is a Boolean row predicate in a WITH's WHERE, alone,
/// negated or under AND and OR (TCK Quantifier11 [3]). It is checked for
/// the Boolean domain like any scalar predicate: a composite that is not a
/// Boolean fails, never filters silently.
#[test]
fn a_with_where_filters_by_list_quantifiers() {
    let kept = |text: &str| {
        let rows = run(&prepare(text), &[], wide(), &mut || Ok(()))
            .unwrap_or_else(|error| panic!("{text}: {error:?}"))
            .value;
        let mut values = ints(&rows, 0);
        values.sort();
        values
    };
    let lists = "UNWIND [[2], [1, 2], [3]] AS list WITH list, size(list) AS n";
    for (filter, expected) in [
        ("single(x IN list WHERE x = 2)", vec![Some(1), Some(2)]),
        ("NOT any(x IN list WHERE x = 2)", vec![Some(1)]),
        (
            "single(x IN list WHERE x = 2) OR all(x IN list WHERE x = 3)",
            vec![Some(1), Some(1), Some(2)],
        ),
        ("any(x IN list WHERE x = 2) AND n > 1", vec![Some(2)]),
        ("none(x IN list WHERE x > 0)", vec![]),
    ] {
        let text = format!("{lists} WHERE {filter} RETURN n");
        assert_eq!(kept(&text), expected, "{text}");
    }
    let text = format!("{lists} WHERE [x IN list | x] RETURN n");
    let failed = PreparedGraphSetText::prepare(&text, symbols)
        .ok()
        .and_then(|query| query.bind_parameters(&GqlParameters::new()).ok())
        .is_none_or(|query| run(&query, &[], wide(), &mut || Ok(())).is_err());
    assert!(failed, "{text}");
}

/// fgdb-4avot: an aggregating WITH item may combine aggregate results with
/// arithmetic and grouping keys (TCK With6 [6], [7]): `COUNT(*) + 1`, or
/// `x * 10 + COUNT(*)` beside the key `x`, including a carried element's
/// property read at the graph boundary (`n.p + COUNT(*)`). The rows equal
/// the two-stage form, which groups first and computes over the grouped
/// columns in a second WITH. A non-aggregate operand that is not a grouping
/// key refuses, as in openCypher.
#[test]
fn an_aggregating_with_combines_aggregate_results_like_a_second_stage() {
    let values = [5, 5, 9, 7].map(CanonicalScalar::Int);
    let rows = |text: &str| {
        let mut rows = run(&prepare(text), &values, wide(), &mut || Ok(()))
            .unwrap_or_else(|error| panic!("{text}: {error:?}"))
            .value;
        rows.sort();
        rows
    };
    for (combined, staged) in [
        (
            "MATCH (n) WITH n.p AS x WITH x, COUNT(*) + 1 AS c RETURN x, c",
            "MATCH (n) WITH n.p AS x WITH x, COUNT(*) AS c0 WITH x, c0 + 1 AS c RETURN x, c",
        ),
        (
            "MATCH (n) WITH n.p AS x WITH x, x * 10 + COUNT(*) AS c RETURN x, c",
            "MATCH (n) WITH n.p AS x WITH x, COUNT(*) AS c0 WITH x, x * 10 + c0 AS c RETURN x, c",
        ),
        (
            "MATCH (n) WITH n.p AS x, n.p + COUNT(*) AS c RETURN x, c",
            "MATCH (n) WITH n.p AS x, COUNT(*) AS c0 WITH x, x + c0 AS c RETURN x, c",
        ),
    ] {
        let found = rows(combined);
        assert_eq!(found, rows(staged), "{combined}");
        assert_eq!(found.len(), 3, "{combined}");
    }
    assert!(
        PreparedGraphSetText::prepare(
            "MATCH (n) WITH n.p AS x, n.p AS y WITH x, y + COUNT(*) AS c RETURN x, c",
            symbols
        )
        .is_err()
    );
}
