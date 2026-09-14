//! Compound text must compile to the same typed set engine, not an interpreter.
use fgdb_delta_types::{LabelId, PropertyKeyId};
use fgdb_gql::algebra::{GraphValueOrder, GraphValueRow};
use fgdb_gql::{
    GqlParameterType, GqlParameters, GqlQueryError, GqlQueryPolicy, GraphPatternTextErrorKind,
    GraphSetBuildError, GraphSetOperation as Op, GraphSetQuantifier as Quant,
    GraphSetTextErrorKind, GraphSymbol, GraphSymbolKind, MAX_GRAPH_SET_DEPTH,
    MAX_GRAPH_SET_OPERANDS, MAX_GRAPH_TEXT_BYTES, PreparedGraphSet, PreparedGraphSetText,
    PreparedGraphText,
};
use fgdb_types::{CanonicalScalar, CanonicalScalarKind, VId};
use std::cell::Cell;
use std::collections::BTreeMap;

const A: &str = "MATCH (a:A) RETURN a.p AS value";
const B: &str = "MATCH (b:B) RETURN b.p AS other";
const C: &str = "MATCH (c:C) RETURN c.p AS third";
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "A") => Some(GraphSymbol::Label(LabelId(1))),
        (GraphSymbolKind::Label, "B") => Some(GraphSymbol::Label(LabelId(2))),
        (GraphSymbolKind::Label, "C") => Some(GraphSymbol::Label(LabelId(3))),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
        _ => None,
    }
}
fn leaf(text: &str) -> PreparedGraphSet {
    PreparedGraphText::prepare(text, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
        .into()
}
fn query(text: &str) -> PreparedGraphSet {
    PreparedGraphSetText::prepare(text, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(10_000, 10_000, 10_000_000, 10_000_000)
}
fn values() -> [Option<CanonicalScalar>; 6] {
    [
        Some(CanonicalScalar::Int(1)),
        Some(CanonicalScalar::Int(1)),
        Some(CanonicalScalar::Null),
        None,
        Some(CanonicalScalar::Int(2)),
        Some(CanonicalScalar::Int(1)),
    ]
}
fn run(query: &PreparedGraphSet) -> Vec<GraphValueRow> {
    let values = values();
    query
        .execute_governed(
            policy(),
            |pattern, remaining| {
                pattern.plan().execute_governed_with_properties(
                    6,
                    (1..=6).map(VId),
                    [],
                    |vid, predicates| {
                        let labels = if vid.0 <= 2 {
                            vec![LabelId(1)]
                        } else if vid.0 <= 4 {
                            vec![LabelId(1), LabelId(2)]
                        } else {
                            vec![LabelId(2), LabelId(3)]
                        };
                        Ok::<_, ()>(predicates.iter().all(|p| p.matches(&labels, &[])))
                    },
                    |vid, _| Ok(values[vid.0 as usize - 1].as_ref()),
                    remaining,
                    || Ok::<_, ()>(()),
                )
            },
            || Ok::<_, ()>(()),
        )
        .unwrap()
        .value
}
fn integers(rows: &[GraphValueRow]) -> Vec<Option<i64>> {
    let mut values: Vec<_> = rows
        .iter()
        .map(|row| match row.get(0).unwrap().as_scalar().unwrap() {
            CanonicalScalar::Null => None,
            CanonicalScalar::Int(n) => Some(*n),
            _ => panic!("unexpected fixture kind"),
        })
        .collect();
    values.sort();
    values
}

#[test]
fn precedence_associativity_and_parentheses_match_the_exact_typed_definition() {
    let (a, b, c) = (leaf(A), leaf(B), leaf(C));
    let cases = [
        (
            format!("{A} UNION {B} INTERSECT ALL {C}"),
            a.clone()
                .combine(
                    Op::Union,
                    Quant::Distinct,
                    b.clone()
                        .combine(Op::Intersect, Quant::All, c.clone())
                        .unwrap(),
                )
                .unwrap(),
        ),
        (
            format!("{A} EXCEPT ALL {B} UNION ALL {C}"),
            a.clone()
                .combine(Op::Except, Quant::All, b.clone())
                .unwrap()
                .combine(Op::Union, Quant::All, c.clone())
                .unwrap(),
        ),
        (
            format!("({A} UNION ALL {B}) INTERSECT {C}"),
            a.clone()
                .combine(Op::Union, Quant::All, b.clone())
                .unwrap()
                .nested()
                .unwrap()
                .combine(Op::Intersect, Quant::Distinct, c.clone())
                .unwrap(),
        ),
        (
            format!("{A} EXCEPT ({B} EXCEPT {C})"),
            a.clone()
                .combine(
                    Op::Except,
                    Quant::Distinct,
                    b.clone()
                        .combine(Op::Except, Quant::Distinct, c)
                        .unwrap()
                        .nested()
                        .unwrap(),
                )
                .unwrap(),
        ),
        (
            format!("{A} UNION DISTINCT {B}"),
            a.combine(Op::Union, Quant::Distinct, b).unwrap(),
        ),
    ];
    for (text, expected) in cases {
        assert_eq!(
            query(&text).canonical_bytes(),
            expected.canonical_bytes(),
            "{text}"
        );
    }
}

#[test]
fn final_pages_cover_the_whole_set_and_nested_ordering_does_not_replace_inner_selection() {
    let text = format!("{A} UNION ALL {B} ORDER BY value DESC NULLS LAST SKIP 1 LIMIT 2");
    let expected = leaf(A)
        .combine(Op::Union, Quant::All, leaf(B))
        .unwrap()
        .with_order_by(&[GraphValueOrder::descending(0)])
        .unwrap()
        .with_page(1, Some(2));
    assert_eq!(query(&text), expected);
    assert_eq!(integers(&run(&query(&text))), vec![Some(1), Some(1)]);
    let text = "(MATCH (n) RETURN n AS x ORDER BY x DESC SKIP 1 LIMIT 3) ORDER BY x ASC LIMIT 1";
    let expected = leaf("MATCH (n) RETURN n AS x")
        .with_order_by(&[GraphValueOrder::descending(0)])
        .unwrap()
        .with_page(1, Some(3))
        .nested()
        .unwrap()
        .with_order_by(&[GraphValueOrder::ascending(0)])
        .unwrap()
        .with_page(0, Some(1));
    assert_eq!(query(text), expected);
    assert_eq!(
        run(&query(text))[0].get(0).unwrap().as_vertex(),
        Some(VId(3))
    );
    let inherited = query("(MATCH (n) RETURN n AS x ORDER BY x DESC LIMIT 3) SKIP 1 LIMIT 1");
    assert_eq!(run(&inherited)[0].get(0).unwrap().as_vertex(), Some(VId(5)));
    let local = query(&format!("({A} LIMIT 1) UNION ALL ({B} LIMIT 1) LIMIT 1"));
    assert_eq!(run(&local).len(), 1);
}

#[test]
fn all_six_set_forms_obey_independent_multiset_arithmetic_including_null() {
    let left = [Some(1), Some(1), None, None];
    let right = [None, None, Some(2), Some(1)];
    for (name, operation) in [
        ("UNION", Op::Union),
        ("INTERSECT", Op::Intersect),
        ("EXCEPT", Op::Except),
    ] {
        for (mode, all) in [("ALL", true), ("DISTINCT", false), ("", false)] {
            let mut expected = Vec::new();
            for value in [None, Some(1), Some(2)] {
                let l = left.iter().filter(|v| **v == value).count();
                let r = right.iter().filter(|v| **v == value).count();
                let (l, r) = if all {
                    (l, r)
                } else {
                    (usize::from(l > 0), usize::from(r > 0))
                };
                let count = match operation {
                    Op::Union if !all => usize::from(l + r > 0),
                    Op::Union => l + r,
                    Op::Intersect => l.min(r),
                    Op::Except => l.saturating_sub(r),
                };
                expected.extend(std::iter::repeat_n(value, count));
            }
            let text = format!("{A} {name} {mode} {B}");
            assert_eq!(integers(&run(&query(&text))), expected, "{text}");
        }
    }
    let tuples = query("MATCH (a:A) RETURN a,a.p AS p INTERSECT MATCH (b:B) RETURN b,b.p AS p");
    let ids: Vec<_> = run(&tuples)
        .iter()
        .map(|r| r.get(0).unwrap().as_vertex().unwrap())
        .collect();
    assert_eq!(
        ids,
        vec![VId(3), VId(4)],
        "null scalar cells do not collapse different vertex cells"
    );
}

#[test]
fn parameters_and_symbols_share_one_contract_without_leaking_arm_variables() {
    let text = "MATCH (a:A) WHERE a.p IN [$x,$x] RETURN a.p AS value UNION ALL \
        MATCH (b:A) WHERE b.p BETWEEN $lo AND $x RETURN b.p AS other LIMIT $page";
    let mut calls = BTreeMap::new();
    let template = PreparedGraphSetText::prepare(text, |kind, name| {
        *calls.entry((kind, name.to_owned())).or_insert(0) += 1;
        symbols(kind, name)
    })
    .unwrap();
    assert_eq!(calls.len(), 2);
    assert!(calls.values().all(|n| *n == 1));
    assert_eq!(template.columns(), &["value"]);
    assert_eq!(
        template
            .parameter_schema()
            .iter()
            .map(|p| (p.name.as_str(), p.occurrences))
            .collect::<Vec<_>>(),
        vec![("x", 3), ("lo", 1), ("page", 1)]
    );
    let args = GqlParameters::new()
        .with_int64("x", 1)
        .unwrap()
        .with_int64("lo", i64::MIN)
        .unwrap()
        .with_uint64("page", 10)
        .unwrap();
    let bound = template.bind_parameters(&args).unwrap();
    let frozen = bound.canonical_bytes();
    assert_eq!(
        template.bind_parameters(&args).unwrap().canonical_bytes(),
        frozen
    );
    assert_eq!(integers(&run(&bound)), vec![Some(1); 4]);
    assert!(calls.values().all(|n| *n == 1));
    assert!(matches!(
        template
            .bind_parameters(&args.clone().with_int64("extra", 2).unwrap())
            .unwrap_err()
            .kind,
        GraphSetTextErrorKind::Pattern(GraphPatternTextErrorKind::UnexpectedArguments)
    ));
    let calls = Cell::new(0);
    let error = PreparedGraphSetText::prepare(
        "MATCH (a:A) WHERE a.p=$x RETURN a UNION MATCH (b:A) RETURN b LIMIT $x",
        |kind, name| {
            calls.set(calls.get() + 1);
            symbols(kind, name)
        },
    )
    .unwrap_err();
    assert!(matches!(
        error.kind,
        GraphSetTextErrorKind::Pattern(GraphPatternTextErrorKind::ConflictingParameterTypes)
    ));
    assert_eq!(calls.get(), 0);
}

#[test]
fn scalar_declarations_and_keyword_looking_literals_are_never_interpolated() {
    let text =
        "MATCH (a:A) WHERE a.p=$needle RETURN a.p AS value UNION MATCH (b:B) RETURN b.p AS other";
    let template = PreparedGraphSetText::prepare_with_parameter_types(
        text,
        &[(
            "needle",
            GqlParameterType::Scalar(CanonicalScalarKind::Text),
        )],
        symbols,
    )
    .unwrap();
    let payload = "x' UNION MATCH (secret) RETURN secret LIMIT 0";
    let args = GqlParameters::new().with_text("needle", payload).unwrap();
    let expected = PreparedGraphText::prepare_with_parameter_types(
        "MATCH (a:A) WHERE a.p=$needle RETURN a.p AS value",
        &[(
            "needle",
            GqlParameterType::Scalar(CanonicalScalarKind::Text),
        )],
        symbols,
    )
    .unwrap()
    .bind_parameters(&args)
    .unwrap();
    let expected = PreparedGraphSet::from(expected)
        .combine(Op::Union, Quant::Distinct, leaf(B))
        .unwrap();
    assert_eq!(template.bind_parameters(&args).unwrap(), expected);
    assert_eq!(template.statement(), text);
    assert!(!format!("{template:?}").contains("needle"));
    template
        .bind_parameters(&GqlParameters::new().with_null("needle").unwrap())
        .unwrap();
    assert!(matches!(
        template
            .bind_parameters(&GqlParameters::new().with_int64("needle", 1).unwrap())
            .unwrap_err()
            .kind,
        GraphSetTextErrorKind::Pattern(GraphPatternTextErrorKind::ParameterTypeMismatch { .. })
    ));
    let quoted = "MATCH (union:A) WHERE union.p IN ['x''] UNION MATCH (bad) RETURN bad', 'EXCEPT'] \
        RETURN union.p AS union UNION MATCH (intersect:B) RETURN intersect.p AS except ORDER BY union";
    assert_eq!(
        PreparedGraphSetText::prepare(quoted, symbols)
            .unwrap()
            .columns(),
        &["union"]
    );
}

#[test]
fn malformed_later_arms_schema_mismatches_and_bad_global_orders_do_not_touch_catalog() {
    for tail in [
        "UNION",
        "UNION ALL ALL MATCH (b) RETURN b.p",
        "UNION MATCH (b) RETURN missing",
        "UNION MATCH (b) RETURN b",
        "UNION MATCH (b) RETURN b.p,b AS extra",
        "UNION (MATCH (b) RETURN b.p",
        "UNION MATCH (b) WHERE b.p IN [1,] RETURN b.p",
        "UNION MATCH (b) RETURN b.p ORDER BY missing",
        "UNION MATCH (b) RETURN b.p ORDER BY value,value",
        "UNION MATCH (b) RETURN b.p LIMIT -1",
        "UNION MATCH (b) RETURN b.p LIMIT 18446744073709551616",
        "LIMIT 1 UNION MATCH (b) RETURN b.p",
        ";",
        "UNION MATCH (b) RETURN COUNT(*) AS value",
    ] {
        let calls = Cell::new(0);
        assert!(
            PreparedGraphSetText::prepare(&format!("{A} {tail}"), |kind, name| {
                calls.set(calls.get() + 1);
                symbols(kind, name)
            })
            .is_err(),
            "{tail}"
        );
        assert_eq!(calls.get(), 0, "{tail}");
    }
    let calls = Cell::new(0);
    assert!(
        PreparedGraphSetText::prepare_with_parameter_types(
            A,
            &[("unused", GqlParameterType::Int64)],
            |kind, name| {
                calls.set(calls.get() + 1);
                symbols(kind, name)
            }
        )
        .is_err()
    );
    assert_eq!(calls.get(), 0);
}

#[test]
fn errors_report_original_utf8_byte_offsets_and_redact_definitions() {
    let text = "\u{2003}MATCH (a:A) WHERE a.p='λ' RETURN a.p AS value UNION \
        MATCH (private_name:B) WHERE private_name.p=$secret_arg RETURN private_name.p";
    let template = PreparedGraphSetText::prepare(text, symbols).unwrap();
    let error = template.bind_parameters(&GqlParameters::new()).unwrap_err();
    assert_eq!(error.offset, text.find("$secret_arg").unwrap());
    assert!(!format!("{error:?} {error} {template:?}").contains("secret_arg"));
    let bad =
        "\u{2003}MATCH (a:A) RETURN a UNION MATCH (private_name:SecretLabel) RETURN private_name";
    let error = PreparedGraphSetText::prepare(bad, symbols).unwrap_err();
    assert_eq!(error.offset, bad.find("SecretLabel").unwrap());
    assert!(!format!("{error:?} {error}").contains("SecretLabel"));
    let bad = "MATCH (a:A) RETURN a UNION MATCH (b:B) RETURN a";
    let error = PreparedGraphSetText::prepare(bad, symbols).unwrap_err();
    assert_eq!(error.offset, bad.len() - 1);
}

#[test]
fn statement_wide_limits_and_utf8_prefixes_cannot_be_reset_per_arm() {
    assert!(matches!(
        PreparedGraphSetText::prepare(&" ".repeat(MAX_GRAPH_TEXT_BYTES + 1), symbols)
            .unwrap_err()
            .kind,
        GraphSetTextErrorKind::Pattern(GraphPatternTextErrorKind::DefinitionTooLarge)
    ));
    let leaf = "MATCH (n) RETURN n";
    let longest = std::iter::repeat_n(leaf, MAX_GRAPH_SET_OPERANDS)
        .collect::<Vec<_>>()
        .join(" UNION ALL ");
    assert_eq!(query(&longest).operand_count(), MAX_GRAPH_SET_OPERANDS);
    assert!(matches!(
        PreparedGraphSetText::prepare(&format!("{longest} UNION {leaf}"), symbols)
            .unwrap_err()
            .kind,
        GraphSetTextErrorKind::SetBuild(GraphSetBuildError::TooManyOperands { .. })
    ));
    let nested = format!(
        "{}{leaf}{}",
        "(".repeat(MAX_GRAPH_SET_DEPTH - 1),
        ")".repeat(MAX_GRAPH_SET_DEPTH - 1)
    );
    query(&nested);
    assert!(PreparedGraphSetText::prepare(&format!("({nested})"), symbols).is_err());
    let arm = format!("MATCH {}(n) RETURN n", "(n),".repeat(1100));
    assert!(PreparedGraphText::prepare(&arm, symbols).is_ok());
    let error = PreparedGraphSetText::prepare(&format!("{arm} UNION {arm}"), symbols).unwrap_err();
    assert!(matches!(
        error.kind,
        GraphSetTextErrorKind::Pattern(GraphPatternTextErrorKind::TooManyTokens)
    ));
    let source = "\u{2003}(MATCH (n:A) WHERE n.p='λ''β' RETURN n.p AS value) UNION MATCH (m:B) RETURN m.p ORDER BY value";
    for end in (0..=source.len()).filter(|end| source.is_char_boundary(*end)) {
        let _ = PreparedGraphSetText::prepare(&source[..end], symbols);
    }
}

#[test]
fn compiled_compounds_share_exact_budgets_and_every_cancellation_checkpoint() {
    let query = query(
        "(MATCH (n) RETURN n AS x ORDER BY x DESC LIMIT 3) \
        EXCEPT ALL (MATCH (m) RETURN m AS y LIMIT 1) ORDER BY x LIMIT 2",
    );
    let seen = Cell::new(0_usize);
    let execute = |policy, stop| {
        seen.set(0);
        let checkpoint = || {
            seen.set(seen.get() + 1);
            if seen.get() == stop {
                Err(stop)
            } else {
                Ok(())
            }
        };
        query.execute_governed(
            policy,
            |pattern, remaining| {
                pattern.plan().execute_governed_with_properties(
                    5,
                    (1..=5).map(VId),
                    [],
                    |_, _| Ok::<_, ()>(true),
                    |_, _| Ok(None),
                    remaining,
                    checkpoint,
                )
            },
            checkpoint,
        )
    };
    let measured = execute(policy(), 0).unwrap();
    let checkpoints = seen.get();
    assert_eq!(measured.rows.snapshot_records, 10);
    assert_eq!(measured.rows.result_rows, 2);
    let exact = GqlQueryPolicy::new(
        10,
        2,
        measured.evaluator.work_units,
        measured.evaluator.scratch_entries,
    );
    assert_eq!(execute(exact, 0).unwrap(), measured);
    for stop in 1..=checkpoints {
        assert!(
            matches!(execute(policy(), stop), Err(GqlQueryError::Interrupted(at)) if at == stop)
        );
        assert_eq!(seen.get(), stop);
    }
    for refused in [
        GqlQueryPolicy::new(9, 2, u64::MAX, u64::MAX),
        GqlQueryPolicy::new(10, 1, u64::MAX, u64::MAX),
        GqlQueryPolicy::new(10, 2, measured.evaluator.work_units - 1, u64::MAX),
        GqlQueryPolicy::new(10, 2, u64::MAX, measured.evaluator.scratch_entries - 1),
    ] {
        assert!(execute(refused, 0).is_err());
    }
}
