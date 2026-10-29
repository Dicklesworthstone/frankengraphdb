//! Complete element maps share native projection, scope and resource owners.

use fgdb_delta_types::catalog::{CatalogSymbol, SymbolCatalog};
use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::{
    GlaDirection, GraphColumn, GraphPathFunction, GraphPatternBuilder, GraphValue, GraphValueRow,
    PreparedGraphPattern,
};
use fgdb_gql::{
    GlaExecutionLimits, GqlParameters, GqlQueryError, GqlQueryExecution, GqlQueryPolicy,
    GraphAggregateValue, GraphElementMetadata, GraphElementMetadataError, GraphIntegerError,
    GraphIntegerErrorKind, GraphMutationPolicy, GraphSymbol, GraphSymbolKind, GraphSymbolResolver,
    PreparedGraphAggregateText, PreparedGraphMutationQueryText, PreparedGraphSetText,
    PreparedGraphText, ReverseSymbolCatalog,
};
use fgdb_types::{CanonicalPropertyValue, CanonicalScalar, EId, VId};
use std::cell::Cell;
use std::collections::BTreeMap;

const P: PropertyKeyId = PropertyKeyId(1);
const R: RelationId = RelationId(1);
type Edge = (EId, VId, RelationId, VId);

#[test]
fn metadata_row_domains_do_not_revive_shadowed_graph_bindings() {
    use fgdb_gql::GraphSetColumnType;

    fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        match (kind, name) {
            (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
            _ => None,
        }
    }

    for text in [
        "MATCH (n) WITH 0 AS n RETURN labels(n)",
        "MATCH (a)-[r:R]->(b) WITH 0 AS r RETURN type(r)",
        "MATCH (n) WITH [0] AS n RETURN labels(n)",
        "MATCH (a)-[r:R]->(b) WITH r AS n RETURN labels(n)",
        "MATCH (n) WITH n AS r RETURN type(r)",
    ] {
        let mut calls = 0;
        let result = PreparedGraphSetText::prepare(text, |kind, name: &str| {
            calls += 1;
            symbols(kind, name)
        });
        assert!(result.is_err(), "{text}");
        assert_eq!(calls, 0, "domain refusal precedes catalog access: {text}");
    }
    for (text, expected) in [
        (
            "MATCH (n) WITH n AS kept RETURN labels(kept)",
            vec![GraphSetColumnType::List],
        ),
        (
            "MATCH (a)-[r:R]->(b) WITH r AS kept RETURN type(kept)",
            vec![GraphSetColumnType::Scalar],
        ),
        (
            "MATCH (n) WITH NULL AS n RETURN labels(n), type(n)",
            vec![GraphSetColumnType::List, GraphSetColumnType::Scalar],
        ),
        (
            "MATCH (n) WITH collect(n) AS ns UNWIND ns AS n RETURN labels(n)",
            vec![GraphSetColumnType::List],
        ),
        (
            "MATCH (n) WITH 0 AS n RETURN [n IN [NULL] | labels(n)] AS names",
            vec![GraphSetColumnType::List],
        ),
    ] {
        let prepared = PreparedGraphSetText::prepare(text, symbols).unwrap();
        assert_eq!(prepared.column_types(), expected, "{text}");
        assert_eq!(
            prepared
                .bind_parameters(&GqlParameters::new())
                .unwrap()
                .column_types(),
            expected,
            "{text}"
        );
    }
}

