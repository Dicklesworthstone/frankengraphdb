//! Ordered, heterogeneous list values retain element identities and boundaries.
use fgdb_gql::algebra::GraphValue;
use fgdb_types::CanonicalScalar;

fn int(value: i64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(value))
}

#[test]
fn nested_list_encoding_preserves_order_nulls_and_boundaries() {
    let list = GraphValue::List(
        vec![
            int(7),
            GraphValue::Scalar(CanonicalScalar::Null),
            GraphValue::List(vec![int(2), int(3)].into_boxed_slice()),
        ]
        .into_boxed_slice(),
    );
    assert_eq!(list.as_list().unwrap()[0], int(7));
    assert!(list.as_list().unwrap()[1].is_null());
    assert_eq!(
        list.canonical_bytes().unwrap(),
        list.clone().canonical_bytes().unwrap()
    );
    let reordered = GraphValue::List(
        vec![
            GraphValue::Scalar(CanonicalScalar::Null),
            int(7),
            GraphValue::List(vec![int(2), int(3)].into_boxed_slice()),
        ]
        .into_boxed_slice(),
    );
    let flattened = GraphValue::List(
        vec![
            int(7),
            GraphValue::Scalar(CanonicalScalar::Null),
            int(2),
            int(3),
        ]
        .into_boxed_slice(),
    );
    assert_ne!(
        list.canonical_bytes().unwrap(),
        reordered.canonical_bytes().unwrap()
    );
    assert_ne!(
        list.canonical_bytes().unwrap(),
        flattened.canonical_bytes().unwrap()
    );
    assert_ne!(
        GraphValue::List(Box::new([])).canonical_bytes().unwrap(),
        GraphValue::Scalar(CanonicalScalar::Null)
            .canonical_bytes()
            .unwrap()
    );
}

#[test]
fn list_parameters_retain_type_and_reject_scalar_binding() {
    use fgdb_gql::{GqlParameterType, GqlParameters, PreparedGraphSetText};
    let template = PreparedGraphSetText::prepare_with_parameter_types(
        "MATCH (n) RETURN $xs AS xs",
        &[("xs", GqlParameterType::List)],
        |_, _| None,
    )
    .unwrap();
    assert_eq!(
        template.parameter_schema()[0].parameter_type,
        GqlParameterType::List
    );
    let arguments = GqlParameters::new()
        .with_list(
            "xs",
            vec![int(1), GraphValue::List(vec![int(2)].into_boxed_slice())],
        )
        .unwrap();
    let bound = template.bind_parameters(&arguments).unwrap();
    let other = GqlParameters::new()
        .with_list("xs", vec![int(1), int(2)])
        .unwrap();
    assert_ne!(
        bound.canonical_bytes(),
        template.bind_parameters(&other).unwrap().canonical_bytes()
    );
    assert_ne!(arguments.canonical_bytes(), other.canonical_bytes());
    assert!(
        template
            .bind_parameters(&GqlParameters::new().with_int64("xs", 1).unwrap())
            .is_err()
    );
}

#[test]
fn map_parameter_templates_keep_map_types_and_nullable_bindings() {
    use fgdb_gql::{GqlParameterType, GqlParameters, GraphSetColumnType, PreparedGraphSetText};
    let plain = |kind| {
        PreparedGraphSetText::prepare_with_parameter_types(
            "RETURN $m AS value",
            &[("m", kind)],
            |_, _| None,
        )
        .unwrap()
    };
    let map = plain(GqlParameterType::Map);
    let list = plain(GqlParameterType::List);
    assert_eq!(
        map.parameter_schema()[0].parameter_type,
        GqlParameterType::Map
    );
    assert_eq!(map.column_types(), [GraphSetColumnType::Any]);
    assert_ne!(
        map.canonical_template_bytes(),
        list.canonical_template_bytes()
    );
    let template = PreparedGraphSetText::prepare_with_parameter_types(
        "RETURN $m.nested.key AS value, keys($m) AS names",
        &[("m", GqlParameterType::Map)],
        |_, _| None,
    )
    .unwrap();
    let before = template.canonical_template_bytes();
    let first = GqlParameters::new()
        .with_map(
            "m",
            vec![(
                "nested".into(),
                GraphValue::map(vec![("key".into(), int(1))]).unwrap(),
            )],
        )
        .unwrap();
    let second = GqlParameters::new()
        .with_map(
            "m",
            vec![(
                "nested".into(),
                GraphValue::map(vec![("key".into(), int(2))]).unwrap(),
            )],
        )
        .unwrap();
    assert_ne!(
        template.bind_parameters(&first).unwrap().canonical_bytes(),
        template.bind_parameters(&second).unwrap().canonical_bytes()
    );
    assert!(
        template
            .bind_parameters(&GqlParameters::new().with_null("m").unwrap())
            .is_ok()
    );
    for arguments in [
        GqlParameters::new().with_int64("m", 1).unwrap(),
        GqlParameters::new().with_text("m", "map").unwrap(),
        GqlParameters::new().with_list("m", vec![]).unwrap(),
    ] {
        assert!(template.bind_parameters(&arguments).is_err());
    }
    assert_eq!(template.canonical_template_bytes(), before);
}

