//! Boolean text lowers to the shared three-valued GLA selection, not a matcher.
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::{
    GlaDirection, GlaOperator, GraphBooleanExpression, GraphBooleanOp as Op,
    GraphBooleanOperand as Arg, GraphColumn, GraphPatternBuilder, GraphValueRow,
    IntegerComparison as Cmp, PreparedGraphPattern, VertexPredicate,
};
use fgdb_gql::{
    GqlParameterType, GqlParameters, GqlQueryError, GqlQueryPolicy, GraphPatternTextErrorKind,
    GraphSymbol, GraphSymbolKind, PreparedGraphAggregateText, PreparedGraphText,
};
use fgdb_types::{CanonicalPropertyValue, CanonicalScalar, CanonicalScalarKind, VId};
use std::cell::Cell;

const P: PropertyKeyId = PropertyKeyId(1);
const FLAG: PropertyKeyId = PropertyKeyId(2);
const R: RelationId = RelationId(1);
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Property, "n") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "flag") => Some(GraphSymbol::Property(FLAG)),
        (GraphSymbolKind::Label, "L") => Some(GraphSymbol::Label(LabelId(1))),
        _ => None,
    }
}
fn query(text: &str) -> PreparedGraphPattern<GraphValueRow> {
    PreparedGraphText::prepare(text, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}
fn wide() -> GqlQueryPolicy {
    GqlQueryPolicy::new(1000, 1000, 1_000_000, 1_000_000)
}
fn less(a: Option<&CanonicalScalar>, b: Option<&CanonicalScalar>) -> Option<bool> {
    match (a, b) {
        (Some(CanonicalScalar::Int(a)), Some(CanonicalScalar::Int(b))) => Some(a < b),
        (Some(CanonicalScalar::Bool(a)), Some(CanonicalScalar::Bool(b))) => Some(a < b),
        _ => None,
    }
}
fn and(a: Option<bool>, b: Option<bool>) -> Option<bool> {
    match (a, b) {
        (Some(false), _) | (_, Some(false)) => Some(false),
        (Some(true), Some(true)) => Some(true),
        _ => None,
    }
}
fn or(a: Option<bool>, b: Option<bool>) -> Option<bool> {
    match (a, b) {
        (Some(true), _) | (_, Some(true)) => Some(true),
        (Some(false), Some(false)) => Some(false),
        _ => None,
    }
}
fn not(a: Option<bool>) -> Option<bool> {
    a.map(|value| !value)
}
fn absent(value: Option<&CanonicalScalar>) -> bool {
    value.is_none_or(|v| matches!(v, CanonicalScalar::Null))
}

#[test]
fn precedence_parentheses_and_negation_match_independent_multigraph_oracles() {
    let choices = [
        None,
        Some(CanonicalScalar::Null),
        Some(CanonicalScalar::Int(-1)),
        Some(CanonicalScalar::Int(2)),
        Some(CanonicalScalar::Bool(true)),
    ];
    let clauses = [
        "a.n < b.n OR b.n IS NULL AND a <> b",
        "(a.n < b.n OR b.n IS NULL) AND a <> b",
        "NOT (a.n < b.n OR b.n IS NULL) OR a = b",
        "NOT a.n < b.n AND (b.n IS NOT NULL OR a = b)",
    ];
    let queries: Vec<_> = clauses
        .iter()
        .map(|clause| query(&format!("MATCH (a)-[:R]->(b) WHERE {clause} RETURN a,b")))
        .collect();
    let universe = [
        (VId(0), R, VId(0)),
        (VId(0), R, VId(1)),
        (VId(1), R, VId(0)),
        (VId(1), R, VId(1)),
    ];
    for left in &choices {
        for right in &choices {
            for mask in 0..16 {
                let values = [left.clone(), right.clone()];
                let stored = values
                    .clone()
                    .map(|value| value.map(CanonicalPropertyValue::from));
                let mut edges: Vec<_> = universe
                    .iter()
                    .enumerate()
                    .filter(|(at, _)| mask & (1 << at) != 0)
                    .map(|(_, e)| *e)
                    .collect();
                if let Some(first) = edges.first().copied() {
                    edges.push(first);
                }
                for (kind, pattern) in queries.iter().enumerate() {
                    let mut expected = Vec::new();
                    for &(a, _, b) in &edges {
                        let lt = less(values[a.0 as usize].as_ref(), values[b.0 as usize].as_ref());
                        let null = Some(absent(values[b.0 as usize].as_ref()));
                        let ne = Some(a != b);
                        let keep = match kind {
                            0 => or(lt, and(null, ne)),
                            1 => and(or(lt, null), ne),
                            2 => or(not(or(lt, null)), not(ne)),
                            _ => and(not(lt), or(not(null), not(ne))),
                        };
                        if keep == Some(true) {
                            expected.push((a, b));
                        }
                    }
                    expected.sort();
                    let result = pattern
                        .plan()
                        .execute_governed_with_properties(
                            edges.len() as u64,
                            [],
                            edges.iter().copied(),
                            |_, _| Ok::<_, ()>(true),
                            |vid, _| Ok(stored[vid.0 as usize].as_ref()),
                            wide(),
                            || Ok::<_, ()>(()),
                        )
                        .unwrap();
                    let actual: Vec<_> = result
                        .value
                        .iter()
                        .map(|row| {
                            (
                                row.get(0).unwrap().as_vertex().unwrap(),
                                row.get(1).unwrap().as_vertex().unwrap(),
                            )
                        })
                        .collect();
                    assert_eq!(actual, expected, "mask={mask}, case={kind}");
                }
            }
        }
    }
}

#[test]
fn text_matches_typed_boolean_and_flat_conjunction_transcripts_are_unchanged() {
    let expression = GraphBooleanExpression::prepare(&[
        Op::Compare {
            left: Arg::Property {
                variable: "a",
                key: P,
            },
            comparison: Cmp::Less,
            right: Arg::Property {
                variable: "b",
                key: P,
            },
        },
        Op::IsNull {
            operand: Arg::Property {
                variable: "b",
                key: P,
            },
            is_null: true,
        },
        Op::Or,
        Op::Compare {
            left: Arg::Vertex("a"),
            comparison: Cmp::Equal,
            right: Arg::Vertex("b"),
        },
        Op::Not,
        Op::And,
    ])
    .unwrap();
    let mut builder = GraphPatternBuilder::new();
    builder.vertex("a").unwrap();
    builder.vertex("b").unwrap();
    builder.edge("a", R, GlaDirection::Forward, "b").unwrap();
    let columns = [GraphColumn::vertex("a", "a"), GraphColumn::vertex("b", "b")];
    let mut extended = builder.clone();
    extended.filter_boolean(&expression).unwrap();
    assert_eq!(
        query("MATCH (a)-[:R]->(b) WHERE (a.n<b.n OR b.n IS NULL) AND NOT (a=b) RETURN a,b"),
        extended
            .prepare_values(&columns, 0, None)
            .unwrap()
            .with_duplicates()
    );
    builder
        .filter(
            "a",
            VertexPredicate::IntegerProperty {
                key: P,
                comparison: Cmp::Greater,
                value: 1,
            },
        )
        .unwrap();
    builder.identity("a", "b", false).unwrap();
    let flat = query("MATCH (a)-[:R]->(b) WHERE a.n>1 AND a<>b RETURN a,b");
    assert_eq!(
        flat,
        builder
            .prepare_values(&columns, 0, None)
            .unwrap()
            .with_duplicates()
    );
    assert!(
        !flat
            .plan()
            .operators()
            .iter()
            .any(|op| matches!(op, GlaOperator::SelectBoolean { .. }))
    );
    assert!(
        query("MATCH (a) WHERE a.n=1 OR a.n=2 RETURN a")
            .plan()
            .operators()
            .iter()
            .any(|op| matches!(op, GlaOperator::SelectBoolean { .. }))
    );
    assert!(
        query("MATCH (a)-[:R]->(b) WHERE a.n=b.n RETURN a")
            .plan()
            .operators()
            .iter()
            .any(|op| matches!(op, GlaOperator::CompareProperties { .. }))
    );
}

#[test]
fn boolean_constants_and_keyword_named_variables_keep_null_and_identity_semantics() {
    for (expression, expected) in [
        ("TRUE", true),
        ("FALSE", false),
        ("NULL", false),
        ("NOT NULL", false),
        ("NULL OR TRUE", true),
        ("NULL AND FALSE", false),
        ("NOT (NULL AND FALSE)", true),
        ("NOT (NULL OR FALSE)", false),
    ] {
        let pattern = query(&format!("MATCH (a) WHERE {expression} RETURN a"));
        let result = pattern
            .plan()
            .execute_governed_with_properties(
                1,
                [VId(u128::MAX)],
                [],
                |_, _| Ok::<_, &str>(true),
                |_, _| Err("constant read property"),
                wide(),
                || Ok::<_, ()>(()),
            )
            .unwrap();
        assert_eq!(result.value.len(), usize::from(expected));
    }
    for statement in [
        "MATCH (not)-[:R]->(true) WHERE not.n<true.n OR NOT (not=true) RETURN not,true",
        "MATCH (false)-[:R]->(null) WHERE false.n=null.n OR null IS NOT NULL RETURN false,null",
        "MATCH (exists) WHERE exists.n=1 OR exists.n IS NULL RETURN exists",
    ] {
        assert!(PreparedGraphText::prepare(statement, symbols).is_ok());
    }
    // NOT keeps a missing or NULL operand UNKNOWN. A Bool is of another
    // domain than 1 (fgdb-v6ku6): `a.n = 1` is FALSE, so NOT keeps the row.
    for (value, kept) in [
        (None, false),
        (
            Some(CanonicalPropertyValue::from(CanonicalScalar::Null)),
            false,
        ),
        (
            Some(CanonicalPropertyValue::from(CanonicalScalar::Bool(true))),
            true,
        ),
    ] {
        let pattern = query("MATCH (a) WHERE NOT (a.n=1) RETURN a");
        let result = pattern
            .plan()
            .execute_governed_with_properties(
                1,
                [VId(0)],
                [],
                |_, _| Ok::<_, ()>(true),
                |_, _| Ok(value.as_ref()),
                wide(),
                || Ok::<_, ()>(()),
            )
            .unwrap();
        assert_eq!(result.value.len(), usize::from(kept));
    }
}

#[test]
fn declared_scalar_rebinding_keeps_schema_catalog_and_prior_plans_immutable() {
    let text = "MATCH (a) WHERE a.n=$status OR NOT (a.flag=$enabled) RETURN a LIMIT $take";
    let calls = Cell::new(0);
    let template = PreparedGraphText::prepare_with_parameter_types(
        text,
        &[
            (
                "status",
                GqlParameterType::Scalar(CanonicalScalarKind::Text),
            ),
            (
                "enabled",
                GqlParameterType::Scalar(CanonicalScalarKind::Bool),
            ),
        ],
        |kind: GraphSymbolKind, name: &str| {
            calls.set(calls.get() + 1);
            symbols(kind, name)
        },
    )
    .unwrap();
    let before = calls.get();
    assert_eq!(before, 2);
    let payload = "OR NOT EXISTS { MATCH () } 'quoted'";
    let args = GqlParameters::new()
        .with_text("status", payload)
        .unwrap()
        .with_bool("enabled", true)
        .unwrap()
        .with_uint64("take", 1)
        .unwrap();
    let original = template.bind_parameters(&args).unwrap();
    let frozen = original.canonical_bytes();
    let changed = template
        .bind_parameters(
            &GqlParameters::new()
                .with_text("status", "other")
                .unwrap()
                .with_bool("enabled", true)
                .unwrap()
                .with_uint64("take", 1)
                .unwrap(),
        )
        .unwrap();
    let status = CanonicalPropertyValue::from(CanonicalScalar::ucs_basic_text(payload).unwrap());
    let enabled = CanonicalPropertyValue::from(CanonicalScalar::Bool(true));
    for (pattern, count) in [(&original, 1), (&changed, 0)] {
        let result = pattern
            .plan()
            .execute_governed_with_properties(
                1,
                [VId(0)],
                [],
                |_, _| Ok::<_, ()>(true),
                |_, key| Ok(Some(if key == P { &status } else { &enabled })),
                wide(),
                || Ok::<_, ()>(()),
            )
            .unwrap();
        assert_eq!(result.value.len(), count);
    }
    assert_eq!(calls.get(), before);
    assert_eq!(original.canonical_bytes(), frozen);
    assert_ne!(changed.canonical_bytes(), frozen);
    assert!(matches!(
        template
            .bind_parameters(&GqlParameters::new())
            .unwrap_err()
            .kind,
        GraphPatternTextErrorKind::MissingParameter
    ));
    let wrong = GqlParameters::new()
        .with_int64("status", 1)
        .unwrap()
        .with_bool("enabled", true)
        .unwrap()
        .with_uint64("take", 1)
        .unwrap();
    assert!(matches!(
        template.bind_parameters(&wrong).unwrap_err().kind,
        GraphPatternTextErrorKind::ParameterTypeMismatch { .. }
    ));
    assert!(matches!(
        template
            .bind_parameters(&args.with_int64("extra", 1).unwrap())
            .unwrap_err()
            .kind,
        GraphPatternTextErrorKind::UnexpectedArguments
    ));
}

#[test]
fn optional_and_exists_children_use_boolean_selection_before_match_success_and_grouping() {
    let head = "MATCH (a) OPTIONAL MATCH (a)-[:R]->(b) WHERE NOT (b.n<=a.n) OR b.n IS NULL";
    let pattern = query(&format!("{head} RETURN a,b"));
    let values = [
        CanonicalScalar::Int(5),
        CanonicalScalar::Int(8),
        CanonicalScalar::Int(1),
        CanonicalScalar::Int(10),
        CanonicalScalar::Null,
    ]
    .map(CanonicalPropertyValue::from);
    let edges = [
        (VId(0), R, VId(1)),
        (VId(0), R, VId(1)),
        (VId(0), R, VId(2)),
        (VId(3), R, VId(2)),
    ];
    let rows = pattern
        .plan()
        .execute_governed_with_properties(
            7,
            [VId(0), VId(3), VId(4)],
            edges,
            |_, _| Ok::<_, ()>(true),
            |vid, _| Ok(Some(&values[vid.0 as usize])),
            wide(),
            || Ok::<_, ()>(()),
        )
        .unwrap()
        .value;
    let actual: Vec<_> = rows
        .iter()
        .map(|row| {
            (
                row.get(0).unwrap().as_vertex().unwrap(),
                row.get(1).unwrap().as_vertex(),
            )
        })
        .collect();
    assert_eq!(
        actual,
        vec![
            (VId(0), Some(VId(1))),
            (VId(0), Some(VId(1))),
            (VId(3), None),
            (VId(4), None)
        ]
    );
    let aggregate=PreparedGraphAggregateText::prepare(&format!("{head} RETURN a,COUNT(*) AS n,COUNT(b) AS present GROUP BY a HAVING n>=1 ORDER BY n DESC,a"),symbols)
        .unwrap().bind_parameters(&GqlParameters::new()).unwrap();
    let rows = aggregate
        .execute_governed(
            7,
            [VId(0), VId(3), VId(4)],
            edges,
            |_, _| Ok::<_, ()>(true),
            |vid, _| Ok(Some(&values[vid.0 as usize])),
            wide(),
            || Ok::<_, ()>(()),
        )
        .unwrap()
        .value;
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].get(0).unwrap().as_count(), Some(2));
    assert!(
        rows[1..]
            .iter()
            .all(|row| row.get(1).unwrap().as_count() == Some(0))
    );
    for anti in [false, true] {
        let pattern = query(&format!(
            "MATCH (a) WHERE {}EXISTS {{ MATCH (a)-[:R]->(b) WHERE b.n>a.n OR b.n IS NULL }} RETURN a",
            if anti { "NOT " } else { "" }
        ));
        let rows = pattern
            .plan()
            .execute_governed_with_properties(
                7,
                [VId(0), VId(3), VId(4)],
                edges,
                |_, _| Ok::<_, ()>(true),
                |vid, _| Ok(Some(&values[vid.0 as usize])),
                wide(),
                || Ok::<_, ()>(()),
            )
            .unwrap()
            .value;
        let actual: Vec<_> = rows
            .iter()
            .map(|row| row.get(0).unwrap().as_vertex().unwrap())
            .collect();
        assert_eq!(
            actual,
            if anti {
                vec![VId(3), VId(4)]
            } else {
                vec![VId(0)]
            }
        );
    }
    assert!(
        PreparedGraphText::prepare(
            "MATCH (a) WHERE a.n=1 AND EXISTS { MATCH (a)-[:R]->(b) } AND a.n>0 RETURN a",
            symbols
        )
        .is_ok()
    );
}