enum FixtureRead<'a> {
    Property(PropertyKeyId),
    Metadata {
        kind: GraphElementMetadata,
        catalog: &'a ReverseSymbolCatalog,
    },
}

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
            (GraphSymbolKind::Relation, "S") => Some(GraphSymbol::Relation(RelationId(2))),
            (GraphSymbolKind::Label, "L") => Some(GraphSymbol::Label(LabelId(1))),
            (GraphSymbolKind::Label, "M") => Some(GraphSymbol::Label(LabelId(2))),
            _ => None,
        }
    }
    fn reverse_catalog(&self) -> Option<ReverseSymbolCatalog> {
        let mut catalog = ReverseSymbolCatalog::new();
        catalog.insert_property(P, self.0);
        catalog.insert_relation(R, "R");
        catalog.insert_relation(RelationId(2), "S");
        catalog.insert_label(LabelId(1), "L");
        catalog.insert_label(LabelId(2), "M");
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
    labels: BTreeMap<VId, Vec<LabelId>>,
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
            labels: BTreeMap::new(),
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
        let mut labels = BTreeMap::new();
        if pattern.plan().projects_labels() {
            for (&vertex, ids) in &self.labels {
                let catalog = pattern
                    .plan()
                    .reverse_catalog
                    .as_deref()
                    .ok_or(GqlQueryError::Source(()))?;
                let names = ids
                    .iter()
                    .map(|id| {
                        catalog
                            .labels
                            .get(id)
                            .and_then(|name| CanonicalScalar::ucs_basic_text(name).ok())
                            .map(GraphValue::Scalar)
                            .ok_or(GqlQueryError::Source(()))
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                labels.insert(vertex, names);
            }
        }
        pattern.plan().execute_governed_with_element_maps(
            (self.vertices.len() + self.edges.len()) as u64,
            self.vertices.keys().copied(),
            self.edges.iter().copied(),
            |_, _| Ok(true),
            |vid, key| Ok(self.vertex_values.get(&(vid, key))),
            |eid, key| Ok(self.edge_values.get(&(eid, key))),
            |vid| Ok(labels.get(&vid).map(Vec::as_slice)),
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

    fn read(
        &self,
        element: ElementId,
        request: FixtureRead<'_>,
    ) -> Result<Option<CanonicalPropertyValue>, ()> {
        let (properties, labels, relation) = match element {
            ElementId::Vertex(id) => {
                if !self.vertices.contains_key(&id) {
                    return Ok(None);
                }
                (
                    self.vertex_values
                        .iter()
                        .filter_map(|(&(vertex, key), value)| {
                            (vertex == id).then_some((key, value))
                        })
                        .collect::<Vec<_>>(),
                    self.labels.get(&id).map(Vec::as_slice).unwrap_or(&[]),
                    None,
                )
            }
            ElementId::Edge(id) => {
                let Some((_, _, relation, _)) = self.edges.iter().find(|(edge, ..)| *edge == id)
                else {
                    return Ok(None);
                };
                (
                    self.edge_values
                        .iter()
                        .filter_map(|(&(edge, key), value)| (edge == id).then_some((key, value)))
                        .collect::<Vec<_>>(),
                    &[][..],
                    Some(*relation),
                )
            }
        };
        let FixtureRead::Metadata { kind, catalog } = request else {
            let FixtureRead::Property(key) = request else {
                unreachable!()
            };
            return Ok(properties
                .into_iter()
                .find(|(candidate, _)| *candidate == key)
                .map(|(_, value)| value.clone()));
        };
        let text = |name: &str| {
            CanonicalScalar::ucs_basic_text(name)
                .map(GraphValue::Scalar)
                .map_err(|_| ())
        };
        let value = match kind {
            GraphElementMetadata::Labels => GraphValue::List(
                labels
                    .iter()
                    .map(|id| text(catalog.labels.get(id).ok_or(())?))
                    .collect::<Result<Vec<_>, _>>()?
                    .into_boxed_slice(),
            ),
            GraphElementMetadata::Type => {
                text(catalog.relations.get(&relation.ok_or(())?).ok_or(())?)?
            }
            GraphElementMetadata::Keys => GraphValue::List(
                properties
                    .iter()
                    .map(|(key, _)| text(catalog.properties.get(key).ok_or(())?))
                    .collect::<Result<Vec<_>, ()>>()?
                    .into_boxed_slice(),
            ),
            GraphElementMetadata::Properties => GraphValue::map(
                properties
                    .into_iter()
                    .map(|(key, value)| {
                        Ok((
                            catalog.properties.get(&key).ok_or(())?.as_str().into(),
                            GraphValue::from_property_value(value).ok_or(())?,
                        ))
                    })
                    .collect::<Result<Vec<_>, ()>>()?,
            )
            .ok_or(())?,
        };
        value.to_property_value().map(Some).ok_or(())
    }

    fn reader<'a>(
        &'a self,
        reads: Option<&'a Cell<usize>>,
    ) -> impl fgdb_gql::GraphElementReader<()> + 'a {
        fgdb_gql::with_element_metadata(
            move |element, key| {
                if let Some(reads) = reads {
                    reads.set(reads.get() + 1);
                }
                self.read(element, FixtureRead::Property(key))
            },
            move |element, kind, catalog: &ReverseSymbolCatalog,
                  control: &mut dyn FnMut(fgdb_gql::GlaExecutionEvent) -> Result<(), ()>| {
                if let Some(reads) = reads {
                    reads.set(reads.get() + 1);
                }
                let Some(raw) = self.read(element, FixtureRead::Metadata { kind, catalog })
                    .map_err(GraphElementMetadataError::Source)? else {
                    return Ok(None);
                };
                let mut interrupted = false;
                let value = GraphValue::admit_property_value(&raw, &mut |event| {
                    control(event).map_err(|()| {
                        interrupted = true;
                        GraphIntegerError { instruction: 0, kind: GraphIntegerErrorKind::StoredValueBounds }
                    })
                });
                value.map(Some).map_err(|_| {
                    if interrupted { GraphElementMetadataError::Interrupted }
                    else { GraphElementMetadataError::Source(()) }
                })
            },
        )
    }

    fn dynamic(
        &self,
        text: &str,
        reads: &Cell<usize>,
    ) -> Result<
        Vec<Vec<GraphValue>>,
        fgdb_gql::GqlQueryError<fgdb_gql::GraphSetExecutionError<()>, ()>,
    > {
        let query = PreparedGraphSetText::prepare_with_parameter_types_and_resolver(
            text,
            &[],
            &mut Catalog("p"),
        )
        .unwrap_or_else(|error| panic!("{text}: {error:?}"))
        .bind_parameters(&GqlParameters::new())
        .unwrap();
        query
            .execute_governed_with_elements(
                policy(),
                |pattern, policy| self.execute(pattern, policy),
                |_, _, _| unreachable!("fixture has no procedure"),
                self.reader(Some(reads)),
                || Ok::<_, ()>(()),
            )
            .map(|result| {
                result
                    .value
                    .into_iter()
                    .map(|row| row.values().to_vec())
                    .collect()
            })
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

fn metadata_fixture() -> Fixture {
    let mut source = Fixture::new();
    source.labels = BTreeMap::from([
        (VId(1), vec![LabelId(1), LabelId(2)]),
        (VId(2), vec![LabelId(2)]),
    ]);
    source.edges[1].2 = RelationId(2);
    source.edge_types = BTreeMap::from([
        (EId(11), CanonicalScalar::ucs_basic_text("R").unwrap()),
        (EId(12), CanonicalScalar::ucs_basic_text("S").unwrap()),
    ]);
    source
}

#[test]
fn dynamic_metadata_matches_bound_elements_across_lists_rows_and_paths() {
    let source = metadata_fixture();
    let reads = Cell::new(0);
    let bound = source
        .rows("MATCH (n) RETURN labels(n) AS labels, keys(n) AS keys, properties(n) AS props");
    assert_eq!(
        source
            .dynamic(
                "MATCH (n) WITH collect(n) AS ns UNWIND ns AS x \
             RETURN labels(x) AS labels, keys(x) AS keys, properties(x) AS props",
                &reads,
            )
            .unwrap(),
        bound,
    );
    assert_eq!(reads.get(), 6);
    let text = |name| GraphValue::Scalar(CanonicalScalar::ucs_basic_text(name).unwrap());
    let list = |values: Vec<_>| GraphValue::List(values.into_boxed_slice());
    assert_eq!(
        source
            .dynamic(
                "MATCH (n) WITH collect(n) AS ns \
             RETURN labels(ns[0]) AS ls, properties(ns[0]).p AS p, \
             [x IN [ns[1], ns[0], ns[1]] | keys(x)] AS ks",
                &reads,
            )
            .unwrap(),
        vec![vec![
            list(vec![text("L"), text("M")]),
            integer(1),
            list(vec![list(vec![]), list(vec![text("p")]), list(vec![])])
        ]],
    );
    for relation in ["R", "S"] {
        let dynamic = format!(
            "MATCH p = (a)-[r:{relation}]->(b) RETURN \
             [n IN nodes(p) | labels(n)] AS ls, \
             [e IN relationships(p) | type(e)] AS ts, \
             [e IN relationships(p) | properties(e)] AS ps"
        );
        let bound = format!(
            "MATCH (a)-[r:{relation}]->(b) RETURN [labels(a), labels(b)] AS ls, \
             [type(r)] AS ts, [properties(r)] AS ps"
        );
        assert_eq!(
            source.dynamic(&dynamic, &reads).unwrap(),
            source.rows(&bound),
            "{relation}"
        );
    }
    assert_eq!(
        source
            .dynamic(
                "MATCH ()-[r]->() WITH collect(r) AS rs UNWIND rs AS x RETURN type(x) AS t",
                &reads,
            )
            .unwrap(),
        vec![vec![text("R")], vec![text("S")]],
    );
    assert_eq!(
        source
            .dynamic(
                "MATCH (n) WITH collect(n) AS ns UNWIND ns AS n \
             WITH n WHERE 'L' IN labels(n) \
             RETURN labels(n) + ['Extra'] AS ls, properties(n).p + 2 AS p",
                &reads,
            )
            .unwrap(),
        vec![vec![
            list(vec![text("L"), text("M"), text("Extra")]),
            integer(3)
        ]],
    );
}

#[test]
fn dynamic_metadata_keeps_null_map_wrong_type_and_lazy_branch_semantics() {
    let source = metadata_fixture();
    let reads = Cell::new(0);
    let null = GraphValue::Scalar(CanonicalScalar::Null);
    assert_eq!(
        source
            .dynamic(
                "RETURN labels(NULL) AS l, type(NULL) AS t, properties(NULL) AS p, keys(NULL) AS k",
                &reads,
            )
            .unwrap(),
        vec![vec![null.clone(); 4]],
    );
    assert_eq!(reads.get(), 0);
    assert_eq!(
        source
            .dynamic(
                "RETURN properties({z: 3, a: 1}) AS p, keys({z: 3, a: 1}) AS k",
                &reads,
            )
            .unwrap(),
        vec![vec![
            map(&[("a", integer(1)), ("z", integer(3))]),
            GraphValue::List(
                vec![
                    GraphValue::Scalar(CanonicalScalar::ucs_basic_text("a").unwrap()),
                    GraphValue::Scalar(CanonicalScalar::ucs_basic_text("z").unwrap()),
                ]
                .into_boxed_slice()
            )
        ]],
    );
    assert_eq!(reads.get(), 0);
    assert_eq!(
        source
            .dynamic(
                "MATCH (n) WITH collect(n) AS ns RETURN \
             [x IN [ns[0], NULL, ns[0]] | properties(x)] AS p",
                &reads,
            )
            .unwrap(),
        vec![vec![GraphValue::List(
            vec![
                source.vertices[&VId(1)].clone(),
                null,
                source.vertices[&VId(1)].clone()
            ]
            .into_boxed_slice()
        )]],
    );
    assert_eq!(reads.get(), 2);
    for statement in [
        "RETURN labels(42) AS bad",
        "RETURN type({}) AS bad",
        "RETURN properties([1]) AS bad",
        "MATCH (n) WITH collect(n) AS ns RETURN type(ns[0]) AS bad",
        "MATCH ()-[r:R]->() WITH collect(r) AS rs RETURN labels(rs[0]) AS bad",
    ] {
        reads.set(0);
        assert!(
            matches!(
                source.dynamic(statement, &reads),
                Err(GqlQueryError::Source(
                    fgdb_gql::GraphSetExecutionError::Projection { .. }
                ))
            ),
            "{statement}"
        );
        assert_eq!(
            reads.get(),
            0,
            "wrong domains must refuse before a metadata lookup: {statement}"
        );
    }
    reads.set(0);
    assert_eq!(
        source
            .dynamic(
                "MATCH (n) WITH collect(n) AS ns RETURN \
             CASE WHEN false THEN labels(ns[0]) ELSE [] END AS skipped, \
             CASE WHEN true THEN properties({a: 1}) ELSE properties(ns[0]) END AS selected, \
             CASE WHEN true THEN 'ok' ELSE type(ns[0]) END AS skipped_type, \
             coalesce('ok',type(ns[0])) AS skipped_coalesce",
                &reads,
            )
            .unwrap(),
        vec![vec![
            GraphValue::List(Box::new([])),
            map(&[("a", integer(1))]),
            GraphValue::Scalar(CanonicalScalar::ucs_basic_text("ok").unwrap()),
            GraphValue::Scalar(CanonicalScalar::ucs_basic_text("ok").unwrap()),
        ]],
    );
    assert_eq!(reads.get(), 0);
    for statement in [
        "RETURN labels(missing) AS bad",
        "RETURN type(missing) AS bad",
        "RETURN properties(missing) AS bad",
        "RETURN keys(missing) AS bad",
    ] {
        assert!(
            PreparedGraphSetText::prepare_with_parameter_types_and_resolver(
                statement,
                &[],
                &mut Catalog("p"),
            )
            .is_err(),
            "{statement}"
        );
    }
}

#[test]
fn dynamic_metadata_catalog_is_frozen_in_template_and_plan_identity() {
    let source = metadata_fixture();
    let statement = "MATCH (n) WITH collect(n) AS ns RETURN properties(ns[0]) AS p";
    let mut catalog = Catalog("original");
    let prepared = PreparedGraphSetText::prepare_with_parameter_types_and_resolver(
        statement,
        &[],
        &mut catalog,
    )
    .unwrap();
    catalog.0 = "changed";
    let changed = PreparedGraphSetText::prepare_with_parameter_types_and_resolver(
        statement,
        &[],
        &mut catalog,
    )
    .unwrap();
    assert_ne!(
        prepared.canonical_template_bytes(),
        changed.canonical_template_bytes()
    );
    let query = prepared.bind_parameters(&GqlParameters::new()).unwrap();
    let repeated = prepared.bind_parameters(&GqlParameters::new()).unwrap();
    assert_eq!(query.canonical_bytes(), repeated.canonical_bytes());
    assert_ne!(
        query.canonical_bytes(),
        changed
            .bind_parameters(&GqlParameters::new())
            .unwrap()
            .canonical_bytes()
    );
    let result = query
        .execute_governed_with_elements(
            policy(),
            |pattern, policy| source.execute(pattern, policy),
            |_, _, _| unreachable!(),
            source.reader(None),
            || Ok::<_, ()>(()),
        )
        .unwrap();
    assert_eq!(
        result.value[0].values(),
        &[map(&[("original", integer(1))])]
    );
}

#[test]
fn aggregate_output_metadata_reads_collected_identities_at_the_same_source() {
    let source = metadata_fixture();
    let statement = "MATCH (n) RETURN [x IN collect(n) | labels(x)] AS ls, \
        [x IN collect(n) | properties(x)] AS ps";
    let query = PreparedGraphAggregateText::prepare_with_parameter_types_and_resolver(
        statement,
        &[],
        Catalog("p"),
    )
    .unwrap()
    .bind_parameters(&GqlParameters::new())
    .unwrap();
    let reads = Cell::new(0);
    let result = query
        .execute_governed_with_element_maps_and_reads(
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
            source.reader(Some(&reads)),
            policy(),
            || Ok::<_, ()>(()),
        )
        .unwrap();
    let expected = source
        .dynamic(
            "MATCH (n) WITH collect(n) AS ns RETURN [x IN ns | labels(x)] AS ls, \
         [x IN ns | properties(x)] AS ps",
            &Cell::new(0),
        )
        .unwrap();
    assert_eq!(
        result.value[0].values(),
        expected[0]
            .iter()
            .cloned()
            .map(GraphAggregateValue::Value)
            .collect::<Vec<_>>()
    );
    assert_eq!(reads.get(), 4);
}

#[test]
fn mutation_metadata_reads_final_intents_in_return_aggregates_and_continuations() {
    let source = metadata_fixture();
    let text = |name| GraphValue::Scalar(CanonicalScalar::ucs_basic_text(name).unwrap());
    let list = |values: Vec<_>| GraphValue::List(values.into_boxed_slice());
    let labels = vec![list(vec![text("L")]), list(vec![])];
    let properties = vec![map(&[("p", integer(7))]); 2];
    for (tail, expected) in [
        (
            "RETURN [x IN [n] | labels(x)] AS ls, [x IN [n] | properties(x)] AS ps",
            (0..2)
                .map(|at| {
                    vec![
                        list(vec![labels[at].clone()]),
                        list(vec![properties[at].clone()]),
                    ]
                })
                .collect::<Vec<_>>(),
        ),
        (
            "RETURN [x IN collect(n) | labels(x)] AS ls, [x IN collect(n) | properties(x)] AS ps",
            vec![vec![list(labels.clone()), list(properties.clone())]],
        ),
        (
            "WITH collect(n) AS ns RETURN [x IN ns | labels(x)] AS ls, [x IN ns | properties(x)] AS ps",
            vec![vec![list(labels.clone()), list(properties.clone())]],
        ),
    ] {
        let statement = format!("MATCH (n) SET n.p=7 REMOVE n:M {tail}");
        let query = PreparedGraphMutationQueryText::prepare(&statement, R, Catalog("p"))
            .unwrap_or_else(|error| panic!("{statement}: {error:?}"))
            .bind_parameters(&GqlParameters::new())
            .unwrap();
        let result = query
            .execute_governed_reading(
                GraphMutationPolicy::new(policy(), 100),
                |pattern, policy| source.execute(pattern, policy),
                source.reader(None),
                || Ok::<_, ()>(()),
            )
            .unwrap_or_else(|error| panic!("{statement}: {error:?}"));
        assert_eq!(
            result
                .returning()
                .value
                .iter()
                .map(|row| row.values().to_vec())
                .collect::<Vec<_>>(),
            expected,
            "{statement}"
        );
    }
    // The host was a frozen pre-statement snapshot throughout each execution.
    assert_eq!(source.vertices[&VId(1)], map(&[("p", integer(1))]));
    assert_eq!(source.labels[&VId(2)], [LabelId(2)]);
}