fn execute(
    text: &str,
    policy: fgdb_gql::GqlQueryPolicy,
) -> Result<
    fgdb_gql::GqlQueryExecution<fgdb_gql::algebra::GraphValueRow>,
    fgdb_gql::GqlQueryError<fgdb_gql::GraphSetExecutionError<()>, ()>,
> {
    let prepared = fgdb_gql::PreparedGraphSetText::prepare(text, |_, _| None)
        .unwrap()
        .bind_parameters(&fgdb_gql::GqlParameters::new())
        .unwrap();
    prepared.execute_governed(
        policy,
        |pattern, budget| {
            pattern.plan().execute_governed_with_properties(
                1,
                [fgdb_types::VId(1)],
                [],
                |_, _| Ok::<_, ()>(true),
                |_, _| Ok(None),
                budget,
                || Ok::<_, ()>(()),
            )
        },
        || Ok::<_, ()>(()),
    )
}

#[test]
fn list_growth_and_unwind_fanout_cannot_hide_behind_limit_zero() {
    use fgdb_gql::{GlaLimitDimension, GqlQueryError, GqlQueryPolicy};
    let wide = (0..256)
        .map(|n| n.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let ample = GqlQueryPolicy::new(100, 1000, 1_000_000, 1_000_000);
    let small_list = "MATCH (n) RETURN [] AS xs LIMIT 0";
    let large_list = format!("MATCH (n) RETURN [{wide}] AS xs LIMIT 0");
    let baseline = execute(small_list, ample)
        .unwrap()
        .evaluator
        .scratch_entries;
    let tight = GqlQueryPolicy::new(100, 1000, 1_000_000, baseline + 32);
    assert!(execute(small_list, tight).unwrap().value.is_empty());
    assert!(
        matches!(execute(&large_list, tight), Err(GqlQueryError::Evaluator(error))
        if error.dimension == GlaLimitDimension::ScratchEntries)
    );
    let small_unwind = "MATCH (n) WITH [1] AS xs UNWIND xs AS x RETURN x LIMIT 0";
    let large_unwind = format!("MATCH (n) WITH [{wide}] AS xs UNWIND xs AS x RETURN x LIMIT 0");
    let construction = format!("MATCH (n) RETURN [{wide}] AS xs LIMIT 0");
    let construction_cost = execute(&construction, ample)
        .unwrap()
        .evaluator
        .scratch_entries;
    let tight = GqlQueryPolicy::new(100, 1000, 1_000_000, construction_cost + 64);
    assert!(execute(&construction, tight).unwrap().value.is_empty());
    assert!(execute(small_unwind, tight).unwrap().value.is_empty());
    assert!(
        matches!(execute(&large_unwind, tight), Err(GqlQueryError::Evaluator(error))
        if error.dimension == GlaLimitDimension::ScratchEntries)
    );
}

/// fgdb-2jw3z: a map's identity is its canonical (sorted) entries. Maps
/// never share an encoding with lists or with a map that splits or fuses
/// keys differently, and malformed shapes are refused.
#[test]
fn map_encoding_is_canonical_and_never_collides() {
    let map = |entries: Vec<(&str, GraphValue)>| {
        GraphValue::map(entries.into_iter().map(|(k, v)| (k.into(), v)).collect()).unwrap()
    };
    let ab = map(vec![("a", int(1)), ("b", int(2))]);
    let ba = map(vec![("b", int(2)), ("a", int(1))]);
    assert_eq!(ab, ba);
    assert_eq!(ab.canonical_bytes().unwrap(), ba.canonical_bytes().unwrap());
    let listed = GraphValue::List(vec![int(1), int(2)].into_boxed_slice());
    let values = [
        ab.clone(),
        map(vec![("ab", int(1))]),
        map(vec![("a", int(12))]),
        map(vec![("a", listed.clone())]),
        listed,
        map(vec![]),
        GraphValue::List(Box::new([])),
    ];
    let encoded: Vec<_> = values
        .iter()
        .map(|v| v.canonical_bytes().unwrap())
        .collect();
    for i in 0..encoded.len() {
        for j in i + 1..encoded.len() {
            assert_ne!(encoded[i], encoded[j], "{i} vs {j}");
        }
    }
    assert!(GraphValue::map(vec![("a".into(), int(1)), ("a".into(), int(2))]).is_none());
    let unsorted = GraphValue::Map {
        keys: vec!["b".into(), "a".into()].into_boxed_slice(),
        values: vec![int(1), int(2)].into_boxed_slice(),
    };
    assert!(!unsorted.validate_bounds());
    let nested = map(vec![("a", map(vec![("z", int(3))])), ("b", int(2))]);
    let copied = nested.copy_with_control(&mut |_| Ok::<(), ()>(())).unwrap();
    assert_eq!(copied, nested);
    assert_eq!(
        copied.canonical_bytes().unwrap(),
        nested.canonical_bytes().unwrap()
    );
}

/// Metadata is a source read over an element VALUE, not a graph-binding-only
/// projection. These identities deliberately share low halves; expectations
/// are literal source records and never computed by the value evaluator.
mod element_metadata_values {
    use super::*;
    use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId};
    use fgdb_gql::algebra::{GraphPath, GraphValueRow, PreparedGraphPattern};
    use fgdb_gql::{
        GlaExecutionEvent, GqlParameterType, GqlParameters, GqlQueryError, GqlQueryExecution,
        GqlQueryPolicy, GraphElementMetadata, GraphElementMetadataError, GraphSetExecutionError,
        GraphSymbol, GraphSymbolKind, GraphSymbolResolver, PreparedGraphPipelineAggregateText,
        PreparedGraphSet, PreparedGraphSetText, ReverseSymbolCatalog, with_element_metadata,
    };
    use fgdb_types::{EId, VId};
    use std::cell::Cell;

    const A: VId = VId((1_u128 << 104) | 7);
    const B: VId = VId((2_u128 << 104) | 7);
    const R: EId = EId((3_u128 << 104) | 7);

    #[derive(Clone)]
    struct Names(ReverseSymbolCatalog);

    impl GraphSymbolResolver for Names {
        fn resolve_symbol(&mut self, _: GraphSymbolKind, _: &str) -> Option<GraphSymbol> {
            None
        }
        fn reverse_catalog(&self) -> Option<ReverseSymbolCatalog> {
            Some(self.0.clone())
        }
    }

    fn names() -> Names {
        let mut catalog = ReverseSymbolCatalog::new();
        catalog.insert_label(LabelId(1), "Person");
        catalog.insert_label(LabelId(2), "Place");
        catalog.insert_relation(RelationId(1), "VISITED");
        catalog.insert_property(PropertyKeyId(1), "name");
        Names(catalog)
    }

    fn text(value: &str) -> GraphValue {
        GraphValue::Scalar(CanonicalScalar::ucs_basic_text(value).unwrap())
    }
    fn list(values: Vec<GraphValue>) -> GraphValue {
        GraphValue::List(values.into_boxed_slice())
    }
    fn map(value: &str) -> GraphValue {
        GraphValue::map(vec![("name".into(), text(value))]).unwrap()
    }
    fn null() -> GraphValue {
        GraphValue::Scalar(CanonicalScalar::Null)
    }
    fn policy() -> GqlQueryPolicy {
        GqlQueryPolicy::new(1000, 1000, 1_000_000, 1_000_000)
    }
    fn no_pattern(
        _: &PreparedGraphPattern<GraphValueRow>,
        _: GqlQueryPolicy,
    ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<&'static str, usize>> {
        panic!("value-only metadata opens no graph pattern")
    }
    fn metadata(
        element: ElementId,
        kind: GraphElementMetadata,
        catalog: &ReverseSymbolCatalog,
        control: &mut dyn FnMut(GlaExecutionEvent) -> Result<(), ()>,
    ) -> Result<Option<GraphValue>, GraphElementMetadataError<&'static str>> {
        control(GlaExecutionEvent::Work).map_err(|()| GraphElementMetadataError::Interrupted)?;
        let value = match (element, kind) {
            (ElementId::Vertex(A), GraphElementMetadata::Labels) => {
                list(vec![text(&catalog.labels[&LabelId(1)])])
            }
            (ElementId::Vertex(B), GraphElementMetadata::Labels) => {
                list(vec![text(&catalog.labels[&LabelId(2)])])
            }
            (ElementId::Edge(R), GraphElementMetadata::Type) => {
                text(&catalog.relations[&RelationId(1)])
            }
            (ElementId::Vertex(A), GraphElementMetadata::Properties) => map("Ada"),
            (ElementId::Vertex(B), GraphElementMetadata::Properties) => map("Paris"),
            (ElementId::Edge(R), GraphElementMetadata::Properties) => map("trip"),
            (ElementId::Vertex(A | B) | ElementId::Edge(R), GraphElementMetadata::Keys) => {
                list(vec![text("name")])
            }
            _ => return Ok(None),
        };
        value
            .copy_with_control(&mut |event| control(event))
            .map(Some)
            .map_err(|()| GraphElementMetadataError::Interrupted)
    }
    fn prepare(statement: &str, values: Vec<GraphValue>) -> PreparedGraphSet {
        let template = PreparedGraphSetText::prepare_with_parameter_types_and_resolver(
            statement,
            &[("xs", GqlParameterType::List)],
            &mut names(),
        )
        .unwrap_or_else(|error| panic!("{statement}: {error}"));
        template
            .bind_parameters(&GqlParameters::new().with_list("xs", values).unwrap())
            .unwrap()
    }
    fn run(query: &PreparedGraphSet) -> Vec<Vec<GraphValue>> {
        query
            .execute_governed_from_source(
                policy(),
                with_element_metadata(no_pattern, metadata),
                || Ok::<_, usize>(()),
            )
            .unwrap()
            .value
            .into_iter()
            .map(|row| row.values().to_vec())
            .collect()
    }

    #[test]
    fn metadata_of_unwound_subscripted_and_local_elements_uses_their_full_identity() {
        for (statement, values, expected) in [
            (
                "UNWIND $xs AS x RETURN labels(x) AS value ORDER BY value",
                vec![GraphValue::Vertex(B), null(), GraphValue::Vertex(A)],
                vec![
                    vec![list(vec![text("Person")])],
                    vec![list(vec![text("Place")])],
                    vec![null()],
                ],
            ),
            (
                "WITH $xs AS xs RETURN labels(xs[0]) AS value, labels(xs[-1]) AS last",
                vec![GraphValue::Vertex(B), GraphValue::Vertex(A)],
                vec![vec![list(vec![text("Place")]), list(vec![text("Person")])]],
            ),
            (
                "RETURN [x IN $xs | labels(x)] AS value",
                vec![GraphValue::Vertex(B), null(), GraphValue::Vertex(A)],
                vec![vec![list(vec![
                    list(vec![text("Place")]),
                    null(),
                    list(vec![text("Person")]),
                ])]],
            ),
            (
                "RETURN [x IN $xs | type(x)] AS value",
                vec![GraphValue::Edge(R), null()],
                vec![vec![list(vec![text("VISITED"), null()])]],
            ),
            (
                "WITH $xs AS xs RETURN type(xs[0]) AS value",
                vec![GraphValue::Edge(R)],
                vec![vec![text("VISITED")]],
            ),
            (
                "RETURN [x IN $xs WHERE type(x) = 'VISITED' | upper(type(x))] AS value",
                vec![GraphValue::Edge(R), null()],
                vec![vec![list(vec![text("VISITED")])]],
            ),
            (
                "RETURN [x IN $xs WHERE labels(x) = ['Person'] | properties(x).name] AS value",
                vec![GraphValue::Vertex(B), null(), GraphValue::Vertex(A)],
                vec![vec![list(vec![text("Ada")])]],
            ),
            (
                "UNWIND $xs AS x RETURN properties(x).name AS name, 'Person' IN labels(x) AS person, size(labels(x)) AS count ORDER BY name",
                vec![GraphValue::Vertex(B), GraphValue::Vertex(A)],
                vec![
                    vec![
                        text("Ada"),
                        GraphValue::Scalar(CanonicalScalar::Bool(true)),
                        int(1),
                    ],
                    vec![
                        text("Paris"),
                        GraphValue::Scalar(CanonicalScalar::Bool(false)),
                        int(1),
                    ],
                ],
            ),
            (
                "RETURN [x IN $xs | labels(x) + ['extra']] AS value",
                vec![GraphValue::Vertex(A), GraphValue::Vertex(B)],
                vec![vec![list(vec![
                    list(vec![text("Person"), text("extra")]),
                    list(vec![text("Place"), text("extra")]),
                ])]],
            ),
            (
                "RETURN [x IN $xs | properties(x)] AS value, [x IN $xs | keys(x)] AS keys",
                vec![
                    GraphValue::Vertex(A),
                    GraphValue::Edge(R),
                    null(),
                    map("literal"),
                ],
                vec![vec![
                    list(vec![map("Ada"), map("trip"), null(), map("literal")]),
                    list(vec![
                        list(vec![text("name")]),
                        list(vec![text("name")]),
                        null(),
                        list(vec![text("name")]),
                    ]),
                ]],
            ),
            (
                "UNWIND $xs AS p RETURN [n IN nodes(p) | labels(n)] AS value, [r IN relationships(p) | type(r)] AS edges",
                vec![GraphValue::Path(GraphPath::new(
                    A,
                    vec![(R, B)].into_boxed_slice(),
                ))],
                vec![vec![
                    list(vec![list(vec![text("Person")]), list(vec![text("Place")])]),
                    list(vec![text("VISITED")]),
                ]],
            ),
        ] {
            let actual = run(&prepare(statement, values));
            assert_eq!(actual, expected, "{statement}");
        }
    }

    #[test]
    fn metadata_remains_live_after_grouping_and_in_aggregate_output() {
        let values = vec![
            GraphValue::Vertex(A),
            GraphValue::Vertex(B),
            GraphValue::Vertex(A),
        ];
        let expected = list(vec![
            list(vec![text("Person")]),
            list(vec![text("Person")]),
            list(vec![text("Place")]),
        ]);
        let query = prepare(
            "UNWIND $xs AS x WITH x ORDER BY x WITH collect(x) AS xs RETURN [n IN xs | labels(n)] AS value",
            values.clone(),
        );
        assert_eq!(run(&query), vec![vec![expected.clone()]]);
        let template =
            PreparedGraphPipelineAggregateText::prepare_with_parameter_types_and_resolver(
                "UNWIND $xs AS x WITH x ORDER BY x RETURN [n IN collect(x) | labels(n)] AS value",
                &[("xs", GqlParameterType::List)],
                names(),
            )
            .unwrap();
        let query = template
            .bind_relation_parameters(&GqlParameters::new().with_list("xs", values).unwrap())
            .unwrap();
        let result = query
            .execute_governed_from_source(
                policy(),
                with_element_metadata(no_pattern, metadata),
                || Ok::<_, usize>(()),
            )
            .unwrap();
        assert_eq!(result.value.len(), 1);
        let output = result.value[0].values();
        assert_eq!(output, &[fgdb_gql::GraphAggregateValue::Value(expected)]);
    }

    #[test]
    fn ordinary_aggregate_visitor_retains_controlled_element_metadata() {
        let query =
            fgdb_gql::PreparedGraphAggregateText::prepare_with_parameter_types_and_resolver(
                "MATCH (n) RETURN labels(min(n)) AS labels, properties(min(n)) AS properties",
                &[],
                names(),
            )
            .unwrap()
            .bind_parameters(&GqlParameters::new())
            .unwrap();
        let result = query
            .execute_governed_with_element_reads(
                2,
                [B, A],
                [],
                |_, _| Ok::<_, &'static str>(true),
                |_, _| Ok(None),
                with_element_metadata(
                    |_, _| -> Result<Option<fgdb_types::CanonicalPropertyValue>, &'static str> {
                        panic!("metadata never probes individual properties")
                    },
                    metadata,
                ),
                policy(),
                || Ok::<_, usize>(()),
            )
            .unwrap();
        assert_eq!(result.value.len(), 1);
        assert_eq!(
            result.value[0].values(),
            &[
                fgdb_gql::GraphAggregateValue::Value(list(vec![text("Person")])),
                fgdb_gql::GraphAggregateValue::Value(map("Ada")),
            ]
        );
    }

    #[test]
    fn metadata_is_lazy_and_unsupported_or_wrong_domain_reads_are_typed_errors() {
        let calls = Cell::new(0);
        for (expression, expected) in [
            (
                "CASE WHEN false THEN labels(x) ELSE ['safe'] END",
                list(vec![text("safe")]),
            ),
            ("coalesce(['safe'], labels(x))", list(vec![text("safe")])),
            (
                "CASE WHEN false THEN upper(type(x)) ELSE 'safe' END",
                text("safe"),
            ),
            ("coalesce('safe', upper(type(x)))", text("safe")),
            (
                "CASE WHEN false THEN properties(x).missing ELSE 'safe' END",
                text("safe"),
            ),
        ] {
            let query = prepare(
                &format!("UNWIND $xs AS x RETURN {expression} AS value"),
                vec![GraphValue::Vertex(A)],
            );
            let result = query.execute_governed_from_source(policy(), with_element_metadata(no_pattern,
                |element, kind, catalog: &ReverseSymbolCatalog, control: &mut dyn FnMut(GlaExecutionEvent) -> Result<(), ()>| {
                    calls.set(calls.get() + 1);
                    metadata(element, kind, catalog, control)
                }), || Ok::<_, usize>(())).unwrap();
            assert_eq!(result.value[0].values(), &[expected]);
        }
        assert_eq!(calls.get(), 0);
        for (expression, value) in [
            ("labels(x)", GraphValue::Edge(R)),
            ("type(x)", GraphValue::Vertex(A)),
            ("properties(x)", int(1)),
            ("keys(x)", int(1)),
        ] {
            let query = prepare(
                &format!("UNWIND $xs AS x RETURN {expression} AS value"),
                vec![value],
            );
            assert!(
                query
                    .execute_governed_from_source(
                        policy(),
                        with_element_metadata(no_pattern, metadata),
                        || Ok::<_, usize>(())
                    )
                    .is_err(),
                "{expression}"
            );
        }
        let query = prepare(
            "UNWIND $xs AS x RETURN labels(x) AS value",
            vec![GraphValue::Vertex(A)],
        );
        assert!(matches!(
            query.execute_governed_from_source(policy(), no_pattern, || Ok::<_, usize>(())),
            Err(GqlQueryError::Source(
                GraphSetExecutionError::Projection { .. }
            ))
        ));
        let null_query = prepare(
            "UNWIND $xs AS x RETURN labels(x) AS value, type(x) AS edge, properties(x) AS props, keys(x) AS keys",
            vec![null()],
        );
        let result = null_query
            .execute_governed_from_source(policy(), no_pattern, || Ok::<_, usize>(()))
            .unwrap();
        assert_eq!(result.value[0].values(), &[null(), null(), null(), null()]);
    }

    #[test]
    fn metadata_cancellation_and_source_errors_never_return_a_prefix() {
        let query = prepare(
            "UNWIND $xs AS x RETURN properties(x) AS value",
            vec![GraphValue::Vertex(A), GraphValue::Vertex(B)],
        );
        let checkpoints = Cell::new(0_usize);
        let result = query
            .execute_governed_from_source(
                policy(),
                with_element_metadata(no_pattern, metadata),
                || {
                    checkpoints.set(checkpoints.get() + 1);
                    Ok::<_, usize>(())
                },
            )
            .unwrap();
        assert_eq!(result.value.len(), 2);
        for stop in 0..checkpoints.get() {
            let at = Cell::new(0);
            let result = query.execute_governed_from_source(
                policy(),
                with_element_metadata(no_pattern, metadata),
                || {
                    let current = at.get();
                    at.set(current + 1);
                    if current == stop { Err(stop) } else { Ok(()) }
                },
            );
            assert!(
                matches!(result, Err(GqlQueryError::Interrupted(found)) if found == stop),
                "checkpoint {stop}"
            );
        }
        let calls = Cell::new(0);
        let result = query.execute_governed_from_source(
            policy(),
            with_element_metadata(
                no_pattern,
                |element,
                 kind,
                 catalog: &ReverseSymbolCatalog,
                 control: &mut dyn FnMut(GlaExecutionEvent) -> Result<(), ()>| {
                    calls.set(calls.get() + 1);
                    if calls.get() == 2 {
                        return Err(GraphElementMetadataError::Source("second read"));
                    }
                    metadata(element, kind, catalog, control)
                },
            ),
            || Ok::<_, usize>(()),
        );
        assert!(matches!(
            result,
            Err(GqlQueryError::Source(GraphSetExecutionError::Source(
                "second read"
            )))
        ));
    }

    #[test]
    fn metadata_uses_the_cumulative_exact_quota_even_under_limit_zero() {
        let query = prepare(
            "UNWIND $xs AS x RETURN properties(x) AS value LIMIT 0",
            vec![GraphValue::Vertex(A), GraphValue::Vertex(B)],
        );
        let execute = |policy| {
            query.execute_governed_from_source(
                policy,
                with_element_metadata(no_pattern, metadata),
                || Ok::<_, usize>(()),
            )
        };
        let reference = execute(policy()).unwrap();
        assert!(reference.value.is_empty());
        let work = reference.evaluator.work_units;
        let scratch = reference.evaluator.scratch_entries;
        assert!(work > 0 && scratch > 0);
        assert_eq!(
            execute(GqlQueryPolicy::new(1000, 1000, work, scratch)).unwrap(),
            reference
        );
        for (work, scratch, dimension) in [
            (work - 1, scratch, fgdb_gql::GlaLimitDimension::WorkUnits),
            (
                work,
                scratch - 1,
                fgdb_gql::GlaLimitDimension::ScratchEntries,
            ),
        ] {
            assert!(
                matches!(execute(GqlQueryPolicy::new(1000, 1000, work, scratch)),
                Err(GqlQueryError::Evaluator(error)) if error.dimension == dimension)
            );
        }
    }

    #[test]
    fn metadata_catalog_is_frozen_and_part_of_template_and_bound_identity() {
        let statement = "WITH $xs AS xs RETURN labels(xs[0]) AS value";
        let mut catalog = names();
        let first = PreparedGraphSetText::prepare_with_parameter_types_and_resolver(
            statement,
            &[("xs", GqlParameterType::List)],
            &mut catalog,
        )
        .unwrap();
        let identity = first.canonical_template_bytes();
        catalog.0.insert_label(LabelId(1), "Renamed");
        let second = PreparedGraphSetText::prepare_with_parameter_types_and_resolver(
            statement,
            &[("xs", GqlParameterType::List)],
            &mut catalog,
        )
        .unwrap();
        assert_ne!(identity, second.canonical_template_bytes());
        assert_eq!(identity, first.canonical_template_bytes());
        let parameters = GqlParameters::new()
            .with_list("xs", vec![GraphValue::Vertex(A)])
            .unwrap();
        let a = first.bind_parameters(&parameters).unwrap();
        let b = second.bind_parameters(&parameters).unwrap();
        assert_ne!(a.canonical_bytes(), b.canonical_bytes());
        assert_eq!(run(&a), vec![vec![list(vec![text("Person")])]]);
        assert_eq!(run(&b), vec![vec![list(vec![text("Renamed")])]]);
    }
}