/// openCypher `v:L` in WHERE (TCK MatchWhere1 [1]): a top-level AND conjunct
/// is the label `(v:L)` writes inline, the same plan byte for byte. Under OR,
/// NOT or parentheses the pattern compiler refuses it, as it does a name the
/// scope does not bind; a read's row filter takes those (the next law).
#[test]
fn a_where_label_conjunct_is_the_inline_label() {
    for (conjunct, inline) in [
        ("MATCH (a) WHERE a:L RETURN a", "MATCH (a:L) RETURN a"),
        (
            "MATCH (a)-[:R]->(b) WHERE b:L AND b.n > 1 RETURN a, b",
            "MATCH (a)-[:R]->(b:L) WHERE b.n > 1 RETURN a, b",
        ),
        (
            "MATCH (a) WHERE a.n = 2 AND a:L RETURN a",
            "MATCH (a:L) WHERE a.n = 2 RETURN a",
        ),
    ] {
        assert_eq!(
            query(conjunct).canonical_bytes(),
            query(inline).canonical_bytes(),
            "{conjunct}"
        );
    }
    for text in [
        "MATCH (a) WHERE NOT a:L RETURN a",
        "MATCH (a) WHERE a:L OR a.n = 1 RETURN a",
        "MATCH (a) WHERE (a:L) RETURN a",
        "MATCH (a) WHERE b:L RETURN a",
    ] {
        assert!(PreparedGraphText::prepare(text, symbols).is_err(), "{text}");
    }
}

