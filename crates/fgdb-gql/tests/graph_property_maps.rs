//! Complete element maps share native projection, scope and resource owners.

use fgdb_delta_types::catalog::{CatalogSymbol, SymbolCatalog};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::algebra::{
    GlaDirection, GraphColumn, GraphPathFunction, GraphPatternBuilder, GraphValue, GraphValueRow,
    PreparedGraphPattern,
};
use fgdb_gql::{
    GlaExecutionLimits, GqlParameters, GqlQueryError, GqlQueryExecution, GqlQueryPolicy,
    GraphAggregateValue, GraphIntegerError, GraphIntegerErrorKind, GraphSymbol, GraphSymbolKind,
    GraphSymbolResolver, PreparedGraphAggregateText, PreparedGraphSetText, PreparedGraphText,
    ReverseSymbolCatalog,
};
use fgdb_types::{CanonicalPropertyValue, CanonicalScalar, EId, VId};
use std::cell::Cell;
use std::collections::BTreeMap;

const P: PropertyKeyId = PropertyKeyId(1);
const R: RelationId = RelationId(1);
type Edge = (EId, VId, RelationId, VId);

#[test]
fn native_catalog_resolver_drives_relation_names_and_complete_property_maps() {
    use fgdb_gql::symbol_bindings::CatalogSymbolResolver;

    let mut catalog = SymbolCatalog::default();
    catalog
        .bind_all([
            ("MENTORS", CatalogSymbol::Relation(R)),
            ("p", CatalogSymbol::Property(P)),
        ])
        .unwrap();
    let mut fixture = Fixture::new();
    // This low-level source lends the already-resolved type text, just as the
    // embedded source does. Bind its metadata through the same catalog pin.
    for (edge, _, relation, _) in &fixture.edges {
        let name = catalog.name(CatalogSymbol::Relation(*relation)).unwrap();
        fixture
            .edge_types
            .insert(*edge, CanonicalScalar::ucs_basic_text(name).unwrap());
    }
    let mut resolver = CatalogSymbolResolver::new(&catalog);
    let query = PreparedGraphSetText::prepare_with_parameter_types_and_resolver(
        "MATCH (a)-[r:MENTORS]->(b) RETURN a.p AS value, type(r) AS relation, properties(r) AS props",
        &[],
        &mut resolver,
    )
    .unwrap()
    .bind_parameters(&GqlParameters::new())
    .unwrap();
    let result = query
        .execute_governed(
            policy(),
            |pattern, policy| fixture.execute(pattern, policy),
            || Ok::<_, ()>(()),
        )
        .unwrap();
    let relation = GraphValue::Scalar(CanonicalScalar::ucs_basic_text("MENTORS").unwrap());
    assert_eq!(
        result
            .value
            .into_iter()
            .map(|row| row.values().to_vec())
            .collect::<Vec<_>>(),
        vec![
            vec![integer(1), relation.clone(), map(&[("p", integer(11))])],
            vec![integer(1), relation, map(&[("p", integer(12))])],
        ]
    );
}

