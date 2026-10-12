//! Computed compound ordering uses the native set/projection engine.
use fgdb_delta_types::{LabelId, PropertyKeyId};
use fgdb_gql::algebra::{GraphValueOrder, GraphValueRow};
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GraphSetBuildError, GraphSetOperation as Op,
    GraphSetQuantifier as Quant, GraphSetTextErrorKind, GraphSymbol, GraphSymbolKind,
    MAX_GRAPH_SET_DEPTH, PreparedGraphSet, PreparedGraphSetText, PreparedGraphText,
};
use fgdb_types::CanonicalScalar;
use std::cell::Cell;

const A: &str = "MATCH (a:A) RETURN a.p AS value";
const B: &str = "MATCH (b:B) RETURN b.p AS value";

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "A") => Some(GraphSymbol::Label(LabelId(1))),
        (GraphSymbolKind::Label, "B") => Some(GraphSymbol::Label(LabelId(2))),
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
#[cfg(test)]
mod compound_order_expression_tests {
    use super::*;
    use fgdb_gql::GqlParameterValue;

    fn free_rows(plan: &PreparedGraphSet) -> Vec<GraphValueRow> {
        plan.execute_governed(
            policy(),
            |_, _| Err::<_, GqlQueryError<usize, usize>>(GqlQueryError::Source(1)),
            || Ok::<_, usize>(()),
        )
        .expect("a source-free compound must not request a graph source")
        .value
    }

    fn ordered_ints(text: &str) -> Vec<Option<i64>> {
        free_rows(&query(text))
            .iter()
            .map(|row| match row.get(0).unwrap().as_scalar().unwrap() {
                CanonicalScalar::Null => None,
                CanonicalScalar::Int(value) => Some(*value),
                _ => panic!("unexpected fixture kind"),
            })
            .collect()
    }

    #[test]
    fn expression_pages_rank_complete_set_bags_without_changing_multiplicity() {
        for (operator, right, expected) in [
            ("UNION ALL", "[2, 3]", vec![4, 4, 3, 2, 1]),
            ("UNION", "[2, 3]", vec![4, 3, 2, 1]),
            ("INTERSECT ALL", "[4, 4, 2]", vec![4, 4]),
            ("INTERSECT", "[4, 4, 2]", vec![4]),
            ("EXCEPT ALL", "[4]", vec![4, 1]),
            ("EXCEPT", "[4]", vec![1]),
        ] {
            let text = format!(
                "UNWIND [4, 1, 4] AS x RETURN x {operator} \
                 UNWIND {right} AS x RETURN x ORDER BY -x"
            );
            assert_eq!(
                ordered_ints(&text),
                expected.into_iter().map(Some).collect::<Vec<_>>(),
                "{text}"
            );
        }
    }

    #[test]
    fn output_names_are_aligned_before_computed_ranking() {
        let plan = query(
            "RETURN 9 AS b, 1 AS a UNION ALL RETURN 2 AS a, 0 AS b \
             ORDER BY a * 10 + b DESC",
        );
        assert_eq!(plan.columns(), &["b", "a"]);
        let rows = free_rows(&plan);
        let expected = [[0, 2], [9, 1]];
        assert_eq!(rows.len(), expected.len());
        for (row, expected) in rows.iter().zip(expected) {
            for (column, value) in expected.into_iter().enumerate() {
                assert_eq!(
                    row.get(column).unwrap().as_scalar(),
                    Some(&CanonicalScalar::Int(value))
                );
            }
            assert!(row.get(2).is_none(), "private sort key escaped");
        }
    }

    #[test]
    fn local_and_outer_expression_pages_keep_distinct_scope_boundaries() {
        assert_eq!(
            ordered_ints(
                "(RETURN 1 AS x UNION ALL RETURN 4 AS x ORDER BY -x LIMIT 1) \
                 UNION ALL \
                 (RETURN 2 AS x UNION ALL RETURN 3 AS x ORDER BY -x LIMIT 1) \
                 ORDER BY x + 0"
            ),
            vec![Some(3), Some(4)]
        );
        assert_eq!(
            ordered_ints("(RETURN 1 AS x UNION ALL RETURN 4 AS x) ORDER BY -x"),
            vec![Some(4), Some(1)]
        );
        assert_eq!(
            ordered_ints(
                "RETURN 1 AS x UNION ALL RETURN 4 AS x UNION ALL RETURN 3 AS x \
                 ORDER BY -x SKIP 1 LIMIT 1"
            ),
            vec![Some(3)]
        );
    }