/// fgdb-qo3jv: under OR, NOT or parentheses, or beside a conjunct the
/// Boolean parser reads as a compound expression, a WHERE label test is a
/// Boolean leaf the pattern filter cannot read, and so is a relationship's
/// `r:T` anywhere. A read's MATCH WHERE then runs as the row filter after an
/// implicit `WITH *` (fgdb-1c29d), where `v:L` reads `'L' IN labels(v)`: the
/// same template as writing that `WITH *`, also before a later MATCH or
/// OPTIONAL MATCH (TCK Match7 [25]), which then joins the filtered rows. A
/// flat conjunction keeps its inline label, byte for byte. An OPTIONAL
/// MATCH's own WHERE belongs to its left join, so there a compound label test
/// still refuses, as does a name no clause binds.
#[test]
fn a_compound_where_label_test_filters_after_an_implicit_with_star() {
    let template = |text: &str| {
        fgdb_gql::PreparedGraphSetText::prepare(text, symbols)
            .unwrap_or_else(|error| panic!("{text}: {error:?}"))
            .canonical_template_bytes()
    };
    for (pattern, predicate, tail) in [
        ("(a)", "NOT a:L", "RETURN a"),
        ("(a)", "a:L OR a.n = 1", "RETURN a"),
        ("(a)", "(a:L)", "RETURN a"),
        ("(a)", "NOT (a:L:M) AND a.n > 0", "RETURN a.n AS n"),
        ("(a)", "a:L AND (a.n = 1 OR a.n = 2)", "RETURN a"),
        ("(a)", "a:L AND a.n IN [1, 2]", "RETURN a"),
        ("(a)-[r]->(b)", "r:R", "RETURN a, b"),
        ("(a)-[r]->(b)", "r:R OR b:L", "RETURN a, b"),
        ("(a)", "NOT (a:L)", "OPTIONAL MATCH (a)-[r:R]->(a) RETURN r"),
        ("(a)", "a:L OR a.n = 1", "MATCH (a)-[:R]->(b) RETURN b"),
    ] {
        let deferred = template(&format!("MATCH {pattern} WHERE {predicate} {tail}"));
        let explicit = template(&format!("MATCH {pattern} WITH * WHERE {predicate} {tail}"));
        assert_eq!(deferred, explicit, "{predicate} {tail}");
    }
    for (conjunct, inline) in [
        ("MATCH (a) WHERE a:L RETURN a", "MATCH (a:L) RETURN a"),
        (
            "MATCH (a) WHERE a.n = 2 AND a:L RETURN a.n AS n",
            "MATCH (a:L) WHERE a.n = 2 RETURN a.n AS n",
        ),
        (
            "MATCH (a) WHERE a:L AND (a)-[:R]->() RETURN a",
            "MATCH (a:L) WHERE (a)-[:R]->() RETURN a",
        ),
    ] {
        assert_eq!(template(conjunct), template(inline), "{conjunct}");
    }
    for text in [
        "MATCH (a) OPTIONAL MATCH (a)-[:R]->(b) WHERE b:L OR b.n = 1 RETURN b",
        "MATCH (a) WHERE b:L OR a.n = 1 RETURN a",
    ] {
        assert!(
            fgdb_gql::PreparedGraphSetText::prepare(text, symbols).is_err(),
            "{text}"
        );
    }
}