#[derive(Clone, Copy)]
struct Catalog(&'static str);
impl GraphSymbolResolver for Catalog {
    fn resolve_symbol(&mut self, kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        match (kind, name) {
            (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
            (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
            _ => None,
        }
    }
    fn reverse_catalog(&self) -> Option<ReverseSymbolCatalog> {
        let mut catalog = ReverseSymbolCatalog::new();
        catalog.insert_property(P, self.0);
        catalog.insert_relation(R, "R");
        Some(catalog)
    }
}
fn integer(value: i64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(value))
}
fn map(entries: &[(&str, GraphValue)]) -> GraphValue {
    GraphValue::map(
        entries
            .iter()
            .map(|(key, value)| ((*key).into(), value.clone()))
            .collect(),
    )
    .unwrap()
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(10_000, 10_000, 100_000, 100_000)
}
fn property(value: Option<&GraphValue>, key: PropertyKeyId) -> Option<&CanonicalScalar> {
    let (keys, values) = value?.as_map()?;
    let name = match key {
        P => "p",
        _ => return None,
    };
    values
        .get(keys.binary_search_by(|key| key.as_ref().cmp(name)).ok()?)?
        .as_scalar()
}
/// The stored property values the per-key source reads: exactly what
/// `property` finds in each element's map, held as stored values.
fn stored<K: Copy + Ord>(
    maps: &BTreeMap<K, GraphValue>,
) -> BTreeMap<(K, PropertyKeyId), CanonicalPropertyValue> {
    maps.iter()
        .filter_map(|(&element, value)| {
            let stored = CanonicalPropertyValue::from(property(Some(value), P)?.clone());
            Some(((element, P), stored))
        })
        .collect()
}

struct Fixture {
    vertices: BTreeMap<VId, GraphValue>,
    edges: Vec<Edge>,
    edge_maps: BTreeMap<EId, GraphValue>,
    vertex_values: BTreeMap<(VId, PropertyKeyId), CanonicalPropertyValue>,
    edge_values: BTreeMap<(EId, PropertyKeyId), CanonicalPropertyValue>,
    edge_types: BTreeMap<EId, CanonicalScalar>,
}
impl Fixture {
    fn new() -> Self {
        let vertices = BTreeMap::from([(VId(1), map(&[("p", integer(1))])), (VId(2), map(&[]))]);
        let edge_maps = BTreeMap::from([
            (EId(11), map(&[("p", integer(11))])),
            (EId(12), map(&[("p", integer(12))])),
        ]);
        Self {
            vertex_values: stored(&vertices),
            edge_values: stored(&edge_maps),
            edge_types: BTreeMap::new(),
            vertices,
            edges: vec![(EId(11), VId(1), R, VId(2)), (EId(12), VId(1), R, VId(2))],
            edge_maps,
        }
    }
    fn execute(
        &self,
        pattern: &PreparedGraphPattern<GraphValueRow>,
        policy: GqlQueryPolicy,
    ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<(), ()>> {
        pattern.plan().execute_governed_with_element_maps(
            (self.vertices.len() + self.edges.len()) as u64,
            self.vertices.keys().copied(),
            self.edges.iter().copied(),
            |_, _| Ok(true),
            |vid, key| Ok(self.vertex_values.get(&(vid, key))),
            |eid, key| Ok(self.edge_values.get(&(eid, key))),
            |_| Ok(None),
            |eid| Ok(self.edge_types.get(&eid)),
            |vid| Ok(self.vertices.get(&vid)),
            |eid| Ok(self.edge_maps.get(&eid)),
            policy,
            || Ok(()),
        )
    }
    fn rows(&self, text: &str) -> Vec<Vec<GraphValue>> {
        let query = PreparedGraphSetText::prepare_with_parameter_types_and_resolver(
            text,
            &[],
            &mut Catalog("p"),
        )
        .unwrap_or_else(|error| panic!("{text}: {error:?}"))
        .bind_parameters(&GqlParameters::new())
        .unwrap();
        query
            .execute_governed(
                policy(),
                |pattern, policy| self.execute(pattern, policy),
                || Ok::<_, ()>(()),
            )
            .unwrap_or_else(|error| panic!("{text}: {error:?}"))
            .value
            .into_iter()
            .map(|row| row.values().to_vec())
            .collect()
    }
}

#[test]
fn typed_maps_keep_parallel_edge_identity_and_share_cumulative_limits() {
    let mut builder = GraphPatternBuilder::new();
    builder.vertex("a").unwrap().vertex("b").unwrap();
    builder.edge("a", R, GlaDirection::Forward, "b").unwrap();
    builder.capture_edge("r", 0).unwrap();
    let pattern = builder
        .prepare_values(
            &[
                GraphColumn::path("node", "a", GraphPathFunction::Properties),
                GraphColumn::path("edge", "r", GraphPathFunction::Properties),
            ],
            0,
            None,
        )
        .unwrap()
        .with_duplicates();
    assert!(pattern.plan().projects_vertex_property_maps());
    assert!(pattern.plan().projects_edge_property_maps());
    assert!(pattern.plan().needs_vertex_values());
    assert!(pattern.plan().requires_identified_edges());
    let mut source = Fixture::new();
    let complete = source.execute(&pattern, policy()).unwrap();
    let expected: Vec<_> = [11, 12]
        .into_iter()
        .map(|value| vec![map(&[("p", integer(1))]), map(&[("p", integer(value))])])
        .collect();
    assert_eq!(
        complete
            .value
            .iter()
            .map(|row| row.values().to_vec())
            .collect::<Vec<_>>(),
        expected
    );
    // Each bound is the cost measured on the same input. The reversed edge
    // order is a different input with the same values, whose cost may
    // differ; it was once measured against the original order's cost
    // (fgdb-nevwt). Both orders are now held to their own measurement.
    for order in ["original", "reversed"] {
        let measured = source.execute(&pattern, policy()).unwrap();
        assert_eq!(measured.value, complete.value, "{order}");
        for (work, scratch) in [
            (
                measured.evaluator.work_units - 1,
                measured.evaluator.scratch_entries,
            ),
            (
                measured.evaluator.work_units,
                measured.evaluator.scratch_entries - 1,
            ),
        ] {
            let mut bounded = policy();
            bounded.evaluator = GlaExecutionLimits::new(work, scratch);
            assert!(
                matches!(
                    source.execute(&pattern, bounded),
                    Err(GqlQueryError::Evaluator(_))
                ),
                "{order}: work {work}, scratch {scratch}"
            );
        }
        let mut exact = policy();
        exact.evaluator = GlaExecutionLimits::new(
            measured.evaluator.work_units,
            measured.evaluator.scratch_entries,
        );
        assert_eq!(
            source.execute(&pattern, exact).unwrap().value,
            complete.value,
            "{order}"
        );
        source.edges.reverse();
    }
}

#[test]
fn legacy_sources_refuse_enumeration_before_consuming_inputs_even_for_limit_zero() {
    let input_reads = Cell::new(0);
    for suffix in ["", " LIMIT 0"] {
        let text = format!("MATCH (n) RETURN properties(n) AS m{suffix}");
        let pattern = PreparedGraphText::prepare_with_resolver(&text, Catalog("p"))
            .unwrap()
            .bind_parameters(&GqlParameters::new())
            .unwrap();
        let vertices = std::iter::once(VId(1)).inspect(|_| input_reads.set(input_reads.get() + 1));
        let result = pattern.plan().execute_governed_with_element_accessors(
            1,
            vertices,
            [],
            |_, _| Ok::<_, ()>(true),
            |_, _| Ok(None),
            |_, _| Ok(None),
            |_| Ok(None),
            |_| Ok(None),
            policy(),
            || Ok::<_, ()>(()),
        );
        assert!(matches!(
            result,
            Err(GqlQueryError::Data(GraphIntegerError {
                kind: GraphIntegerErrorKind::PropertyMapSourceRequired,
                ..
            }))
        ));
        assert_eq!(input_reads.get(), 0);
        for (value, kind) in [
            (None, GraphIntegerErrorKind::PropertyMapSourceRequired),
            (Some(integer(1)), GraphIntegerErrorKind::NonMap),
        ] {
            let result = pattern.plan().execute_governed_with_element_maps(
                1,
                [VId(1)],
                [],
                |_, _| Ok::<_, ()>(true),
                |_, _| Ok(None),
                |_, _| Ok(None),
                |_| Ok(None),
                |_| Ok(None),
                |_| Ok(value.as_ref()),
                |_| Ok(None),
                policy(),
                || Ok::<_, ()>(()),
            );
            assert!(matches!(result, Err(GqlQueryError::Data(error)) if error.kind == kind));
        }
    }
}

fn assert_vertex_property_source_refusal(text: &str) {
    let pattern = PreparedGraphText::prepare_with_resolver(text, Catalog("p"))
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
    let populated = [VId(1), VId(2)];
    for (identified, ids) in [
        (false, populated.as_slice()),
        (true, populated.as_slice()),
        (false, &[]),
        (true, &[]),
    ] {
        let reads = Cell::new(0);
        let touch = || reads.set(reads.get() + 1);
        let vertices = ids.iter().copied().inspect(|_| touch());
        let test_vertex = |_, _: &[_]| {
            touch();
            Ok::<_, GraphIntegerError>(true)
        };
        let property = |_, _| {
            touch();
            Ok::<_, GraphIntegerError>(None)
        };
        let control = |_| {
            touch();
            Ok::<_, GraphIntegerError>(())
        };
        let result = if identified {
            pattern.plan().execute_with_identified_properties_control(
                vertices,
                [(EId(11), VId(1), R, VId(2))]
                    .into_iter()
                    .inspect(|_| touch()),
                test_vertex,
                property,
                control,
            )
        } else {
            pattern.plan().execute_with_properties_control(
                vertices,
                [(VId(1), R, VId(2))].into_iter().inspect(|_| touch()),
                test_vertex,
                property,
                control,
            )
        };
        let error = result.expect_err(text);
        assert_eq!(
            error.kind,
            GraphIntegerErrorKind::ElementSourceRequired,
            "{text}"
        );
        assert_eq!(
            reads.get(),
            0,
            "{text}, identified={identified}, ids={ids:?}"
        );
    }
}

#[test]
fn vertex_property_sources_refuse_element_projections_before_consuming_inputs() {
    for suffix in ["", " LIMIT 0"] {
        for text in [
            "MATCH (n) RETURN labels(n) AS labels",
            "MATCH (a)-[r:R]->(b) RETURN type(r) AS type",
            "MATCH (a)-[r:R]->(b) RETURN r.p AS p",
        ] {
            assert_vertex_property_source_refusal(&format!("{text}{suffix}"));
        }
    }
}

#[test]
fn vertex_property_sources_refuse_captured_edge_predicates_instead_of_empty_rows() {
    let source = Fixture::new();
    assert_eq!(
        source.rows("MATCH (a)-[r:R]->(b) WHERE r.p = 11 RETURN a"),
        vec![vec![GraphValue::Vertex(VId(1))]],
    );
    assert_eq!(
        source.rows("MATCH (a)-[r:R]->(b) WHERE r.p >= 11 RETURN a"),
        vec![vec![GraphValue::Vertex(VId(1))]; 2],
    );
    for suffix in ["", " LIMIT 0"] {
        for text in [
            "MATCH (a)-[r:R]->(b) WHERE r.p = 11 RETURN a",
            "MATCH (a)-[r:R]->(b) WHERE r.p >= 11 RETURN a",
        ] {
            assert_vertex_property_source_refusal(&format!("{text}{suffix}"));
        }
    }
}

#[test]
fn vertex_property_and_full_element_sources_keep_their_supported_answers() {
    let source = Fixture::new();
    let node_label = GraphValue::Scalar(CanonicalScalar::ucs_basic_text("L").unwrap());
    let labels = [node_label.clone()];
    let edge_type = CanonicalScalar::ucs_basic_text("R").unwrap();
    let null = GraphValue::Scalar(CanonicalScalar::Null);
    let cases = [
        (
            "MATCH (n) RETURN n, n.p AS p",
            vec![
                vec![GraphValue::Vertex(VId(1)), integer(1)],
                vec![GraphValue::Vertex(VId(2)), null],
            ],
        ),
        (
            "MATCH (n) RETURN n, labels(n) AS labels",
            vec![
                vec![
                    GraphValue::Vertex(VId(1)),
                    GraphValue::List(vec![node_label].into()),
                ],
                vec![GraphValue::Vertex(VId(2)), GraphValue::List(vec![].into())],
            ],
        ),
        (
            "MATCH (a)-[r:R]->(b) RETURN type(r) AS type",
            vec![vec![GraphValue::Scalar(edge_type.clone())]; 2],
        ),
        (
            "MATCH (a)-[r:R]->(b) RETURN r.p AS p",
            vec![vec![integer(11)], vec![integer(12)]],
        ),
    ];
    for (text, expected) in cases {
        let pattern = PreparedGraphText::prepare_with_resolver(text, Catalog("p"))
            .unwrap()
            .bind_parameters(&GqlParameters::new())
            .unwrap();
        let rows = pattern
            .plan()
            .execute_with_element_properties_control(
                source.vertices.keys().copied(),
                source.edges.iter().copied(),
                |_, _| Ok::<_, GraphIntegerError>(true),
                |vid, key| Ok(source.vertex_values.get(&(vid, key))),
                |eid, key| Ok(source.edge_values.get(&(eid, key))),
                |vid| Ok(Some(if vid == VId(1) { &labels[..] } else { &[] })),
                |_| Ok(Some(&edge_type)),
                |_| Ok(()),
            )
            .unwrap();
        assert_eq!(
            rows.iter()
                .map(|row| row.values().to_vec())
                .collect::<Vec<_>>(),
            expected,
            "{text}"
        );
        if text == "MATCH (n) RETURN n, n.p AS p" {
            for identified in [false, true] {
                let rows = if identified {
                    pattern.plan().execute_with_identified_properties_control(
                        source.vertices.keys().copied(),
                        source.edges.iter().copied(),
                        |_, _| Ok::<_, GraphIntegerError>(true),
                        |vid, key| Ok(source.vertex_values.get(&(vid, key))),
                        |_| Ok(()),
                    )
                } else {
                    pattern.plan().execute_with_properties_control(
                        source.vertices.keys().copied(),
                        source
                            .edges
                            .iter()
                            .map(|&(_, src, rel, dst)| (src, rel, dst)),
                        |_, _| Ok::<_, GraphIntegerError>(true),
                        |vid, key| Ok(source.vertex_values.get(&(vid, key))),
                        |_| Ok(()),
                    )
                }
                .unwrap();
                assert_eq!(
                    rows.iter()
                        .map(|row| row.values().to_vec())
                        .collect::<Vec<_>>(),
                    expected,
                    "identified={identified}"
                );
            }
        }
    }
}

#[test]
fn maps_follow_optional_and_with_scope_without_exposing_private_columns() {
    let source = Fixture::new();
    let null = GraphValue::Scalar(CanonicalScalar::Null);
    assert_eq!(source.rows("MATCH (n) OPTIONAL MATCH (n)-[r:R]->(m) RETURN n, properties(m) AS m, properties(r) AS r ORDER BY n, r"), vec![
        vec![GraphValue::Vertex(VId(1)), map(&[]), map(&[("p", integer(11))])],
        vec![GraphValue::Vertex(VId(1)), map(&[]), map(&[("p", integer(12))])],
        vec![GraphValue::Vertex(VId(2)), null.clone(), null.clone()],
    ]);
    for text in [
        "MATCH (n) WITH n AS kept RETURN kept{.*, p: 9} AS m",
        "MATCH (n) WITH properties(n) AS kept RETURN kept{p: 9, .*} AS m",
        "MATCH (n) RETURN n{p: 9, .*} AS m",
    ] {
        assert_eq!(
            source.rows(text),
            vec![vec![map(&[("p", integer(9))])]; 2],
            "{text}"
        );
    }
    assert_eq!(
        source.rows("MATCH (n) RETURN properties(n).p AS p ORDER BY p"),
        vec![vec![integer(1)], vec![null.clone()]]
    );
    for projection in ["m{.*}", "m{}", "m{p: 1/0}"] {
        assert_eq!(
            source.rows(&format!("WITH NULL AS m RETURN {projection} AS result")),
            vec![vec![null.clone()]]
        );
    }
    let query = PreparedGraphSetText::prepare_with_parameter_types_and_resolver(
        "UNWIND [1] AS m RETURN m{} AS result",
        &[],
        &mut Catalog("p"),
    )
    .unwrap()
    .bind_parameters(&GqlParameters::new())
    .unwrap();
    assert!(
        query
            .execute_governed(
                policy(),
                |pattern, policy| source.execute(pattern, policy),
                || Ok::<_, ()>(())
            )
            .is_err()
    );
    assert!(
        PreparedGraphSetText::prepare_with_parameter_types_and_resolver(
            "MATCH (n) WITH 1 AS x RETURN properties(n) AS m",
            &[],
            &mut Catalog("p")
        )
        .is_err()
    );
}

#[test]
fn map_inputs_and_outputs_use_the_existing_aggregate_engine() {
    let source = Fixture::new();
    for (text, expected) in [
        (
            "MATCH (n) RETURN collect(n{.*, p: 9}) AS result",
            GraphValue::List(vec![map(&[("p", integer(9))]); 2].into_boxed_slice()),
        ),
        (
            "MATCH (n) RETURN properties({count: count(*)}) AS result",
            map(&[("count", integer(2))]),
        ),
    ] {
        let aggregate = PreparedGraphAggregateText::prepare_with_parameter_types_and_resolver(
            text,
            &[],
            Catalog("p"),
        )
        .unwrap_or_else(|error| panic!("{text}: {error:?}"))
        .bind_parameters(&GqlParameters::new())
        .unwrap();
        let rows = aggregate
            .execute_governed_with_element_maps(
                2,
                source.vertices.keys().copied(),
                [],
                |_, _| Ok::<_, ()>(true),
                |vid, key| Ok(source.vertex_values.get(&(vid, key))),
                |_, _| Ok(None),
                |_| Ok(None),
                |_| Ok(None),
                |vid| Ok(source.vertices.get(&vid)),
                |_| Ok(None),
                policy(),
                || Ok::<_, ()>(()),
            )
            .unwrap_or_else(|error| panic!("{text}: {error:?}"));
        assert_eq!(rows.value.len(), 1);
        assert_eq!(
            rows.value[0].values(),
            &[GraphAggregateValue::Value(expected)]
        );
    }
    assert_eq!(
        source.rows("MATCH (n) WITH n, count(*) AS c RETURN n{.*, count: c} AS m ORDER BY m"),
        vec![
            vec![map(&[("count", integer(1))])],
            vec![map(&[("count", integer(1)), ("p", integer(1))])],
        ]
    );
}

#[test]
fn property_names_bind_both_template_and_plan_identity_only_when_maps_are_read() {
    for text in [
        "MATCH (n) RETURN properties(n) AS m",
        "MATCH (n) RETURN n.p AS p",
    ] {
        let original = PreparedGraphText::prepare_with_resolver(text, Catalog("p")).unwrap();
        let renamed = PreparedGraphText::prepare_with_resolver(text, Catalog("renamed")).unwrap();
        let original_plan = original.bind_parameters(&GqlParameters::new()).unwrap();
        let renamed_plan = renamed.bind_parameters(&GqlParameters::new()).unwrap();
        if text.contains("properties") {
            assert_ne!(original.template_bytes(), renamed.template_bytes());
            assert_ne!(
                original_plan.canonical_bytes(),
                renamed_plan.canonical_bytes()
            );
        } else {
            assert_eq!(original.template_bytes(), renamed.template_bytes());
            assert_eq!(
                original_plan.canonical_bytes(),
                renamed_plan.canonical_bytes()
            );
        }
    }
}