    #[test]
    fn case_null_placement_and_semantic_ties_keep_only_public_columns() {
        let prefix = "RETURN null AS x UNION ALL RETURN 2 AS x UNION ALL RETURN 1 AS x";
        assert_eq!(
            ordered_ints(&format!("{prefix} ORDER BY x + 0 DESC NULLS FIRST")),
            vec![None, Some(2), Some(1)]
        );
        assert_eq!(
            ordered_ints(&format!("{prefix} ORDER BY x + 0 ASC NULLS LAST")),
            vec![Some(1), Some(2), None]
        );
        assert_eq!(
            ordered_ints(
                "RETURN 1 AS x UNION ALL RETURN 3 AS x UNION ALL RETURN 2 AS x \
                 ORDER BY CASE WHEN x = 2 THEN 0 ELSE 1 END, x DESC"
            ),
            vec![Some(2), Some(3), Some(1)]
        );
        let plan = query(
            "RETURN 1 AS __fg_order_1 UNION ALL RETURN 3 AS __fg_order_1 \
             ORDER BY -__fg_order_1",
        );
        assert_eq!(plan.columns(), &["__fg_order_1"]);
        assert!(free_rows(&plan).iter().all(|row| row.get(1).is_none()));
    }

    #[test]
    fn parameters_are_shared_by_name_but_keep_local_expression_ordinals() {
        let text = "RETURN $bias AS value UNION ALL RETURN 5 AS value \
                    ORDER BY value * $direction + $bias SKIP $skip LIMIT $limit";
        let prepared = PreparedGraphSetText::prepare(text, symbols).unwrap();
        let template = prepared.canonical_template_bytes();
        let bias = prepared
            .parameter_schema()
            .iter()
            .find(|parameter| parameter.name == "bias")
            .unwrap();
        assert_eq!(bias.occurrences, 2);
        let arguments = |direction| {
            let mut parameters = GqlParameters::new();
            for (name, value) in [
                ("bias", GqlParameterValue::Int64(2)),
                ("direction", GqlParameterValue::Int64(direction)),
                ("skip", GqlParameterValue::UInt64(0)),
                ("limit", GqlParameterValue::UInt64(2)),
            ] {
                parameters.insert(name.to_owned(), value).unwrap();
            }
            parameters
        };
        let descending = prepared.bind_parameters(&arguments(-1)).unwrap();
        let ascending = prepared.bind_parameters(&arguments(1)).unwrap();
        for (plan, expected) in [(&descending, [5, 2]), (&ascending, [2, 5])] {
            let rows = free_rows(plan);
            assert_eq!(rows.len(), 2);
            for (row, value) in rows.iter().zip(expected) {
                assert_eq!(
                    row.get(0).unwrap().as_scalar(),
                    Some(&CanonicalScalar::Int(value))
                );
            }
        }
        assert_ne!(descending.canonical_bytes(), ascending.canonical_bytes());
        assert_eq!(prepared.canonical_template_bytes(), template);
        let renamed =
            PreparedGraphSetText::prepare(&text.replace("$direction", "$factor"), symbols).unwrap();
        assert_ne!(renamed.canonical_template_bytes(), template);
    }

    #[test]
    fn expression_identity_ignores_trivia_but_not_operators_or_parameter_bindings() {
        let plain = "RETURN 1 AS x UNION ALL RETURN 2 AS x ORDER BY x + $weight";
        let decorated = "RETURN 1 AS x UNION ALL RETURN 2 AS x \
                         ORDER /* a */ BY x /* b */ + /* c */ $weight";
        let prepare = |text: &str| PreparedGraphSetText::prepare(text, symbols).unwrap();
        assert_eq!(
            prepare(plain).canonical_template_bytes(),
            prepare(decorated).canonical_template_bytes()
        );
        assert_ne!(
            prepare(plain).canonical_template_bytes(),
            prepare(&plain.replace("x +", "x *")).canonical_template_bytes()
        );
        // Both global parameter tables stay [a, b]. Each page's sole local
        // parameter has ordinal zero, but it refers to a DIFFERENT argument.
        // Merely recording the global table cannot distinguish these plans.
        let first = prepare(
            "RETURN $a AS x, $b AS y UNION ALL RETURN $b AS x, $a AS y \
             ORDER BY x * $a",
        );
        let second = prepare(
            "RETURN $a AS x, $b AS y UNION ALL RETURN $b AS x, $a AS y \
             ORDER BY x * $b",
        );
        let names = |prepared: &PreparedGraphSetText| {
            prepared
                .parameter_schema()
                .iter()
                .map(|spec| (spec.name.clone(), spec.parameter_type))
                .collect::<Vec<_>>()
        };
        assert_eq!(names(&first), names(&second));
        assert_ne!(
            first.canonical_template_bytes(),
            second.canonical_template_bytes()
        );
    }