/// openCypher `v:L` as a value (TCK Graph5, Return2 [8], fgdb-qo3jv): a
/// vertex test compiles to exactly `'L' IN labels(v)`, a conjunction of
/// labels to the AND of those, and a relationship test to `type(r) = 'T'`.
/// NULL elements and names no catalog knows therefore behave as those forms
/// do. A vertex test needs a value context, so a SET right-hand side
/// refuses, and a row-scope name is not a graph element.
#[test]
fn a_label_test_value_is_label_membership_or_type_equality() {
    let template = |text: &str| {
        fgdb_gql::PreparedGraphSetText::prepare(text, symbols)
            .unwrap_or_else(|error| panic!("{text}: {error:?}"))
            .canonical_template_bytes()
    };
    for (test, spelled) in [
        (
            "MATCH (a) RETURN a, a:L AS result",
            "MATCH (a) RETURN a, 'L' IN labels(a) AS result",
        ),
        (
            "MATCH (a) RETURN (a:L) AS result",
            "MATCH (a) RETURN ('L' IN labels(a)) AS result",
        ),
        (
            "MATCH (a) RETURN a:L:M AS result",
            "MATCH (a) RETURN 'L' IN labels(a) AND 'M' IN labels(a) AS result",
        ),
        (
            "MATCH (a)-[r:R]->(b) RETURN r, r:R AS result",
            "MATCH (a)-[r:R]->(b) RETURN r, type(r) = 'R' AS result",
        ),
        (
            "MATCH (a) OPTIONAL MATCH (a)-[:R]->(m) RETURN m:L AS result",
            "MATCH (a) OPTIONAL MATCH (a)-[:R]->(m) RETURN 'L' IN labels(m) AS result",
        ),
        (
            "MATCH (a) WITH a, a:L AS l RETURN a, l",
            "MATCH (a) WITH a, 'L' IN labels(a) AS l RETURN a, l",
        ),
    ] {
        assert_eq!(template(test), template(spelled), "{test}");
    }
    assert!(
        fgdb_gql::PreparedGraphSetText::prepare("MATCH (a) WITH a.n AS n RETURN n:L AS x", symbols)
            .is_err()
    );
    // A written label test is the same Boolean value a RETURN reads: SET
    // stores true or false (both the CLI and the TCK runner read it back).
    // Both spellings lower to one canonical mutation.
    assert_eq!(
        fgdb_gql::PreparedGraphMutationText::prepare(
            "MATCH (a) SET a.n = a:L",
            RelationId(1),
            symbols,
        )
        .unwrap()
        .bind_parameters(&fgdb_gql::GqlParameters::new())
        .unwrap()
        .canonical_bytes(),
        fgdb_gql::PreparedGraphMutationText::prepare(
            "MATCH (a) SET a.n = 'L' IN labels(a)",
            RelationId(1),
            symbols,
        )
        .unwrap()
        .bind_parameters(&fgdb_gql::GqlParameters::new())
        .unwrap()
        .canonical_bytes()
    );
}

