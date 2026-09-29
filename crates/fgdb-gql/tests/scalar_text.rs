//! Literal predicates are parsed once and use the same canonical GLA selector.
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::{
    GraphColumn, GraphMatchClause, GraphPatternBuilder, IntegerComparison, ScalarPredicate,
    VertexPredicate,
};
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphPatternTextErrorKind, GraphSymbol, GraphSymbolKind,
    PreparedGraphAggregateText, PreparedGraphSetText, PreparedGraphText, PreparedGraphWriteScript,
    PreparedTemporalGraphText,
};
use fgdb_types::{CanonicalScalar, VId};
use std::collections::BTreeMap;

const P: PropertyKeyId = PropertyKeyId(1);
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(RelationId(1))),
        (GraphSymbolKind::Label, "L") => Some(GraphSymbol::Label(LabelId(1))),
        _ => None,
    }
}
fn wide() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100, 100, u64::MAX, u64::MAX)
}
fn text(value: &str) -> CanonicalScalar {
    CanonicalScalar::ucs_basic_text(value).unwrap()
}
fn ids(statement: &str, values: &[Option<CanonicalScalar>]) -> Vec<VId> {
    let query = PreparedGraphText::prepare(statement, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
    query
        .plan()
        .execute_governed_with_properties(
            values.len() as u64,
            (0..values.len()).map(|i| VId(i as u128)),
            [],
            |vid, predicates| {
                let property = values[vid.0 as usize].as_ref().map(|value| (P, value));
                Ok::<_, ()>(predicates.iter().all(|p| p.matches_borrowed([], property)))
            },
            |vid, _| Ok(values[vid.0 as usize].as_ref()),
            wide(),
            || Ok::<_, ()>(()),
        )
        .unwrap()
        .value
        .iter()
        .map(|row| row.get(0).unwrap().as_vertex().unwrap())
        .collect()
}

#[test]
fn quoted_boolean_and_null_operands_follow_independent_value_expectations() {
    let values = [
        None,
        Some(CanonicalScalar::Null),
        Some(text("ready")),
        Some(text("other")),
        Some(CanonicalScalar::Bool(true)),
        Some(CanonicalScalar::Bool(false)),
        Some(CanonicalScalar::Int(1)),
        Some(text("TRUE")),
        Some(text("")),
    ];
    for (predicate, expected) in [
        ("n.p = 'ready'", vec![2]),
        ("n.p != 'ready'", vec![3, 7, 8]),
        ("n.p < 'ready'", vec![3, 7, 8]),
        ("n.p >= 'ready'", vec![2]),
        ("n.p = TRUE", vec![4]),
        ("n.p <> false", vec![4]),
        ("n.p <= FALSE", vec![5]),
        ("n.p = 'TRUE'", vec![7]),
        ("n.p = ''", vec![8]),
        ("n.p IS NULL", vec![0, 1]),
        ("n.p IS NOT NULL", vec![2, 3, 4, 5, 6, 7, 8]),
        ("n.p = NULL", vec![]),
        ("n.p <> NULL", vec![]),
        ("n.p = 1", vec![6]),
    ] {
        assert_eq!(
            ids(&format!("MATCH (n) WHERE {predicate} RETURN n"), &values),
            expected.into_iter().map(VId).collect::<Vec<_>>(),
            "{predicate}"
        );
    }
}

#[test]
fn quoted_payloads_are_not_query_tokens_and_doubled_quotes_are_the_only_escape() {
    for value in [
        "O'Reilly",
        "é猫🦀",
        "x' OR n.p IS NOT NULL",
        "{} $fake RETURN",
        "a\\b",
        "line\nnext",
        "\0",
        "'",
    ] {
        let encoded = value.replace('\'', "''");
        let statement = format!("MATCH (n) WHERE n.p = '{encoded}' RETURN n");
        // The same payload double-quoted, where only a doubled " escapes
        // (fgdb-285i2): the same rows and the same plan bytes.
        let double = format!(
            "MATCH (n) WHERE n.p = \"{}\" RETURN n",
            value.replace('"', "\"\"")
        );
        for statement in [&statement, &double] {
            assert_eq!(
                ids(
                    statement,
                    &[Some(text(value)), None, Some(text("different"))]
                ),
                vec![VId(0)],
                "{statement}"
            );
        }
        let plan = |statement: &str| {
            PreparedGraphText::prepare(statement, symbols)
                .unwrap()
                .bind_parameters(&GqlParameters::new())
                .unwrap()
                .canonical_bytes()
        };
        assert_eq!(plan(&statement), plan(&double), "{double}");
        let prepared = PreparedGraphText::prepare(&statement, symbols).unwrap();
        assert!(prepared.parameter_schema().is_empty());
        assert!(!format!("{prepared:?}").contains(value));
        for at in (0..statement.len()).filter(|at| statement.is_char_boundary(*at)) {
            let _ = PreparedGraphText::prepare(&statement[..at], symbols);
        }
    }
}

/// One lexical rule (fgdb-285i2): comments are trivia, a double-quoted
/// literal is text, and a backtick-delimited identifier is the identifier it
/// spells. Each spelling prepares to exactly the plan of its plain form.
#[test]
fn comments_double_quotes_and_delimited_identifiers_prepare_the_plain_plan() {
    let plan = |statement: &str| {
        PreparedGraphText::prepare(statement, symbols)
            .expect(statement)
            .bind_parameters(&GqlParameters::new())
            .unwrap()
            .canonical_bytes()
    };
    let plain = plan("MATCH (n:L)-[:R]->(m) WHERE n.p = 'a;b' RETURN n, m.p ORDER BY m.p");
    for spelling in [
        "MATCH (n:L)-[:R]->(m) WHERE n.p = \"a;b\" RETURN n, m.p ORDER BY m.p",
        "// leading comment\nMATCH (n:L)-[:R]->(m) WHERE n.p = 'a;b' RETURN n, m.p ORDER BY m.p",
        "MATCH /* inline */ (n:L)-[:R]->(m) WHERE n.p = 'a;b' RETURN n, m.p ORDER BY m.p // tail",
        "MATCH (n:L)-[:R]->(m) WHERE n.p = 'a;b' // RETURN nothing\n RETURN n, m.p ORDER BY m.p",
        "MATCH (`n`:`L`)-[:`R`]->(`m`) WHERE `n`.`p` = 'a;b' RETURN `n`, `m`.p ORDER BY m.`p`",
        "MATCH (n:L)-[:R]->(m)/**/WHERE n.p = \"a;b\"/* '\" */RETURN n, m.p ORDER BY m.p",
    ] {
        assert_eq!(plan(spelling), plain, "{spelling}");
    }
    // Delimiters and comment markers inside a literal are its payload.
    let quoted = |value: &str| {
        ids(
            &format!("MATCH (n) WHERE n.p = {value} RETURN n"),
            &[
                Some(text("it's")),
                Some(text("say \"hi\"")),
                Some(text("/* x */")),
                Some(text("a\\b")),
            ],
        )
    };
    assert_eq!(quoted("\"it's\""), vec![VId(0)]);
    assert_eq!(quoted("'say \"hi\"'"), vec![VId(1)]);
    assert_eq!(quoted("\"say \"\"hi\"\"\""), vec![VId(1)]);
    assert_eq!(quoted("\"/* x */\""), vec![VId(2)]);
    // A backslash is an ordinary byte in both quote styles.
    assert_eq!(quoted("\"a\\b\""), vec![VId(3)]);
    assert_eq!(quoted("'a\\b'"), vec![VId(3)]);
}

/// The temporal root scanner, UNION composition, aggregate text and the
/// write-script splitter see the same trivia and quoted spans as the graph
/// lexer: clause keywords or a `;` inside a comment or a double-quoted literal
/// are not syntax.
#[test]
fn every_statement_family_shares_the_lexical_rule() {
    let temporal = |statement: &str| {
        PreparedTemporalGraphText::prepare(statement, symbols)
            .expect(statement)
            .canonical_template_bytes()
    };
    assert_eq!(
        temporal(
            "MATCH (n) /* FOR SYSTEM_TIME AS OF SEQ 9 */ FOR SYSTEM_TIME AS OF SEQ 3 \
             // FOR SYSTEM_TIME AS OF SEQ 8\n RETURN n"
        ),
        temporal("MATCH (n) FOR SYSTEM_TIME AS OF SEQ 3 RETURN n")
    );
    let set = |statement: &str| {
        PreparedGraphSetText::prepare(statement, symbols)
            .expect(statement)
            .canonical_template_bytes()
    };
    assert_eq!(
        set(
            "MATCH (a) RETURN a.p AS x /* UNION */ UNION MATCH (b) WHERE b.p = \"y\" RETURN b.p AS x"
        ),
        set("MATCH (a) RETURN a.p AS x UNION MATCH (b) WHERE b.p = 'y' RETURN b.p AS x")
    );
    let aggregate = |statement: &str| {
        PreparedGraphAggregateText::prepare(statement, symbols)
            .expect(statement)
            .canonical_template_bytes()
    };
    assert_eq!(
        aggregate("MATCH (`n`) WHERE n.p = \"k\" // count them\n RETURN count(*) AS c"),
        aggregate("MATCH (n) WHERE n.p = 'k' RETURN count(*) AS c")
    );
    let args = GqlParameters::new();
    let script = |text: &str| {
        let script = PreparedGraphWriteScript::prepare(text, RelationId(1), symbols).expect(text);
        (
            script.statements().len(),
            script.bind_parameters(&args).expect(text),
        )
    };
    assert_eq!(
        script(
            "CREATE (n {p: \"a;b\"}); /* ; */ MATCH (n) WHERE n.p = \"a;b\" DELETE n // ;\n; // done"
        ),
        script("CREATE (n {p: 'a;b'}); MATCH (n) WHERE n.p = 'a;b' DELETE n")
    );
}

#[test]
fn unclosed_or_keyword_delimited_spans_refuse_before_catalog_calls() {
    for (statement, expected) in [
        ("MATCH (n) WHERE n.p = \"x RETURN n", "closing double quote"),
        ("MATCH (`n) RETURN n", "closing backtick"),
        (
            "MATCH (n) /* never closed RETURN n",
            "*/ closing the block comment",
        ),
        (
            "MATCH (``) RETURN n",
            "a nonempty delimited identifier without a doubled backtick",
        ),
        (
            "MATCH (`a``b`) RETURN n",
            "a nonempty delimited identifier without a doubled backtick",
        ),
        (
            "MATCH (`MATCH`) RETURN n",
            "a delimited identifier that is not a keyword",
        ),
        (
            "MATCH (n) WHERE n.p = `true` RETURN n",
            "a delimited identifier that is not a keyword",
        ),
        (
            "MATCH (n) RETURN n AS `return`",
            "a delimited identifier that is not a keyword",
        ),
    ] {
        let mut calls = 0;
        let error = PreparedGraphText::prepare(statement, |kind: GraphSymbolKind, name: &str| {
            calls += 1;
            symbols(kind, name)
        })
        .unwrap_err();
        assert_eq!(
            error.kind,
            GraphPatternTextErrorKind::Expected(expected),
            "{statement}"
        );
        assert_eq!(calls, 0, "{statement}");
    }
}

#[test]
fn resolved_literals_are_reused_when_numeric_parameters_are_rebound() {
    let mut calls = BTreeMap::new();
    let template = PreparedGraphText::prepare(
        "MATCH (n:L) WHERE n.p = 'ready' AND n.p IS NOT NULL RETURN n LIMIT $take",
        |kind: GraphSymbolKind, name: &str| {
            *calls.entry((kind, name.to_owned())).or_insert(0) += 1;
            symbols(kind, name)
        },
    )
    .unwrap();
    assert!(calls.values().all(|n| *n == 1));
    let first = template
        .bind_parameters(&GqlParameters::new().with_uint64("take", 1).unwrap())
        .unwrap();
    let old = first.canonical_bytes();
    let second = template
        .bind_parameters(&GqlParameters::new().with_uint64("take", 2).unwrap())
        .unwrap();
    assert_ne!(old, second.canonical_bytes());
    assert_eq!(old, first.canonical_bytes());
    assert!(matches!(
        template
            .bind_parameters(&GqlParameters::new().with_int64("take", 1).unwrap())
            .unwrap_err()
            .kind,
        GraphPatternTextErrorKind::ParameterTypeMismatch { .. }
    ));
    let mut expected = GraphPatternBuilder::new();
    expected.vertex("n").unwrap();
    expected
        .filter("n", VertexPredicate::HasLabel(LabelId(1)))
        .unwrap();
    expected
        .filter(
            "n",
            VertexPredicate::ScalarProperty {
                key: P,
                predicate: ScalarPredicate::new(text("ready"), IntegerComparison::Equal).unwrap(),
            },
        )
        .unwrap();
    expected
        .filter(
            "n",
            VertexPredicate::PropertyNull {
                key: P,
                is_null: false,
            },
        )
        .unwrap();
    assert_eq!(
        first,
        expected
            .prepare_values(&[GraphColumn::vertex("n", "n")], 0, Some(1))
            .unwrap()
            .with_duplicates()
    );
}

#[test]
fn optional_and_existential_children_share_scalar_lowering_and_aggregate_null_rules() {
    let mut root = GraphPatternBuilder::new();
    root.vertex("n").unwrap();
    let mut child = GraphPatternBuilder::new();
    child.vertex("n").unwrap();
    child.vertex("c").unwrap();
    child
        .edge(
            "n",
            RelationId(1),
            fgdb_gql::algebra::GlaDirection::Forward,
            "c",
        )
        .unwrap();
    child
        .filter(
            "c",
            VertexPredicate::ScalarProperty {
                key: P,
                predicate: ScalarPredicate::new(
                    CanonicalScalar::Bool(true),
                    IntegerComparison::Equal,
                )
                .unwrap(),
            },
        )
        .unwrap();
    let typed = root
        .prepare_values_with_clauses(
            &[GraphMatchClause::optional(&child)],
            &[GraphColumn::vertex("n", "n"), GraphColumn::vertex("c", "c")],
            0,
            None,
        )
        .unwrap()
        .with_duplicates();
    let head = "MATCH (n) OPTIONAL MATCH (n)-[:R]->(c) WHERE c.p = TRUE";
    let parsed = PreparedGraphText::prepare(&format!("{head} RETURN n,c"), symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
    assert_eq!(typed, parsed);
    let aggregate = PreparedGraphAggregateText::prepare(
        &format!("{head} RETURN n, COUNT(*) AS rows, COUNT(c) AS present GROUP BY n"),
        symbols,
    )
    .unwrap()
    .bind_parameters(&GqlParameters::new())
    .unwrap();
    let values = [Some(CanonicalScalar::Bool(true)), None];
    let rows = aggregate
        .execute_governed(
            3,
            [VId(1), VId(2)],
            [(VId(1), RelationId(1), VId(1))],
            |vid, predicates| {
                Ok::<_, ()>(predicates.iter().all(|p| {
                    p.matches_borrowed([], values[vid.0 as usize - 1].as_ref().map(|v| (P, v)))
                }))
            },
            |vid, _| Ok(values[vid.0 as usize - 1].as_ref()),
            wide(),
            || Ok::<_, ()>(()),
        )
        .unwrap();
    assert_eq!(rows.value.len(), 2);
    assert_eq!(rows.value[0].get(1).unwrap().as_count(), Some(1));
    assert_eq!(rows.value[1].get(0).unwrap().as_count(), Some(1));
    assert_eq!(rows.value[1].get(1).unwrap().as_count(), Some(0));
    let absent = PreparedGraphText::prepare(
        "MATCH (n) WHERE NOT EXISTS { MATCH (n)-[:R]->(c) WHERE c.p = TRUE } RETURN n",
        symbols,
    )
    .unwrap()
    .bind_parameters(&GqlParameters::new())
    .unwrap();
    assert_eq!(
        absent,
        root.prepare_values_with_clauses(
            &[GraphMatchClause::not_exists(&child)],
            &[GraphColumn::vertex("n", "n")],
            0,
            None
        )
        .unwrap()
        .with_duplicates()
    );
}

#[test]
fn malformed_or_oversized_literals_refuse_before_catalog_calls() {
    for statement in [
        "MATCH (n) WHERE n.p = 'unterminated RETURN n",
        "MATCH (n) WHERE n.p IS TRUE RETURN n",
        "MATCH (n) WHERE n.p = TRUE false RETURN n",
        "MATCH (n) WHERE n.p = 'a' 'b' RETURN n",
        "MATCH (n) WHERE n.p = 'x'; RETURN n",
        "MATCH (n) WHERE n.p = 'x' OR RETURN n",
        // Double quotes delimit text (fgdb-285i2); these stay malformed.
        "MATCH (n) WHERE n.p = \"unterminated RETURN n",
        "MATCH (n) WHERE n.p = \"x\"' RETURN n",
        "MATCH (n) /* unclosed WHERE n.p = 'x' RETURN n",
        "MATCH (n) WHERE n.p IS NOT RETURN n",
    ] {
        let mut calls = 0;
        assert!(
            PreparedGraphText::prepare(statement, |kind: GraphSymbolKind, name: &str| {
                calls += 1;
                symbols(kind, name)
            })
            .is_err()
        );
        assert_eq!(calls, 0, "{statement}");
    }
    let statement = format!("MATCH (n) WHERE n.p = '{}' RETURN n", "x".repeat(60_000));
    let mut calls = 0;
    let error = PreparedGraphText::prepare(&statement, |kind: GraphSymbolKind, name: &str| {
        calls += 1;
        symbols(kind, name)
    })
    .unwrap_err();
    assert_eq!(error.kind, GraphPatternTextErrorKind::ScalarLiteral);
    assert_eq!(calls, 0);
}
