//! Post-statement maps use final native proposals and the frozen MATCH bag.

use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GraphValue, GraphValueRow, PreparedGraphPattern};
use fgdb_gql::{
    GlaExecutionLimits, GqlParameters, GqlQueryError, GqlQueryExecution, GqlQueryPolicy,
    GraphMutationAction, GraphMutationBinding, GraphMutationError, GraphMutationPolicy,
    GraphMutationQueryBatch, GraphMutationQueryBuildError, GraphMutationQueryError,
    GraphMutationTextErrorKind, GraphSetProjection, GraphSetQuantifier, GraphSetValue, GraphSymbol,
    GraphSymbolKind, GraphSymbolResolver, PreparedGraphMutation, PreparedGraphMutationQuery,
    PreparedGraphMutationQueryText, PreparedGraphText, ReverseSymbolCatalog,
};
use fgdb_types::{CanonicalPropertyValue, CanonicalScalar, EId, VId};
use std::cell::Cell;
use std::collections::BTreeMap;

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);
const OLD: PropertyKeyId = PropertyKeyId(3);
const KEEP: PropertyKeyId = PropertyKeyId(4);
type ResultOf = Result<GraphMutationQueryBatch, GqlQueryError<GraphMutationQueryError<()>, ()>>;

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
        (GraphSymbolKind::Property, "old") => Some(GraphSymbol::Property(OLD)),
        (GraphSymbolKind::Property, "keep") => Some(GraphSymbol::Property(KEEP)),
        _ => None,
    }
}

#[derive(Clone, Copy)]
struct Catalog(Option<&'static str>);
impl GraphSymbolResolver for Catalog {
    fn resolve_symbol(&mut self, kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        symbols(kind, name)
    }
    fn reverse_catalog(&self) -> Option<ReverseSymbolCatalog> {
        let mut catalog = ReverseSymbolCatalog::new();
        for (key, name) in [(P, "p"), (OLD, "old"), (KEEP, "keep")] {
            catalog.insert_property(key, name);
        }
        if let Some(name) = self.0 {
            catalog.insert_property(Q, name);
        }
        Some(catalog)
    }
}

fn int(value: i64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(value))
}
fn null() -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Null)
}
fn map(entries: Vec<(&str, GraphValue)>) -> GraphValue {
    GraphValue::map(
        entries
            .into_iter()
            .map(|(key, value)| (key.into(), value))
            .collect(),
    )
    .unwrap()
}
fn list(values: Vec<GraphValue>) -> GraphValue {
    GraphValue::List(values.into_boxed_slice())
}
fn text(value: &str) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::ucs_basic_text(value).unwrap())
}
fn policy() -> GraphMutationPolicy {
    GraphMutationPolicy::new(
        GqlQueryPolicy::new(10_000, 10_000, 1_000_000, 1_000_000),
        1_000,
    )
}
fn prepare(statement: &str, catalog: impl GraphSymbolResolver) -> PreparedGraphMutationQuery {
    PreparedGraphMutationQueryText::prepare(statement, R, catalog)
        .unwrap_or_else(|error| panic!("{statement}: {error:?}"))
        .bind_parameters(&GqlParameters::new())
        .unwrap_or_else(|error| panic!("{statement}: {error:?}"))
}
fn rows(batch: &GraphMutationQueryBatch) -> Vec<Vec<GraphValue>> {
    batch
        .returning()
        .value
        .iter()
        .map(|row| row.values().to_vec())
        .collect()
}
fn property(value: Option<&GraphValue>, key: PropertyKeyId) -> Option<&CanonicalScalar> {
    let name = match key {
        P => "p",
        Q => "q",
        OLD => "old",
        KEEP => "keep",
        _ => return None,
    };
    let (keys, values) = value?.as_map()?;
    values[keys.binary_search_by(|key| key.as_ref().cmp(name)).ok()?].as_scalar()
}
/// The stored property values `property` reads from the current maps.
fn stored<Id: Copy + Ord>(
    maps: &BTreeMap<Id, GraphValue>,
) -> BTreeMap<(Id, PropertyKeyId), CanonicalPropertyValue> {
    let mut stored = BTreeMap::new();
    for (&id, value) in maps {
        for key in [P, Q, OLD, KEEP] {
            if let Some(scalar) = property(Some(value), key) {
                stored.insert((id, key), CanonicalPropertyValue::from(scalar.clone()));
            }
        }
    }
    stored
}