/// openCypher binds IN tighter than comparisons, AND, OR and XOR, and looser
/// than `+` (TCK Precedence3 [4]-[6]). The list operand of an IN therefore
/// ends before a following Boolean operator even when it is a graph function,
/// a name or a parameter: each unparenthesized spelling is its parenthesized
/// one (fgdb-klah2). The list used to swallow the rest of the conjunction.
#[test]
fn in_ends_its_list_operand_before_boolean_operators() {
    let template = |text: &str| {
        fgdb_gql::PreparedGraphSetText::prepare(text, symbols)
            .unwrap_or_else(|error| panic!("{text}: {error:?}"))
            .canonical_template_bytes()
    };
    for (bare, grouped) in [
        (
            "MATCH (a) RETURN 'L' IN labels(a) AND 'M' IN labels(a) AS r",
            "MATCH (a) RETURN ('L' IN labels(a)) AND ('M' IN labels(a)) AS r",
        ),
        (
            "MATCH (a) RETURN 'L' IN labels(a) OR 'M' IN labels(a) AS r",
            "MATCH (a) RETURN ('L' IN labels(a)) OR ('M' IN labels(a)) AS r",
        ),
        (
            "MATCH (a) RETURN 'L' IN labels(a) AND a.n = 1 AS r",
            "MATCH (a) RETURN ('L' IN labels(a)) AND a.n = 1 AS r",
        ),
        (
            "MATCH (a) RETURN a.n = 1 AND 'L' IN labels(a) AND 'M' IN labels(a) AS r",
            "MATCH (a) RETURN a.n = 1 AND ('L' IN labels(a)) AND ('M' IN labels(a)) AS r",
        ),
        (
            "MATCH (a) RETURN 'n' IN keys(a) AND 'L' IN labels(a) AS r",
            "MATCH (a) RETURN ('n' IN keys(a)) AND ('L' IN labels(a)) AS r",
        ),
        (
            "MATCH (a) WITH a, labels(a) AS l RETURN 'L' IN l AND 'M' IN l AS r",
            "MATCH (a) WITH a, labels(a) AS l RETURN ('L' IN l) AND ('M' IN l) AS r",
        ),
    ] {
        assert_eq!(template(bare), template(grouped), "{bare}");
        // The grouping is not vacuous: the conjunction is not one membership.
        assert_ne!(
            template(bare),
            template("MATCH (a) RETURN 'L' IN labels(a) AS r"),
            "{bare}"
        );
    }
    // A lone membership and a literal list keep their reading.
    for text in [
        "MATCH (a) RETURN 'L' IN labels(a) AS r",
        "MATCH (a) RETURN 1 IN [1] AND 2 IN [2] AS r",
    ] {
        template(text);
    }
}