    #[test]
    fn invalid_output_scope_delimiters_and_parameter_contracts_refuse_before_catalog() {
        let head = "MATCH (n:A) RETURN n.p AS value \
                    UNION ALL MATCH (m:B) RETURN m.p AS value";
        for tail in [
            "ORDER BY missing + 1",
            "ORDER BY n.p + 1",
            "ORDER BY (value + 1]",
            "ORDER BY [value + 1)",
            "ORDER BY value + $p LIMIT $p",
        ] {
            let calls = Cell::new(0);
            let result = PreparedGraphSetText::prepare(&format!("{head} {tail}"), |kind, name| {
                calls.set(calls.get() + 1);
                symbols(kind, name)
            });
            assert!(result.is_err(), "{tail}");
            assert_eq!(calls.get(), 0, "{tail}");
        }
        let text = format!("{head} ORDER BY value + missing");
        let error = PreparedGraphSetText::prepare(&text, symbols).unwrap_err();
        assert_eq!(error.offset, text.find("missing").unwrap());
        // A repeated sort key is legal and redundant, as in openCypher.
        PreparedGraphSetText::prepare(&format!("{head} ORDER BY value + 1, value + 1"), symbols)
            .unwrap();
    }

    #[test]
    fn zero_limit_does_not_hide_sort_expression_errors() {
        let bad = query(
            "UNWIND [1, 2] AS x RETURN x UNION ALL RETURN 0 AS x \
             ORDER BY 10 / x LIMIT 0",
        );
        let result = bad.execute_governed(
            policy(),
            |_, _| Err::<_, GqlQueryError<usize, usize>>(GqlQueryError::Source(1)),
            || Ok::<_, usize>(()),
        );
        assert!(
            matches!(
                result,
                Err(GqlQueryError::Source(
                    fgdb_gql::GraphSetExecutionError::Projection { .. }
                ))
            ),
            "LIMIT 0 hid the failing sort expression"
        );
        assert!(
            free_rows(&query(
                "UNWIND [1, 2] AS x RETURN x UNION ALL RETURN 3 AS x \
                 ORDER BY 10 / x LIMIT 0"
            ))
            .is_empty()
        );
    }

    #[test]
    fn compound_ordering_keeps_cancellation_and_work_admission() {
        let plan = query(
            "UNWIND [1, 2, 3] AS x RETURN x UNION ALL RETURN 4 AS x \
             ORDER BY -x SKIP 1 LIMIT 2",
        );
        let calls = Cell::new(0);
        let baseline = plan
            .execute_governed(
                policy(),
                |_, _| Err::<_, GqlQueryError<usize, usize>>(GqlQueryError::Source(1)),
                || {
                    calls.set(calls.get() + 1);
                    Ok::<_, usize>(())
                },
            )
            .unwrap();
        assert_eq!(baseline.value.len(), 2);
        let total = calls.get();
        assert!(total > 3);
        for cut in [1, total / 2, total] {
            calls.set(0);
            let result = plan.execute_governed(
                policy(),
                |_, _| Err::<_, GqlQueryError<usize, usize>>(GqlQueryError::Source(1)),
                || {
                    calls.set(calls.get() + 1);
                    if calls.get() == cut { Err(cut) } else { Ok(()) }
                },
            );
            assert!(matches!(result, Err(GqlQueryError::Interrupted(at)) if at == cut));
        }
        let result = plan.execute_governed(
            GqlQueryPolicy::new(10_000, 10_000, 0, 0),
            |_, _| Err::<_, GqlQueryError<usize, usize>>(GqlQueryError::Source(1)),
            || Ok::<_, usize>(()),
        );
        assert!(
            result.is_err(),
            "expression page refreshed exhausted admission"
        );
    }

    #[test]
    fn existing_column_only_native_definition_is_unchanged() {
        let text = format!("{A} UNION ALL {B} ORDER BY value DESC SKIP 1 LIMIT 2");
        let expected = leaf(A)
            .combine(Op::Union, Quant::All, leaf(B))
            .unwrap()
            .with_order_by(&[GraphValueOrder::descending(0)])
            .unwrap()
            .with_page(1, Some(2));
        assert_eq!(query(&text).canonical_bytes(), expected.canonical_bytes());
    }

    #[test]
    fn nested_expression_pages_charge_native_depth_before_catalog() {
        let mut text = format!("{A} UNION ALL {B}");
        for layers in 0..MAX_GRAPH_SET_DEPTH {
            let calls = Cell::new(0);
            let result = PreparedGraphSetText::prepare(&text, |kind, name| {
                calls.set(calls.get() + 1);
                symbols(kind, name)
            });
            if let Err(error) = result {
                assert!(matches!(
                    error.kind,
                    GraphSetTextErrorKind::SetBuild(GraphSetBuildError::TooDeep { .. })
                ));
                assert_eq!(calls.get(), 0);
                // Each layer owns a scope and TWO native projections. A
                // parser-only nesting count would incorrectly admit twice
                // this many layers and then fail during native binding.
                assert!(layers < MAX_GRAPH_SET_DEPTH / 2);
                return;
            }
            text = format!("({text}) ORDER BY value + 1");
        }
        panic!("computed page projections were omitted from depth admission");
    }
}