struct Fixture {
    vertices: BTreeMap<VId, GraphValue>,
    edges: Vec<(EId, VId, RelationId, VId)>,
    edge_maps: BTreeMap<EId, GraphValue>,
}
impl Fixture {
    fn new() -> Self {
        Self {
            vertices: BTreeMap::from([
                (
                    VId(1),
                    map(vec![("p", int(10)), ("old", int(3)), ("keep", int(7))]),
                ),
                (VId(2), map(vec![("p", int(20))])),
            ]),
            edges: vec![(EId(11), VId(1), R, VId(2)), (EId(12), VId(1), R, VId(2))],
            edge_maps: BTreeMap::from([
                (EId(11), map(vec![("p", int(11)), ("old", int(8))])),
                (EId(12), map(vec![("p", int(12)), ("old", int(9))])),
            ]),
        }
    }
    fn source(
        &self,
        pattern: &PreparedGraphPattern<GraphValueRow>,
        policy: GqlQueryPolicy,
    ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<(), ()>> {
        let vertex_properties = stored(&self.vertices);
        let edge_properties = stored(&self.edge_maps);
        pattern.plan().execute_governed_with_element_maps(
            (self.vertices.len() + self.edges.len()) as u64,
            self.vertices.keys().copied(),
            self.edges.iter().copied(),
            |_, _| Ok(true),
            |vid, key| Ok(vertex_properties.get(&(vid, key))),
            |eid, key| Ok(edge_properties.get(&(eid, key))),
            |_| Ok(None),
            |_| Ok(None),
            |vid| Ok(self.vertices.get(&vid)),
            |eid| Ok(self.edge_maps.get(&eid)),
            policy,
            || Ok(()),
        )
    }
    fn execute(&self, query: &PreparedGraphMutationQuery, policy: GraphMutationPolicy) -> ResultOf {
        let calls = Cell::new(0);
        let result = query.execute_governed(
            policy,
            |pattern, budget| {
                calls.set(calls.get() + 1);
                self.source(pattern, budget)
            },
            || Ok(()),
        );
        assert_eq!(calls.get(), 1, "RETURN must use the original source once");
        result
    }
}

#[test]
fn vertex_and_edge_maps_merge_all_final_assignments_and_keep_occurrences() {
    let source = Fixture::new();
    let query = prepare(
        "MATCH (a)-[r:R]->(b) SET a.p=b.p,a.q=NULL,r.q=r.p+1 \
         REMOVE a.old,r.old,r.p RETURN properties(a) AS a,properties(b) AS b,\
         properties(r) AS edge,keys(a) AS names,a{.*,p:a.p+1} AS projected ORDER BY edge",
        Catalog(Some("q")),
    );
    let batch = source.execute(&query, policy()).unwrap();
    let a = map(vec![("keep", int(7)), ("p", int(20)), ("q", null())]);
    let projected = map(vec![("keep", int(7)), ("p", int(21)), ("q", null())]);
    assert_eq!(
        rows(&batch),
        [12, 13]
            .into_iter()
            .map(|q| vec![
                a.clone(),
                map(vec![("p", int(20))]),
                map(vec![("q", int(q))]),
                list(vec![text("keep"), text("p"), text("q")]),
                projected.clone(),
            ])
            .collect::<Vec<_>>()
    );
    assert_eq!(batch.mutation().stats().selection.result_rows, 2);
    assert_eq!(batch.mutation().stats().effects, 9);
    assert_eq!(
        source.vertices[&VId(1)],
        map(vec![("p", int(10)), ("old", int(3)), ("keep", int(7))])
    );
}

#[test]
fn aliases_of_one_element_and_aggregates_read_the_same_final_map() {
    let mut source = Fixture::new();
    source.edges = vec![(EId(11), VId(1), R, VId(1)), (EId(12), VId(1), R, VId(1))];
    let statement = "MATCH (a)-[r:R]->(b) SET a.p=30,b.q=40 REMOVE a.old \
                     RETURN properties(a) AS a,b{.*} AS b ORDER BY a";
    let batch = source
        .execute(&prepare(statement, Catalog(Some("q"))), policy())
        .unwrap();
    let expected = map(vec![("keep", int(7)), ("p", int(30)), ("q", int(40))]);
    assert_eq!(
        rows(&batch),
        vec![vec![expected.clone(), expected.clone()]; 2]
    );
    assert_eq!(batch.mutation().stats().effects, 3);
    let grouped = prepare(
        "MATCH (a)-[r:R]->(b) SET a.p=30,b.q=40 REMOVE a.old \
         RETURN properties(a) AS node,count(*) AS copies,collect(b{.*}) AS maps",
        Catalog(Some("q")),
    );
    assert_eq!(
        rows(&source.execute(&grouped, policy()).unwrap()),
        vec![vec![
            expected.clone(),
            int(2),
            list(vec![expected.clone(), expected]),
        ]]
    );
}

#[test]
fn nullable_maps_skip_override_evaluation_and_selective_maps_need_no_catalog() {
    let mut source = Fixture::new();
    source.vertices.retain(|vid, _| *vid == VId(1));
    source.edges.clear();
    let query = prepare(
        "MATCH (n) OPTIONAL MATCH (n)-[r:R]->(m) SET m.p=7,r.q=8 \
         RETURN properties(m) AS a,keys(r) AS b,m{.*,q:1/0} AS c,\
         m{.p,q:1/0} AS d,properties(NULL) AS e,properties({q:1}) AS f",
        Catalog(Some("q")),
    );
    let batch = source.execute(&query, policy()).unwrap();
    assert_eq!(
        rows(&batch),
        vec![vec![
            null(),
            null(),
            null(),
            null(),
            null(),
            map(vec![("q", int(1))])
        ]]
    );
    assert_eq!(batch.mutation().stats().effects, 0);
    let selective = prepare(
        "MATCH (n) SET n.p=5 RETURN n{.p,answer:n.p+1} AS m",
        symbols,
    );
    assert!(
        !selective
            .mutation()
            .selection()
            .expect("a MATCH mutation reads a graph selection")
            .plan()
            .projects_property_maps()
    );
    assert_eq!(
        rows(&source.execute(&selective, policy()).unwrap()),
        vec![vec![map(vec![("p", int(5)), ("answer", int(6))])]]
    );
}

#[test]
fn maps_share_the_whole_operation_budget_and_limit_zero_still_materializes_them() {
    let mut source = Fixture::new();
    source
        .vertices
        .insert(VId(1), map(vec![("keep", text(&"payload".repeat(1_000)))]));
    for suffix in ["", " LIMIT 0"] {
        let query = prepare(
            &format!("MATCH (n) SET n.q=2 RETURN properties(n) AS m{suffix}"),
            Catalog(Some("q")),
        );
        let complete = source.execute(&query, policy()).unwrap();
        assert_eq!(complete.mutation().stats().effects, 2);
        let stats = complete.returning().evaluator;
        let mut exact = policy();
        exact.query.evaluator = GlaExecutionLimits::new(stats.work_units, stats.scratch_entries);
        assert_eq!(
            rows(&source.execute(&query, exact).unwrap()),
            rows(&complete)
        );
        for (work, scratch) in [
            (stats.work_units - 1, stats.scratch_entries),
            (stats.work_units, stats.scratch_entries - 1),
        ] {
            let mut bounded = policy();
            bounded.query.evaluator = GlaExecutionLimits::new(work, scratch);
            assert!(matches!(
                source.execute(&query, bounded),
                Err(GqlQueryError::Evaluator(_))
            ));
        }
    }
}

#[test]
fn names_of_new_properties_are_required_and_identity_binds_the_full_catalog() {
    let source = Fixture::new();
    let statement = "MATCH (n) SET n.q=2 RETURN properties(n) AS m";
    let normal = prepare(statement, Catalog(Some("q")));
    let renamed = prepare(statement, Catalog(Some("new-name")));
    assert_ne!(normal.canonical_bytes(), renamed.canonical_bytes());
    let missing = prepare(statement, Catalog(None));
    assert!(matches!(
        source.execute(&missing, policy()),
        Err(GqlQueryError::Source(GraphMutationQueryError::Mutation(
            GraphMutationError::UnmappedProperty { .. }
        )))
    ));
    let duplicate =
        PreparedGraphMutationQueryText::prepare(statement, R, Catalog(Some("p"))).unwrap();
    assert!(matches!(
        duplicate
            .bind_parameters(&GqlParameters::new())
            .unwrap_err()
            .kind,
        GraphMutationTextErrorKind::ReturnBuild(GraphMutationQueryBuildError::PropertyMapCatalog)
    ));
    let scalar = "MATCH (n) SET n.q=2 RETURN n.q AS q";
    assert_eq!(
        prepare(scalar, Catalog(Some("q"))).canonical_bytes(),
        prepare(scalar, Catalog(Some("new-name"))).canonical_bytes()
    );
}

#[test]
fn deleted_vertex_maps_and_cascaded_edge_maps_never_return_frozen_properties() {
    let source = Fixture::new();
    let deleted = prepare(
        "MATCH (n) DETACH DELETE n RETURN properties(n) AS m",
        Catalog(Some("q")),
    );
    assert!(matches!(
        source.execute(&deleted, policy()),
        Err(GqlQueryError::Source(GraphMutationQueryError::Mutation(
            GraphMutationError::DeletedElementRead { .. }
        )))
    ));
    for expression in ["properties(r)", "keys(r)", "r{.*}", "r{.p}"] {
        let statement = format!("MATCH (a)-[r:R]->(b) DETACH DELETE a RETURN {expression} AS m");
        let reads = Cell::new(0);
        assert!(
            PreparedGraphMutationQueryText::prepare(&statement, R, |kind, name: &str| {
                reads.set(reads.get() + 1);
                symbols(kind, name)
            })
            .is_err()
        );
        assert_eq!(
            reads.get(),
            0,
            "edge cascade refusal must precede catalog access"
        );
    }
    let selection = PreparedGraphText::prepare_with_resolver(
        "MATCH (a)-[r:R]->(b) RETURN a,r,properties(r) AS m",
        Catalog(Some("q")),
    )
    .unwrap()
    .bind_parameters(&GqlParameters::new())
    .unwrap();
    let mutation = PreparedGraphMutation::prepare(
        selection,
        R,
        vec![GraphMutationAction::DetachDelete { target: 0 }],
    )
    .unwrap();
    assert!(matches!(
        PreparedGraphMutationQuery::prepare(
            mutation,
            vec![GraphMutationBinding::Properties {
                target: 1,
                current: 2
            }],
            vec![GraphSetProjection::new("m", GraphSetValue::Column(0))],
            GraphSetQuantifier::All,
        ),
        Err(GraphMutationQueryBuildError::Binding { binding: 0 })
    ));
}

mod dynamic_metadata {
    use super::*;
    use fgdb_delta_types::{ElementId, LabelId};
    use fgdb_gql::{
        GlaExecutionEvent, GraphElementMetadata, GraphElementMetadataError, GraphElementReader,
        GraphMutationIntent, with_element_metadata,
    };