/// fgdb-dn9q3: `a.n IN $xs` with a declared List argument is the literal
/// list's membership, member for member: three-valued, NOT outside the
/// membership, an empty list FALSE, inside AND/OR as any predicate. A NULL
/// argument is UNKNOWN for every vertex. A nested member refuses at binding,
/// and the template stays bindable.
#[test]
fn a_list_argument_in_a_pattern_where_is_the_literal_lists_membership() {
    use fgdb_gql::algebra::GraphValue;
    let values = [
        Some(CanonicalScalar::Int(1)),
        Some(CanonicalScalar::Int(2)),
        Some(CanonicalScalar::Int(3)),
        Some(CanonicalScalar::Null),
        None,
    ]
    .map(|value| value.map(CanonicalPropertyValue::from));
    let rows = |pattern: &PreparedGraphPattern<GraphValueRow>| -> Vec<VId> {
        pattern
            .plan()
            .execute_governed_with_properties(
                values.len() as u64,
                (1..=values.len()).map(|vid| VId(vid as u128)),
                [],
                |_, _| Ok::<_, ()>(true),
                |vid, _| Ok(values[vid.0 as usize - 1].as_ref()),
                wide(),
                || Ok::<_, ()>(()),
            )
            .unwrap()
            .value
            .iter()
            .map(|row| row.values()[0].as_vertex().unwrap())
            .collect()
    };
    let calls = Cell::new(0);
    let declared = |predicate: &str| {
        PreparedGraphText::prepare_with_parameter_types(
            &format!("MATCH (a) WHERE {predicate} RETURN a"),
            &[("xs", GqlParameterType::List)],
            |kind: GraphSymbolKind, name: &str| {
                calls.set(calls.get() + 1);
                symbols(kind, name)
            },
        )
        .unwrap_or_else(|error| panic!("{predicate}: {error:?}"))
    };
    let list = |members: &[Option<i64>]| {
        GqlParameters::new()
            .with_list(
                "xs",
                members
                    .iter()
                    .map(|member| {
                        GraphValue::Scalar(
                            member.map_or(CanonicalScalar::Null, CanonicalScalar::Int),
                        )
                    })
                    .collect(),
            )
            .unwrap()
    };
    for (predicate, literal, members, expected) in [
        (
            "a.n IN $xs",
            "a.n IN [1, 3, 3]",
            &[Some(1), Some(3), Some(3)][..],
            &[1, 3][..],
        ),
        (
            "a.n NOT IN $xs",
            "a.n NOT IN [1, 3]",
            &[Some(1), Some(3)],
            &[2],
        ),
        ("a.n IN $xs", "a.n IN [1, NULL]", &[Some(1), None], &[1]),
        (
            "NOT a.n IN $xs",
            "NOT a.n IN [1, NULL]",
            &[Some(1), None],
            &[],
        ),
        ("a.n IN $xs", "a.n IN []", &[], &[]),
        ("a.n NOT IN $xs", "a.n NOT IN []", &[], &[1, 2, 3, 4, 5]),
        (
            "a.n IN $xs AND a.n > 1",
            "a.n IN [1, 2, 3] AND a.n > 1",
            &[Some(1), Some(2), Some(3)],
            &[2, 3],
        ),
        (
            "a.n IN $xs OR a.n IS NULL",
            "a.n IN [2] OR a.n IS NULL",
            &[Some(2)],
            &[2, 4, 5],
        ),
    ] {
        let expected: Vec<_> = expected.iter().map(|&vid| VId(vid)).collect();
        let template = declared(predicate);
        assert_eq!(
            rows(&template.bind_parameters(&list(members)).unwrap()),
            expected,
            "{predicate}"
        );
        assert_eq!(
            rows(&query(&format!("MATCH (a) WHERE {literal} RETURN a"))),
            expected,
            "{literal}"
        );
    }
    // One catalog call per statement: the list never re-enters resolution.
    assert_eq!(calls.get(), 8);
    let template = declared("a.n IN $xs");
    let null = GqlParameters::new().with_null("xs").unwrap();
    assert!(rows(&template.bind_parameters(&null).unwrap()).is_empty());
    let negated = declared("a.n NOT IN $xs");
    assert!(rows(&negated.bind_parameters(&null).unwrap()).is_empty());
    let nested = GqlParameters::new()
        .with_list(
            "xs",
            vec![GraphValue::List(
                vec![GraphValue::Scalar(CanonicalScalar::Int(1))].into(),
            )],
        )
        .unwrap();
    assert!(template.bind_parameters(&nested).is_err());
    assert_eq!(
        rows(&template.bind_parameters(&list(&[Some(2)])).unwrap()),
        [VId(2)]
    );
}

