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
        fn resolve_symbol(&mut self, kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
            match (kind, name) {
                (GraphSymbolKind::Property, "name") => {
                    Some(GraphSymbol::Property(PropertyKeyId(1)))
                }
                _ => None,
            }
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
            (ElementId::Edge(R), GraphElementMetadata::StartNode) => GraphValue::Vertex(A),
            (ElementId::Edge(R), GraphElementMetadata::EndNode) => GraphValue::Vertex(B),
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
    fn relationship_endpoint_values_keep_full_identity_in_rows_lists_and_comparisons() {
        for (statement, values, expected) in [
            (
                "UNWIND $xs AS r RETURN startNode(r) AS source, endNode(r) AS target",
                vec![GraphValue::Edge(R), null(), GraphValue::Edge(EId(9))],
                vec![
                    vec![GraphValue::Vertex(A), GraphValue::Vertex(B)],
                    vec![null(), null()],
                    vec![null(), null()],
                ],
            ),
            (
                "WITH $xs AS edges RETURN startNode(edges[0]) AS source, \
                 [r IN edges | endNode(r)] AS targets",
                vec![GraphValue::Edge(R), null()],
                vec![vec![
                    GraphValue::Vertex(A),
                    list(vec![GraphValue::Vertex(B), null()]),
                ]],
            ),
            (
                "UNWIND $xs AS r RETURN startNode(r) IS NULL AS missing, \
                 endNode(r) IS NOT NULL AS present",
                vec![GraphValue::Edge(R), null(), GraphValue::Edge(EId(9))],
                vec![
                    vec![
                        GraphValue::Scalar(CanonicalScalar::Bool(false)),
                        GraphValue::Scalar(CanonicalScalar::Bool(true)),
                    ],
                    vec![
                        GraphValue::Scalar(CanonicalScalar::Bool(true)),
                        GraphValue::Scalar(CanonicalScalar::Bool(false)),
                    ],
                    vec![
                        GraphValue::Scalar(CanonicalScalar::Bool(true)),
                        GraphValue::Scalar(CanonicalScalar::Bool(false)),
                    ],
                ],
            ),
            (
                "WITH $xs AS xs RETURN startNode(xs[0]) = xs[1] AS same, \
                 endNode(xs[0]) = xs[1] AS different, xs[1] = startNode(xs[0]) AS reversed",
                vec![GraphValue::Edge(R), GraphValue::Vertex(A)],
                vec![vec![
                    GraphValue::Scalar(CanonicalScalar::Bool(true)),
                    GraphValue::Scalar(CanonicalScalar::Bool(false)),
                    GraphValue::Scalar(CanonicalScalar::Bool(true)),
                ]],
            ),
        ] {
            assert_eq!(run(&prepare(statement, values)), expected, "{statement}");
        }
    }

    #[test]
    fn endpoint_property_reads_use_the_derived_vertex_not_the_relationship() {
        let query = prepare(
            "UNWIND $xs AS r RETURN startNode(r).name AS source, endNode(r).name AS target",
            vec![GraphValue::Edge(R), null()],
        );
        let properties = Cell::new(0);
        let result = query
            .execute_governed_from_source(
                policy(),
                with_element_metadata(
                    fgdb_gql::WithElementReads {
                        source: no_pattern,
                        elements: |element, key| {
                            properties.set(properties.get() + 1);
                            assert_eq!(key, PropertyKeyId(1));
                            let name = match element {
                                ElementId::Vertex(A) => "Ada",
                                ElementId::Vertex(B) => "Paris",
                                _ => panic!("endpoint field read must address its vertex"),
                            };
                            Ok::<_, &'static str>(Some(
                                CanonicalScalar::ucs_basic_text(name).unwrap().into(),
                            ))
                        },
                    },
                    metadata,
                ),
                || Ok::<_, usize>(()),
            )
            .unwrap();
        assert_eq!(
            properties.get(),
            2,
            "NULL endpoints perform no property reads"
        );
        assert_eq!(result.value[0].values(), &[text("Ada"), text("Paris")]);
        assert_eq!(result.value[1].values(), &[null(), null()]);
    }

    #[test]
    fn endpoint_null_wrong_domain_and_lazy_branches_preserve_read_boundaries() {
        let null_query = PreparedGraphSetText::prepare(
            "RETURN startNode(NULL) AS source, endNode(NULL) AS target",
            |_, _| None,
        )
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
        let result = null_query
            .execute_governed_from_source(policy(), no_pattern, || Ok::<_, usize>(()))
            .unwrap();
        assert_eq!(result.value[0].values(), &[null(), null()]);
        for operand in [
            GraphValue::Vertex(A),
            int(7),
            list(vec![GraphValue::Edge(R)]),
        ] {
            for function in ["startNode", "endNode"] {
                let query = prepare(
                    &format!("UNWIND $xs AS x RETURN {function}(x) AS endpoint"),
                    vec![operand.clone()],
                );
                assert!(matches!(
                    query.execute_governed_from_source(
                        policy(),
                        with_element_metadata(
                            no_pattern,
                            |_,
                             _,
                             _: &ReverseSymbolCatalog,
                             _: &mut dyn FnMut(GlaExecutionEvent) -> Result<(), ()>| {
                                panic!("wrong-domain endpoint operands must refuse before source access")
                            },
                        ),
                        || Ok::<_, usize>(()),
                    ),
                    Err(GqlQueryError::Source(GraphSetExecutionError::Projection { .. }))
                ));
            }
        }
        for expression in [
            "CASE WHEN false THEN startNode(x) ELSE x END",
            "coalesce(x, endNode(x))",
        ] {
            let query = prepare(
                &format!("UNWIND $xs AS x RETURN {expression} AS endpoint"),
                vec![GraphValue::Vertex(A)],
            );
            let result = query
                .execute_governed_from_source(policy(), no_pattern, || Ok::<_, usize>(()))
                .unwrap();
            assert_eq!(result.value[0].values(), &[GraphValue::Vertex(A)]);
        }
    }

    #[test]
    fn endpoint_plans_do_not_depend_on_unread_catalog_names() {
        let statement =
            "WITH $xs AS edges RETURN startNode(edges[0]) AS source, endNode(edges[0]) AS target";
        let mut first = names();
        let mut second = names();
        second.0.insert_label(LabelId(1), "Changed");
        second.0.insert_relation(RelationId(99), "Unrelated");
        let a = PreparedGraphSetText::prepare_with_parameter_types_and_resolver(
            statement,
            &[("xs", GqlParameterType::List)],
            &mut first,
        )
        .unwrap();
        let b = PreparedGraphSetText::prepare_with_parameter_types_and_resolver(
            statement,
            &[("xs", GqlParameterType::List)],
            &mut second,
        )
        .unwrap();
        assert_eq!(a.canonical_template_bytes(), b.canonical_template_bytes());
        assert_eq!(a.column_types(), [fgdb_gql::GraphSetColumnType::Vertex; 2]);
        let args = GqlParameters::new()
            .with_list("xs", vec![GraphValue::Edge(R)])
            .unwrap();
        assert_eq!(
            a.bind_parameters(&args).unwrap().canonical_bytes(),
            b.bind_parameters(&args).unwrap().canonical_bytes(),
        );
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
    fn endpoints_remain_graph_values_after_grouping_and_in_aggregate_output() {
        let values = vec![GraphValue::Edge(R), GraphValue::Edge(R)];
        let expected = list(vec![GraphValue::Vertex(B), GraphValue::Vertex(B)]);
        let query = prepare(
            "UNWIND $xs AS r WITH collect(r) AS edges RETURN [r IN edges | endNode(r)] AS targets",
            values.clone(),
        );
        assert_eq!(run(&query), vec![vec![expected.clone()]]);
        let query = PreparedGraphPipelineAggregateText::prepare_with_parameter_types_and_resolver(
            "UNWIND $xs AS r RETURN [r IN collect(r) | endNode(r)] AS targets",
            &[("xs", GqlParameterType::List)],
            names(),
        )
        .unwrap()
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
        assert_eq!(
            result.value[0].values(),
            &[fgdb_gql::GraphAggregateValue::Value(expected)],
        );
    }

    #[test]
    fn element_entrypoints_preserve_metadata_for_sets_and_aggregate_outputs() {
        let values = vec![GraphValue::Vertex(A), GraphValue::Vertex(B)];
        let query = prepare(
            "UNWIND $xs AS x RETURN labels(x) AS labels, properties(x) AS props, keys(x) AS keys",
            values.clone(),
        );
        let reference = query
            .execute_governed_from_source(
                policy(),
                with_element_metadata(no_pattern, metadata),
                || Ok::<_, usize>(()),
            )
            .unwrap();
        for seeded in [false, true] {
            let reader = with_element_metadata(|_, _| Ok(None), metadata);
            let result = if seeded {
                query.execute_governed_with_seeded_elements(
                    policy(),
                    no_pattern,
                    |_, _, _| None,
                    |_, _, _| panic!("metadata needs no procedure"),
                    reader,
                    || Ok(()),
                )
            } else {
                query.execute_governed_with_elements(
                    policy(),
                    no_pattern,
                    |_, _, _| panic!("metadata needs no procedure"),
                    reader,
                    || Ok(()),
                )
            };
            assert_eq!(result.unwrap(), reference);
        }

        let query = PreparedGraphPipelineAggregateText::prepare_with_parameter_types_and_resolver(
            "UNWIND $xs AS x RETURN [n IN collect(x) | labels(n)] AS names",
            &[("xs", GqlParameterType::List)],
            names(),
        )
        .unwrap()
        .bind_relation_parameters(&GqlParameters::new().with_list("xs", values).unwrap())
        .unwrap();
        let reference = query
            .execute_governed_from_source(
                policy(),
                with_element_metadata(no_pattern, metadata),
                || Ok::<_, usize>(()),
            )
            .unwrap();
        for seeded in [false, true] {
            let reader = with_element_metadata(|_, _| Ok(None), metadata);
            let result = if seeded {
                query.execute_governed_with_seeded_elements(
                    policy(),
                    no_pattern,
                    |_, _, _| None,
                    |_, _, _| panic!("metadata needs no procedure"),
                    reader,
                    || Ok(()),
                )
            } else {
                query.execute_governed_with_elements(
                    policy(),
                    no_pattern,
                    |_, _, _| panic!("metadata needs no procedure"),
                    reader,
                    || Ok(()),
                )
            };
            assert_eq!(result.unwrap(), reference);
        }
    }

    #[test]
    fn explicit_reader_absence_keeps_null_and_property_only_reader_preserves_source_metadata() {
        let query = prepare(
            "UNWIND $xs AS x RETURN labels(x) AS names",
            vec![GraphValue::Vertex(A)],
        );
        let source = fgdb_gql::WithElementReads {
            source: with_element_metadata(no_pattern, metadata),
            elements: |_, _| Ok(None),
        };
        assert_eq!(
            query
                .execute_governed_from_source(policy(), source, || Ok::<_, usize>(()))
                .unwrap()
                .value[0]
                .values(),
            &[list(vec![text("Person")])],
        );
        let source = fgdb_gql::WithElementReads {
            source: with_element_metadata(no_pattern, metadata),
            elements: with_element_metadata(
                |_, _| Ok(None),
                |_,
                 _,
                 _: &ReverseSymbolCatalog,
                 _: &mut dyn FnMut(GlaExecutionEvent) -> Result<(), ()>| Ok(None),
            ),
        };
        assert_eq!(
            query
                .execute_governed_from_source(policy(), source, || Ok::<_, usize>(()))
                .unwrap()
                .value[0]
                .values(),
            &[null()],
        );
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

mod inferred_collection_parameters {
    use super::int;
    use fgdb_gql::algebra::{GraphValue, GraphValueRow};
    use fgdb_gql::{GqlParameterType, GqlParameters, GqlQueryPolicy, PreparedGraphSetText};
    use fgdb_types::{CanonicalScalar, VId};

    fn list(values: &[i64]) -> GraphValue {
        GraphValue::List(values.iter().copied().map(int).collect())
    }

    fn run(template: &PreparedGraphSetText, parameters: &GqlParameters) -> Vec<GraphValueRow> {
        template
            .bind_parameters(parameters)
            .unwrap()
            .execute_governed(
                GqlQueryPolicy::new(100, 100, 1_000_000, 1_000_000),
                |pattern, policy| {
                    pattern.plan().execute_governed_with_properties(
                        1,
                        [VId(1)],
                        [],
                        |_, _| Ok::<_, ()>(true),
                        |_, _| Ok(None),
                        policy,
                        || Ok::<_, ()>(()),
                    )
                },
                || Ok::<_, ()>(()),
            )
            .unwrap()
            .value
    }

    #[test]
    fn direct_list_sources_infer_in_graph_and_row_scopes() {
        let yes = GraphValue::Scalar(CanonicalScalar::Bool(true));
        let arguments = GqlParameters::new()
            .with_list("items", vec![int(1), int(2), int(3)])
            .unwrap();
        let cases = [
            ("[x IN $items | x + 1]", list(&[2, 3, 4])),
            ("[x IN $items WHERE x > 1 | x * 2]", list(&[4, 6])),
            ("any(x IN $items WHERE x = 2)", yes.clone()),
            ("all(x IN $items WHERE x > 0)", yes.clone()),
            ("none(x IN $items WHERE x < 0)", yes.clone()),
            ("single(x IN $items WHERE x = 2)", yes),
            ("head($items)", int(1)),
            ("last($items)", int(3)),
            ("tail($items)", list(&[2, 3])),
            ("size($items)", int(3)),
            ("reduce(total = 0, x IN $items | total + x)", int(6)),
        ];
        for prefix in ["", "MATCH (n) ", "WITH 1 AS seed "] {
            for (expression, expected) in &cases {
                let text = format!("{prefix}RETURN {expression} AS value");
                let inferred = PreparedGraphSetText::prepare(&text, |_, _| None).unwrap();
                let declared = PreparedGraphSetText::prepare_with_parameter_types(
                    &text,
                    &[("items", GqlParameterType::List)],
                    |_, _| None,
                )
                .unwrap();
                assert_eq!(
                    inferred.parameter_schema()[0].parameter_type,
                    GqlParameterType::List,
                    "{text}"
                );
                assert_eq!(
                    inferred.canonical_template_bytes(),
                    declared.canonical_template_bytes(),
                    "{text}"
                );
                assert_eq!(
                    run(&inferred, &arguments),
                    vec![GraphValueRow::from_owned_values(vec![expected.clone()])],
                    "{text}"
                );
            }
        }
    }

    fn inferred_as(text: &str, declarations: &[(&str, GqlParameterType)]) -> PreparedGraphSetText {
        let inferred = PreparedGraphSetText::prepare(text, |_, _| None)
            .unwrap_or_else(|error| panic!("{text}: {error:?}"));
        let declared =
            PreparedGraphSetText::prepare_with_parameter_types(text, declarations, |_, _| None)
                .unwrap();
        assert_eq!(
            inferred.canonical_template_bytes(),
            declared.canonical_template_bytes(),
            "{text}"
        );
        for &(name, kind) in declarations {
            assert!(
                inferred
                    .parameter_schema()
                    .iter()
                    .any(|spec| { spec.name.as_str() == name && spec.parameter_type == kind }),
                "{text}: {name}"
            );
        }
        inferred
    }

    #[test]
    fn list_constraints_follow_identity_aliases_and_graph_boundaries() {
        let arguments = GqlParameters::new()
            .with_list("items", vec![int(1), int(2), int(3)])
            .unwrap();
        let yes = GraphValue::Scalar(CanonicalScalar::Bool(true));
        for prefix in [
            "WITH $items AS xs ",
            "MATCH (n) WITH $items AS xs ",
            "WITH $items AS original WITH original AS xs ",
            "WITH $items AS xs WITH * ",
            "WITH $items AS original MATCH (n) WITH original AS xs ",
            "WITH $items AS xs MATCH (n) WITH * ",
            "WITH $items AS original WITH original AS xs, count(*) AS occurrences ",
        ] {
            for (expression, expected) in [
                ("[x IN xs | x + 1]", list(&[2, 3, 4])),
                ("[x IN xs WHERE x > 1 | x * 2]", list(&[4, 6])),
                ("any(x IN xs WHERE x = 2)", yes.clone()),
                ("all(x IN xs WHERE x > 0)", yes.clone()),
                ("none(x IN xs WHERE x < 0)", yes.clone()),
                ("single(x IN xs WHERE x = 2)", yes.clone()),
                ("head(xs)", int(1)),
                ("last(xs)", int(3)),
                ("tail(xs)", list(&[2, 3])),
                ("size(xs)", int(3)),
                ("reduce(total = 0, x IN xs | total + x)", int(6)),
            ] {
                let text = format!("{prefix}RETURN {expression} AS value");
                let inferred = inferred_as(&text, &[("items", GqlParameterType::List)]);
                assert_eq!(
                    run(&inferred, &arguments),
                    vec![GraphValueRow::from_owned_values(vec![expected])],
                    "{text}"
                );
            }
        }
    }

    #[test]
    fn aliased_lists_rebind_without_changing_order_multiplicity_or_nulls() {
        for text in [
            "WITH $items AS original WITH original AS xs \
             RETURN [x IN xs | x] AS copy, head(xs) AS first, tail(xs) AS rest",
            "WITH $items AS xs MATCH (n) \
             RETURN [x IN xs | x] AS copy, head(xs) AS first, tail(xs) AS rest",
        ] {
            let template = inferred_as(text, &[("items", GqlParameterType::List)]);
            let definition = template.canonical_template_bytes();
            let null = GraphValue::Scalar(CanonicalScalar::Null);
            for values in [vec![], vec![2, 2, 7], vec![9]] {
                let arguments = GqlParameters::new()
                    .with_list("items", values.iter().copied().map(int).collect())
                    .unwrap();
                let first = values.first().copied().map_or_else(|| null.clone(), int);
                assert_eq!(
                    run(&template, &arguments),
                    vec![GraphValueRow::from_owned_values(vec![
                        list(&values),
                        first,
                        list(&values[values.len().min(1)..]),
                    ])],
                    "{text}"
                );
                assert_eq!(template.canonical_template_bytes(), definition);
            }
            assert_eq!(
                run(&template, &GqlParameters::new().with_null("items").unwrap()),
                vec![GraphValueRow::from_owned_values(vec![null; 3])]
            );
        }
        let unwind = inferred_as(
            "WITH $items AS original WITH original AS xs UNWIND xs AS item RETURN item",
            &[("items", GqlParameterType::List)],
        );
        for values in [vec![], vec![2, 2, 7]] {
            let arguments = GqlParameters::new()
                .with_list("items", values.iter().copied().map(int).collect())
                .unwrap();
            assert_eq!(
                run(&unwind, &arguments),
                values
                    .into_iter()
                    .map(|value| GraphValueRow::from_owned_values(vec![int(value)]))
                    .collect::<Vec<_>>()
            );
        }
        assert!(run(&unwind, &GqlParameters::new().with_null("items").unwrap()).is_empty());
    }

    #[test]
    fn map_aliases_and_multiple_parameters_infer_one_reusable_contract() {
        for prefix in [
            "WITH $props AS p ",
            "WITH $props AS original WITH original AS p ",
            "MATCH (n) WITH $props AS p ",
            "WITH $props AS original MATCH (n) WITH original AS p ",
        ] {
            for expression in ["keys(p)", "properties(p)", "p.a"] {
                let text = format!("{prefix}RETURN {expression} AS value");
                let inferred = inferred_as(&text, &[("props", GqlParameterType::Map)]);
                for value in [1, 9] {
                    let entries = vec![("a".into(), int(value))];
                    let arguments = GqlParameters::new()
                        .with_map("props", entries.clone())
                        .unwrap();
                    let expected = match expression {
                        "keys(p)" => GraphValue::List(
                            vec![GraphValue::Scalar(
                                CanonicalScalar::ucs_basic_text("a").unwrap(),
                            )]
                            .into(),
                        ),
                        "properties(p)" => GraphValue::map(entries).unwrap(),
                        _ => int(value),
                    };
                    assert_eq!(
                        run(&inferred, &arguments),
                        vec![GraphValueRow::from_owned_values(vec![expected])],
                        "{text}"
                    );
                }
                assert_eq!(
                    run(&inferred, &GqlParameters::new().with_null("props").unwrap()),
                    vec![GraphValueRow::from_owned_values(vec![GraphValue::Scalar(
                        CanonicalScalar::Null,
                    )])],
                    "{text}"
                );
            }
        }
        let inferred = inferred_as(
            "WITH $items AS xs, $props AS p \
             RETURN [x IN xs | x] AS copy, keys(p) AS names",
            &[
                ("items", GqlParameterType::List),
                ("props", GqlParameterType::Map),
            ],
        );
        assert_eq!(inferred.parameter_schema()[0].name.as_str(), "items");
        assert_eq!(inferred.parameter_schema()[1].name.as_str(), "props");
        assert_eq!(
            run(
                &inferred,
                &GqlParameters::new()
                    .with_list("items", vec![int(2), int(2)])
                    .unwrap()
                    .with_map("props", vec![])
                    .unwrap()
            ),
            vec![GraphValueRow::from_owned_values(vec![
                list(&[2, 2]),
                list(&[]),
            ])]
        );
    }

    #[test]
    fn alias_inference_respects_shadowing_explicit_types_and_scalar_constraints() {
        for text in [
            "WITH $items AS xs WITH [9] AS xs RETURN head(xs) AS value",
            "WITH $items AS xs RETURN head([xs IN [[9]] | head(xs)]) AS value",
        ] {
            let template = inferred_as(text, &[("items", GqlParameterType::Int64)]);
            assert_eq!(
                run(
                    &template,
                    &GqlParameters::new().with_int64("items", 1).unwrap()
                ),
                vec![GraphValueRow::from_owned_values(vec![int(9)])]
            );
        }
        let declared = PreparedGraphSetText::prepare_with_parameter_types(
            "WITH $items AS xs RETURN size(xs) AS value",
            &[(
                "items",
                GqlParameterType::Scalar(fgdb_types::CanonicalScalarKind::Text),
            )],
            |_, _| None,
        )
        .unwrap();
        assert_eq!(
            run(
                &declared,
                &GqlParameters::new().with_text("items", "naïve").unwrap()
            ),
            vec![GraphValueRow::from_owned_values(vec![int(5)])]
        );
        let null = PreparedGraphSetText::prepare_with_parameter_types(
            "WITH $items AS xs RETURN head(xs) AS value",
            &[(
                "items",
                GqlParameterType::Scalar(fgdb_types::CanonicalScalarKind::Null),
            )],
            |_, _| None,
        )
        .unwrap();
        assert_eq!(
            run(&null, &GqlParameters::new().with_null("items").unwrap()),
            vec![GraphValueRow::from_owned_values(vec![GraphValue::Scalar(
                CanonicalScalar::Null,
            )])]
        );
        // Dynamic row admission does not change an explicit argument's
        // contract: legacy Int64 remains nonnullable and never becomes List.
        let scalar = PreparedGraphSetText::prepare_with_parameter_types(
            "WITH $items AS xs RETURN head(xs) AS value",
            &[("items", GqlParameterType::Int64)],
            |_, _| None,
        )
        .unwrap();
        assert_eq!(
            scalar.parameter_schema()[0].parameter_type,
            GqlParameterType::Int64
        );
        assert!(
            scalar
                .bind_parameters(&GqlParameters::new().with_list("items", vec![]).unwrap())
                .is_err()
        );
        assert!(
            scalar
                .bind_parameters(&GqlParameters::new().with_null("items").unwrap())
                .is_err()
        );
        let nullable = PreparedGraphSetText::prepare_with_parameter_types(
            "WITH $items AS xs RETURN head(xs) AS value",
            &[(
                "items",
                GqlParameterType::Scalar(fgdb_types::CanonicalScalarKind::Int),
            )],
            |_, _| None,
        )
        .unwrap();
        assert_eq!(
            run(&nullable, &GqlParameters::new().with_null("items").unwrap()),
            vec![GraphValueRow::from_owned_values(vec![GraphValue::Scalar(
                CanonicalScalar::Null,
            )])]
        );
        assert!(
            PreparedGraphSetText::prepare(
                "MATCH (n:Person) WITH $items AS xs RETURN xs * 2 AS scalar, head(xs) AS first",
                |_, _| panic!("incompatible scalar uses must refuse before catalog resolution"),
            )
            .is_err()
        );
        let union = inferred_as(
            "WITH $items AS xs RETURN size(xs) AS value \
             UNION ALL WITH 9 AS xs RETURN xs AS value",
            &[("items", GqlParameterType::List)],
        );
        assert_eq!(
            run(
                &union,
                &GqlParameters::new()
                    .with_list("items", vec![int(2), int(2)])
                    .unwrap()
            ),
            vec![
                GraphValueRow::from_owned_values(vec![int(2)]),
                GraphValueRow::from_owned_values(vec![int(9)]),
            ]
        );
    }

    #[test]
    fn conflicting_alias_and_direct_collection_requirements_refuse_before_catalog() {
        for text in [
            "MATCH (n:Person) WITH $items AS xs RETURN head(xs) AS first, keys(xs) AS names",
            "MATCH (n:Person) WITH $items AS xs RETURN keys(xs) AS names, head(xs) AS first",
            "MATCH (n:Person) WITH $items AS xs RETURN head(xs) AS first, xs.a AS field",
            "MATCH (n:Person) WITH $items AS xs RETURN xs.a AS field, head(xs) AS first",
            "MATCH (n:Person) WITH $items AS xs RETURN head(xs) AS first, keys($items) AS names",
            "MATCH (n:Person) WITH $items AS xs RETURN keys(xs) AS names, head($items) AS first",
            "MATCH (n:Person) WITH $items AS xs RETURN head($items) AS first, keys(xs) AS names",
            "MATCH (n:Person) WITH $items AS xs RETURN keys($items) AS names, head(xs) AS first",
        ] {
            let result = PreparedGraphSetText::prepare(text, |_, _| {
                panic!("incompatible collection uses must refuse before catalog resolution")
            });
            assert!(
                matches!(
                    result,
                    Err(fgdb_gql::GraphSetTextError {
                        kind: fgdb_gql::GraphSetTextErrorKind::Pattern(
                            fgdb_gql::GraphPatternTextErrorKind::ConflictingParameterTypes,
                        ),
                        ..
                    })
                ),
                "{text}"
            );
        }
        // The caller can declare NULL explicitly; neither collection use
        // changes that contract or invents a conflicting inferred domain.
        let null = PreparedGraphSetText::prepare_with_parameter_types(
            "WITH $items AS xs RETURN head(xs) AS first, keys(xs) AS names",
            &[(
                "items",
                GqlParameterType::Scalar(fgdb_types::CanonicalScalarKind::Null),
            )],
            |_, _| None,
        )
        .unwrap();
        let absent = GraphValue::Scalar(CanonicalScalar::Null);
        assert_eq!(
            run(&null, &GqlParameters::new().with_null("items").unwrap()),
            vec![GraphValueRow::from_owned_values(vec![absent; 2])]
        );
    }

    #[test]
    fn inferred_lists_rebind_and_preserve_empty_null_and_repeated_uses() {
        let text = "RETURN [x IN $items | x] AS copy, head($items) AS first, \
                    tail($items) AS rest, $items AS original";
        let template = PreparedGraphSetText::prepare(text, |_, _| None).unwrap();
        let definition = template.canonical_template_bytes();
        let null = GraphValue::Scalar(CanonicalScalar::Null);
        for values in [vec![], vec![2, 2, 7], vec![9]] {
            let arguments = GqlParameters::new()
                .with_list("items", values.iter().copied().map(int).collect())
                .unwrap();
            let first = values.first().copied().map_or_else(|| null.clone(), int);
            let rest = &values[values.len().min(1)..];
            assert_eq!(
                run(&template, &arguments),
                vec![GraphValueRow::from_owned_values(vec![
                    list(&values),
                    first,
                    list(rest),
                    list(&values),
                ])]
            );
            assert_eq!(template.canonical_template_bytes(), definition);
        }
        assert_eq!(
            run(&template, &GqlParameters::new().with_null("items").unwrap()),
            vec![GraphValueRow::from_owned_values(vec![null; 4])]
        );
        for invalid in [
            GqlParameters::new().with_int64("items", 1).unwrap(),
            GqlParameters::new().with_text("items", "list").unwrap(),
            GqlParameters::new().with_map("items", vec![]).unwrap(),
        ] {
            assert!(template.bind_parameters(&invalid).is_err());
        }
    }

    #[test]
    fn map_functions_infer_the_whole_parameter_without_retaining_values() {
        let text = "RETURN keys($props) AS names, properties($props) AS value";
        let template = PreparedGraphSetText::prepare(text, |_, _| None).unwrap();
        assert_eq!(
            template.parameter_schema()[0].parameter_type,
            GqlParameterType::Map
        );
        let declared = PreparedGraphSetText::prepare_with_parameter_types(
            text,
            &[("props", GqlParameterType::Map)],
            |_, _| None,
        )
        .unwrap();
        assert_eq!(
            template.canonical_template_bytes(),
            declared.canonical_template_bytes()
        );
        for value in [1, 9] {
            let entries = vec![("a".into(), int(value)), ("b".into(), list(&[2, 3]))];
            let arguments = GqlParameters::new()
                .with_map("props", entries.clone())
                .unwrap();
            assert_eq!(
                run(&template, &arguments),
                vec![GraphValueRow::from_owned_values(vec![
                    GraphValue::List(
                        ["a", "b"]
                            .map(|key| {
                                GraphValue::Scalar(CanonicalScalar::ucs_basic_text(key).unwrap())
                            })
                            .into(),
                    ),
                    GraphValue::map(entries).unwrap(),
                ])]
            );
        }
        let null = GraphValue::Scalar(CanonicalScalar::Null);
        assert_eq!(
            run(&template, &GqlParameters::new().with_null("props").unwrap()),
            vec![GraphValueRow::from_owned_values(vec![null; 2])]
        );
    }

    #[test]
    fn collection_context_does_not_retype_fields_arithmetic_or_declared_arguments() {
        let map = GqlParameters::new()
            .with_map("payload", vec![("items".into(), list(&[3, 4]))])
            .unwrap();
        for expression in [
            "[x IN $payload.items | x]",
            "tail($payload.items)",
            "reduce(total = 0, x IN $payload.items | total + x)",
        ] {
            let text = format!("RETURN {expression} AS value");
            let inferred = PreparedGraphSetText::prepare(&text, |_, _| None).unwrap();
            assert_eq!(
                inferred.parameter_schema()[0].parameter_type,
                GqlParameterType::Map
            );
            assert!(inferred.bind_parameters(&map).is_ok());
        }
        let text = "RETURN head($value + [7]) AS value";
        let template = PreparedGraphSetText::prepare(text, |_, _| None).unwrap();
        assert_eq!(
            template.parameter_schema()[0].parameter_type,
            GqlParameterType::Int64
        );
        assert_eq!(
            run(
                &template,
                &GqlParameters::new().with_int64("value", 5).unwrap()
            ),
            vec![GraphValueRow::from_owned_values(vec![int(5)])]
        );
        let scalar =
            PreparedGraphSetText::prepare("WITH $value AS k RETURN k + 1 AS value", |_, _| None)
                .unwrap();
        assert_eq!(
            scalar.parameter_schema()[0].parameter_type,
            GqlParameterType::Int64
        );
        let declared = PreparedGraphSetText::prepare_with_parameter_types(
            "RETURN head($items) AS value",
            &[(
                "items",
                GqlParameterType::Scalar(fgdb_types::CanonicalScalarKind::Null),
            )],
            |_, _| None,
        )
        .unwrap();
        assert_eq!(
            declared.parameter_schema()[0].parameter_type,
            GqlParameterType::Scalar(fgdb_types::CanonicalScalarKind::Null)
        );
        assert_eq!(
            run(&declared, &GqlParameters::new().with_null("items").unwrap()),
            vec![GraphValueRow::from_owned_values(vec![GraphValue::Scalar(
                CanonicalScalar::Null,
            )])]
        );
        assert!(
            PreparedGraphSetText::prepare(
                "RETURN $items + 1 AS scalar, head($items) AS value",
                |_, _| None,
            )
            .is_err()
        );
    }
}