    #[derive(Clone, Copy)]
    struct MetadataCatalog;
    impl GraphSymbolResolver for MetadataCatalog {
        fn resolve_symbol(&mut self, kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
            match (kind, name) {
                (GraphSymbolKind::Label, "Before") => Some(GraphSymbol::Label(LabelId(1))),
                (GraphSymbolKind::Label, "After") => Some(GraphSymbol::Label(LabelId(2))),
                _ => symbols(kind, name),
            }
        }
        fn reverse_catalog(&self) -> Option<ReverseSymbolCatalog> {
            let mut catalog = Catalog(Some("q")).reverse_catalog().unwrap();
            catalog.insert_label(LabelId(1), "Before");
            catalog.insert_label(LabelId(2), "After");
            catalog.insert_relation(R, "R");
            Some(catalog)
        }
    }

    fn reader<'a>(
        fixture: &'a Fixture,
        metadata_calls: &'a Cell<usize>,
        hidden: bool,
    ) -> impl GraphElementReader<()> + 'a {
        let labels = list(vec![text("Before")]);
        let relation = text("R");
        with_element_metadata(
            move |element, key| {
                let map = match element {
                    ElementId::Vertex(vertex) => fixture.vertices.get(&vertex),
                    ElementId::Edge(edge) => fixture.edge_maps.get(&edge),
                };
                Ok(property(map, key)
                    .cloned()
                    .map(CanonicalPropertyValue::from))
            },
            move |element, kind, _catalog, control| {
                metadata_calls.set(metadata_calls.get() + 1);
                let map = match element {
                    ElementId::Vertex(vertex) => fixture.vertices.get(&vertex),
                    ElementId::Edge(edge) => fixture.edge_maps.get(&edge),
                };
                if hidden || map.is_none() {
                    return Ok(None);
                }
                let mut charge =
                    |event| control(event).map_err(|()| GraphElementMetadataError::Interrupted);
                let map = map.unwrap();
                if kind == GraphElementMetadata::Keys {
                    charge(GlaExecutionEvent::ScratchEntry)?;
                    let mut values = Vec::new();
                    for key in map.as_map().unwrap().0 {
                        for _ in 0..=key
                            .len()
                            .div_ceil(fgdb_gql::algebra::GRAPH_VALUE_PAYLOAD_UNIT_BYTES)
                        {
                            charge(GlaExecutionEvent::ScratchEntry)?;
                        }
                        values.push(text(key));
                    }
                    return Ok(Some(list(values)));
                }
                let value = match kind {
                    GraphElementMetadata::Labels => &labels,
                    GraphElementMetadata::Type => &relation,
                    GraphElementMetadata::Properties => map,
                    GraphElementMetadata::Keys => unreachable!(),
                };
                value.copy_with_control(&mut charge).map(Some)
            },
        )
    }

    fn execute(
        fixture: &Fixture,
        query: &PreparedGraphMutationQuery,
        allowance: GraphMutationPolicy,
        calls: &Cell<usize>,
        hidden: bool,
        checkpoint: impl FnMut() -> Result<(), ()>,
    ) -> ResultOf {
        query.execute_governed_reading(
            allowance,
            |pattern, budget| fixture.source(pattern, budget),
            reader(fixture, calls, hidden),
            checkpoint,
        )
    }

    #[test]
    fn computed_identities_read_final_property_label_and_relationship_metadata() {
        let fixture = Fixture::new();
        let query = prepare(
            "MATCH (a)-[r:R]->(b) SET a.p=b.p,a.q=NULL,r.q=r.p+1,a:After \
             REMOVE a.old,r.old,r.p,a:Before \
             RETURN properties([a][0]) AS node,keys([a][0]) AS names,\
             labels([a][0]) AS labels,properties([r][0]) AS edge,\
             type([r][0]) AS relation ORDER BY edge",
            MetadataCatalog,
        );
        let calls = Cell::new(0);
        let batch = execute(&fixture, &query, policy(), &calls, false, || Ok(())).unwrap();
        assert_eq!(
            rows(&batch),
            [12, 13]
                .into_iter()
                .map(|q| vec![
                    map(vec![("keep", int(7)), ("p", int(20)), ("q", null())]),
                    list(vec![text("keep"), text("p"), text("q")]),
                    list(vec![text("After")]),
                    map(vec![("q", int(q))]),
                    text("R"),
                ])
                .collect::<Vec<_>>()
        );
        assert_eq!(calls.get(), 10);
        assert_eq!(
            fixture.vertices[&VId(1)],
            map(vec![("p", int(10)), ("old", int(3)), ("keep", int(7)),])
        );
    }

    #[test]
    fn simultaneous_rhs_is_frozen_and_grouped_tail_metadata_sees_all_proposals() {
        let fixture = Fixture::new();
        let query = prepare(
            "MATCH (a)-[r:R]->(b) \
             SET a.q=properties([a][0]).p,a.p=99,r.q=properties([r][0]).p,a:After \
             WITH collect(a) AS ns,collect(r) AS rs \
             RETURN properties(ns[0]).q AS old,properties(ns[0]).p AS final,\
             [x IN ns | labels(x)] AS labels,[x IN rs | keys(x)] AS keys,\
             [x IN rs | type(x)] AS types",
            MetadataCatalog,
        );
        let batch = execute(&fixture, &query, policy(), &Cell::new(0), false, || Ok(())).unwrap();
        assert_eq!(
            rows(&batch),
            vec![vec![
                int(10),
                int(99),
                list(vec![list(vec![text("Before"), text("After")]); 2]),
                list(vec![list(vec![text("old"), text("p"), text("q")]); 2]),
                list(vec![text("R"), text("R")]),
            ]]
        );
        assert!(batch.mutation().intents().iter().any(|intent| matches!(
            intent,
            GraphMutationIntent::StoredProperty { target: ElementId::Vertex(VId(1)), key: Q, value }
                if value.as_scalar() == Some(&CanonicalScalar::Int(10))
        ) || matches!(
            intent,
            GraphMutationIntent::Property { vertex: VId(1), key: Q, value: Some(value) }
                if value.as_scalar() == Some(&CanonicalScalar::Int(10))
        )));
    }

    #[test]
    fn input_relations_and_aggregate_arguments_share_the_reader() {
        let fixture = Fixture::new();
        for statement in [
            "MATCH (n) WITH n,properties([n][0]).p AS old \
             SET n.p=old+1 RETURN properties([n][0]).p AS p ORDER BY p",
            "MATCH (n) SET n.p=n.p+1 WITH properties([n][0]).p AS p RETURN p ORDER BY p",
        ] {
            let query = prepare(statement, MetadataCatalog);
            let result =
                execute(&fixture, &query, policy(), &Cell::new(0), false, || Ok(())).unwrap();
            assert_eq!(
                rows(&result),
                vec![vec![int(11)], vec![int(21)]],
                "{statement}"
            );
        }
        let query = prepare(
            "MATCH (n) SET n.p=n.p+1 RETURN collect(properties([n][0]).p) AS ps",
            MetadataCatalog,
        );
        let result = execute(&fixture, &query, policy(), &Cell::new(0), false, || Ok(())).unwrap();
        assert_eq!(rows(&result), vec![vec![list(vec![int(11), int(21)])]]);
    }

    #[test]
    fn create_and_insert_output_bind_metadata_of_new_identities() {
        use fgdb_gql::PreparedGraphInsertQueryText;
        use fgdb_gql::insertion::{GraphInsertPolicy, GraphInsertRequest};

        let fixture = Fixture::new();
        for keyword in ["CREATE", "INSERT"] {
            for tail in [
                "RETURN properties([a][0]) AS node,labels([a][0]) AS labels,\
                 properties([r][0]) AS edge,keys([r][0]) AS names,type([r][0]) AS relation",
                "WITH [a] AS ns,[r] AS rs RETURN properties(ns[0]) AS node,labels(ns[0]) AS labels,\
                 properties(rs[0]) AS edge,keys(rs[0]) AS names,type(rs[0]) AS relation",
            ] {
                let statement = format!(
                    "{keyword} (a:Before {{p:10,old:3,keep:7}})-[r:R {{p:11,old:8}}]->(b:Before {{p:20}}) {tail}"
                );
                let query = PreparedGraphInsertQueryText::prepare(&statement, R, MetadataCatalog)
                    .unwrap()
                    .bind_parameters(&GqlParameters::new())
                    .unwrap();
                let collection = query
                    .collect_governed_over(
                        None,
                        GraphInsertPolicy::new(policy().query, 100, 100),
                        |_: &PreparedGraphPattern<GraphValueRow>, _: GqlQueryPolicy|
                         -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<(), ()>> {
                            panic!("standalone insertion has no graph source")
                        },
                        |request| {
                            Ok::<_, ()>(match request {
                                GraphInsertRequest::Vertex { row: 0, vertex } => {
                                    ElementId::Vertex(VId(1 + vertex as u128))
                                }
                                GraphInsertRequest::Edge { row: 0, edge } => {
                                    ElementId::Edge(EId(11 + edge as u128))
                                }
                                _ => panic!("one creation occurrence"),
                            })
                        },
                        || Ok(()),
                    )
                    .unwrap();
                let (insertion, output) = collection.into_parts();
                assert_eq!(insertion.stats().created_vertices, 2);
                assert_eq!(insertion.stats().created_edges, 1);
                // The DB stages the returned intents before this boundary.
                // This fixture supplies those exact final records; the law
                // exercises the real parser, binder and output evaluator.
                let calls = Cell::new(0);
                let result = output
                    .finish_governed_reading::<(), ()>(reader(&fixture, &calls, false), || Ok(()))
                    .unwrap();
                assert_eq!(
                    result.value[0].values(),
                    &[
                        fixture.vertices[&VId(1)].clone(),
                        list(vec![text("Before")]),
                        fixture.edge_maps[&EId(11)].clone(),
                        list(vec![text("old"), text("p")]),
                        text("R"),
                    ],
                    "{statement}"
                );
                assert_eq!(calls.get(), 5);
            }
        }
    }

    #[test]
    fn vertex_merge_and_upsert_outputs_retain_metadata_catalog_and_reader() {
        use fgdb_gql::{
            GlaExecutionStats, GqlExecutionStats, GraphVertexMergeRowsBinding,
            GraphVertexReturnBinding, GraphWriteChainStep, PreparedGraphVertexUpsertQueryText,
            PreparedGraphWriteChainText,
        };

        let fixture = Fixture::new();
        let suffix =
            "RETURN properties([n][0]) AS node,keys([n][0]) AS names,labels([n][0]) AS labels";
        let expected = vec![
            fixture.vertices[&VId(1)].clone(),
            list(vec![text("keep"), text("old"), text("p")]),
            list(vec![text("Before")]),
        ];
        for actions in ["", "ON CREATE SET n.old=3,n.keep=7"] {
            let statement = format!("MERGE (n:Before {{p:10}}) {actions} {suffix}");
            let query = PreparedGraphVertexUpsertQueryText::prepare(&statement, R, MetadataCatalog)
                .unwrap()
                .bind_parameters(&GqlParameters::new())
                .unwrap();
            assert_eq!(query.bindings(), &[GraphVertexReturnBinding::Vertex]);
            let calls = Cell::new(0);
            let result = query
                .project_governed_reading::<(), ()>(
                    vec![GraphValue::Vertex(VId(1))],
                    GqlExecutionStats::default(),
                    GlaExecutionStats::default(),
                    policy().query,
                    reader(&fixture, &calls, false),
                    || Ok(()),
                )
                .unwrap();
            assert_eq!(result.value[0].values(), expected);
            assert_eq!(calls.get(), 3);
        }
        for suffix in [
            suffix,
            "WITH collect(n) AS ns WHERE size(ns)>0 RETURN properties(ns[0]) AS node,\
             keys(ns[0]) AS names,labels(ns[0]) AS labels",
        ] {
            let statement = format!("CREATE (audit) WITH audit MERGE (n:Before {{p:10}}) {suffix}");
            let chain = PreparedGraphWriteChainText::prepare(&statement, R, MetadataCatalog)
                .unwrap()
                .bind_parameters(&GqlParameters::new())
                .unwrap();
            let [_, GraphWriteChainStep::VertexMergeQuery(query)] = chain.steps() else {
                panic!("owned-row vertex merge query");
            };
            assert_eq!(query.bindings(), &[GraphVertexMergeRowsBinding::Vertex]);
            let calls = Cell::new(0);
            let result = query
                .finish_governed_reading::<(), ()>(
                    vec![vec![GraphValue::Vertex(VId(1))]],
                    GqlExecutionStats::default(),
                    GlaExecutionStats::default(),
                    policy().query,
                    reader(&fixture, &calls, false),
                    || Ok(()),
                )
                .unwrap();
            assert_eq!(result.value[0].values(), expected, "{statement}");
            assert_eq!(calls.get(), 3);
        }
    }

    #[test]
    fn relationship_merge_and_upsert_outputs_retain_metadata_catalog_and_reader() {
        use fgdb_gql::{
            GlaExecutionStats, GqlExecutionStats, GraphEdgeMergeBinding, GraphEdgeReturnBinding,
            PreparedGraphEdgeMergeQueryText, PreparedGraphEdgeUpsertQueryText,
        };

        let fixture = Fixture::new();
        let suffix =
            "RETURN properties([r][0]) AS edge,keys([r][0]) AS names,type([r][0]) AS relation";
        let expected = vec![
            fixture.edge_maps[&EId(11)].clone(),
            list(vec![text("old"), text("p")]),
            text("R"),
        ];
        let query = PreparedGraphEdgeMergeQueryText::prepare(
            &format!("MATCH (a),(b) MERGE (a)-[r:R]->(b) {suffix}"),
            R,
            MetadataCatalog,
        )
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
        assert_eq!(query.bindings(), &[GraphEdgeMergeBinding::Edge]);
        let calls = Cell::new(0);
        let result = query
            .finish_governed_reading::<(), ()>(
                vec![vec![GraphValue::Edge(EId(11))]],
                GqlExecutionStats::default(),
                GlaExecutionStats::default(),
                policy().query,
                reader(&fixture, &calls, false),
                || Ok(()),
            )
            .unwrap();
        assert_eq!(result.value[0].values(), expected);
        assert_eq!(calls.get(), 3);
        let query = PreparedGraphEdgeUpsertQueryText::prepare(
            &format!("MATCH (a),(b) MERGE (a)-[r:R]->(b) ON CREATE SET r.p=11,r.old=8 {suffix}"),
            R,
            MetadataCatalog,
        )
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
        assert_eq!(query.bindings(), &[GraphEdgeReturnBinding::Edge]);
        let calls = Cell::new(0);
        let result = query
            .project_governed_reading::<(), ()>(
                Some(vec![GraphValue::Edge(EId(11))]),
                GqlExecutionStats::default(),
                GlaExecutionStats::default(),
                policy().query,
                reader(&fixture, &calls, false),
                || Ok(()),
            )
            .unwrap();
        assert_eq!(result.value[0].values(), expected);
        assert_eq!(calls.get(), 3);
    }

    #[test]
    fn deletion_and_cascade_refuse_before_reading_old_metadata() {
        let fixture = Fixture::new();
        for statement in [
            "MATCH (n) DETACH DELETE n RETURN labels([n][0]) AS names",
            "MATCH (a)-[r:R]->(b) DELETE r RETURN type([r][0]) AS relation",
            "MATCH (a)-[r:R]->(b) DETACH DELETE a RETURN keys([r][0]) AS names",
        ] {
            let query = prepare(statement, MetadataCatalog);
            let calls = Cell::new(0);
            assert!(
                execute(&fixture, &query, policy(), &calls, false, || Ok(())).is_err(),
                "{statement}"
            );
            assert_eq!(calls.get(), 0, "{statement}");
        }
    }

    #[test]
    fn lazy_metadata_and_masked_values_do_not_gain_authority_from_assignments() {
        let fixture = Fixture::new();
        let query = prepare(
            "MATCH (n) SET n.p=99 RETURN CASE WHEN false THEN labels([n][0]) ELSE NULL END AS names",
            MetadataCatalog,
        );
        assert_eq!(
            rows(&fixture.execute(&query, policy()).unwrap()),
            vec![vec![null()]; 2]
        );
        let query = prepare(
            "MATCH (n) SET n.p=99,n:After RETURN properties([n][0]) AS m,labels([n][0]) AS ls",
            MetadataCatalog,
        );
        assert!(
            fixture.execute(&query, policy()).is_err(),
            "missing capability refuses"
        );
        assert_eq!(
            rows(&execute(&fixture, &query, policy(), &Cell::new(0), true, || Ok(())).unwrap()),
            vec![vec![null(), null()]; 2],
        );
    }

    #[test]
    fn every_metadata_cancellation_and_cumulative_quota_refuses_without_a_batch() {
        let fixture = Fixture::new();
        let query = prepare(
            "MATCH (n) SET n.q=[n.p,42],n:After REMOVE n.old \
             RETURN properties([n][0]) AS p,keys([n][0]) AS k,labels([n][0]) AS ls LIMIT 0",
            MetadataCatalog,
        );
        let polls = Cell::new(0);
        let batch = execute(&fixture, &query, policy(), &Cell::new(0), false, || {
            polls.set(polls.get() + 1);
            Ok(())
        })
        .unwrap();
        assert!(batch.returning().value.is_empty());
        assert!(!batch.mutation().intents().is_empty());
        for stop in 1..=polls.get() {
            let seen = Cell::new(0);
            let result = execute(&fixture, &query, policy(), &Cell::new(0), false, || {
                seen.set(seen.get() + 1);
                if seen.get() == stop { Err(()) } else { Ok(()) }
            });
            assert!(
                matches!(result, Err(GqlQueryError::Interrupted(()))),
                "checkpoint {stop}: {result:?}"
            );
            assert_eq!(seen.get(), stop, "nothing runs after refusal");
        }
        let measured = batch.returning();
        let exact = GraphMutationPolicy::new(
            GqlQueryPolicy::new(
                measured.rows.snapshot_records,
                0,
                measured.evaluator.work_units,
                measured.evaluator.scratch_entries,
            ),
            policy().max_effects,
        );
        let again = execute(&fixture, &query, exact, &Cell::new(0), false, || Ok(())).unwrap();
        assert_eq!(again.returning().evaluator, measured.evaluator);
        for allowance in [
            GraphMutationPolicy::new(
                GqlQueryPolicy::new(10_000, 0, measured.evaluator.work_units - 1, 1_000_000),
                policy().max_effects,
            ),
            GraphMutationPolicy::new(
                GqlQueryPolicy::new(10_000, 0, 1_000_000, measured.evaluator.scratch_entries - 1),
                policy().max_effects,
            ),
        ] {
            assert!(execute(&fixture, &query, allowance, &Cell::new(0), false, || Ok(())).is_err());
        }
    }
}