#[test]
fn malformed_boolean_programs_and_scope_mixtures_refuse_before_catalog_resolution() {
    for statement in [
        "MATCH (a) WHERE () RETURN a",
        "MATCH (a) WHERE NOT RETURN a",
        "MATCH (a) WHERE a.n=1 OR OR a.n=2 RETURN a",
        "MATCH (a) WHERE (a.n=1 RETURN a",
        "MATCH (a) WHERE a.n=1) RETURN a",
        "MATCH (a) WHERE a.n=1 OR missing.n=2 RETURN a",
        "MATCH (a) WHERE NOT (a>a) RETURN a",
        "MATCH (a) WHERE a.n=b.n RETURN a",
        "MATCH (a) WHERE a.n=1 OR EXISTS { MATCH (a) } RETURN a",
        "MATCH (a) WHERE a.n=1 OR a.n=2 AND EXISTS { MATCH (a) } RETURN a",
        "MATCH (a) WHERE EXISTS { MATCH (a) } AND (a.n=1 OR a.n=2) RETURN a",
        "MATCH (a) WHERE NOT (EXISTS { MATCH (a) }) RETURN a",
        "MATCH (a) OPTIONAL MATCH (a)-[:R]->(b) WHERE TRUE OR EXISTS { MATCH (b) } RETURN a",
        "MATCH (a) WHERE a IS NOT TRUE RETURN a",
    ] {
        let mut calls = 0;
        assert!(
            PreparedGraphText::prepare(statement, |kind: GraphSymbolKind, name: &str| {
                calls += 1;
                symbols(kind, name)
            })
            .is_err(),
            "{statement}"
        );
        assert_eq!(calls, 0, "{statement}");
    }
    let nested = |count| {
        format!(
            "MATCH (a) WHERE {}a.n=1{} RETURN a",
            "(".repeat(count),
            ")".repeat(count)
        )
    };
    assert!(PreparedGraphText::prepare(&nested(64), symbols).is_ok());
    assert!(matches!(
        PreparedGraphText::prepare(&nested(65), symbols)
            .unwrap_err()
            .kind,
        GraphPatternTextErrorKind::BooleanNesting { limit: 64 }
    ));
    let too_many = format!("MATCH (a) WHERE {}TRUE RETURN a", "TRUE OR ".repeat(256));
    let mut calls = 0;
    assert!(
        PreparedGraphText::prepare(&too_many, |kind: GraphSymbolKind, name: &str| {
            calls += 1;
            symbols(kind, name)
        })
        .is_err()
    );
    assert_eq!(calls, 0);
    let unicode = "\u{2003}MATCH (a) WHERE NOT (a.n='雪''OR') OR a.n IS NULL RETURN a";
    for end in (0..=unicode.len()).filter(|end| unicode.is_char_boundary(*end)) {
        let _ = PreparedGraphText::prepare(&unicode[..end], symbols);
    }
    assert!(PreparedGraphText::prepare(unicode, symbols).is_ok());
}

#[test]
fn boolean_text_keeps_exact_controls_and_eager_errors_even_with_a_zero_page() {
    let pattern = query("MATCH (a)-[:R]->(b) WHERE a.n<b.n OR b.n IS NULL RETURN a,b LIMIT 1");
    let values =
        [CanonicalScalar::Int(0), CanonicalScalar::Int(2)].map(CanonicalPropertyValue::from);
    let edges = [(VId(0), R, VId(1)); 2];
    let mut total = 0;
    let measured = pattern
        .plan()
        .execute_governed_with_properties(
            2,
            [],
            edges,
            |_, _| Ok::<_, ()>(true),
            |vid, _| Ok(Some(&values[vid.0 as usize])),
            wide(),
            || {
                total += 1;
                Ok::<_, usize>(())
            },
        )
        .unwrap();
    let exact = GqlQueryPolicy::new(
        2,
        1,
        measured.evaluator.work_units,
        measured.evaluator.scratch_entries,
    );
    let run = |cap| {
        pattern.plan().execute_governed_with_properties(
            2,
            [],
            edges,
            |_, _| Ok::<_, ()>(true),
            |vid, _| Ok(Some(&values[vid.0 as usize])),
            cap,
            || Ok::<_, usize>(()),
        )
    };
    assert_eq!(run(exact).unwrap(), measured);
    for cap in [
        GqlQueryPolicy::new(2, 0, u64::MAX, u64::MAX),
        GqlQueryPolicy::new(2, 1, measured.evaluator.work_units - 1, u64::MAX),
        GqlQueryPolicy::new(2, 1, u64::MAX, measured.evaluator.scratch_entries - 1),
    ] {
        assert!(run(cap).is_err());
    }
    for stop in 1..=total {
        let mut at = 0;
        let result = pattern.plan().execute_governed_with_properties(
            2,
            [],
            edges,
            |_, _| Ok::<_, ()>(true),
            |vid, _| Ok(Some(&values[vid.0 as usize])),
            wide(),
            || {
                at += 1;
                if at == stop { Err(stop) } else { Ok(()) }
            },
        );
        assert!(matches!(result,Err(GqlQueryError::Interrupted(value))if value==stop));
        assert_eq!(at, stop);
    }
    for count in [0, 1] {
        let pattern = query(&format!(
            "MATCH (a)-[:R]->(b) WHERE TRUE OR b.n=2 RETURN a LIMIT {count}"
        ));
        let mut reads = 0;
        let result = pattern.plan().execute_governed_with_properties(
            2,
            [],
            edges,
            |_, _| Ok::<_, &str>(true),
            |_, _| {
                reads += 1;
                if reads == 2 {
                    Err("late source failure")
                } else {
                    Ok(Some(&values[1]))
                }
            },
            wide(),
            || Ok::<_, ()>(()),
        );
        assert!(matches!(
            result,
            Err(GqlQueryError::Source("late source failure"))
        ));
        assert_eq!(reads, 2);
    }
}
