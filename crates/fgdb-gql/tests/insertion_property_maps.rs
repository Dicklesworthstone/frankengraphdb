//! CREATE/INSERT maps consume checked occurrence fields before ID allocation.

use fgdb_delta_types::{ElementId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GraphValue, GraphValueRow};
use fgdb_gql::insertion::{
    GraphInsertError, GraphInsertPolicy, GraphInsertRequest, GraphInsertVertex, PreparedGraphInsert,
};
use fgdb_gql::{
    GlaExecutionLimits, GqlParameters, GqlQueryError, GqlQueryExecution, GqlQueryPolicy,
    GqlScalarParameter, GraphInsertBinding, GraphInsertQueryBatch, GraphInsertQueryBuildError,
    GraphInsertQueryError, GraphInsertTextErrorKind, GraphMutationValue, GraphSetProjection,
    GraphSetQuantifier, GraphSetValue, GraphSymbol, GraphSymbolKind, GraphSymbolResolver,
    PreparedGraphInsertQuery, PreparedGraphInsertQueryText, ReverseSymbolCatalog,
};
use fgdb_types::{CanonicalPropertyValue, CanonicalScalar, EId, VId};
use std::cell::Cell;
use std::collections::BTreeMap;

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
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
        let mut result = ReverseSymbolCatalog::new();
        if let Some(name) = self.0 {
            result.insert_property(P, name);
        }
        result.insert_property(Q, "q");
        Some(result)
    }
}
fn int(value: i64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(value))
}
fn null() -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Null)
}
fn text(value: &str) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::ucs_basic_text(value).unwrap())
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
fn policy() -> GraphInsertPolicy {
    GraphInsertPolicy::new(
        GqlQueryPolicy::new(100, 100, 1_000_000, 1_000_000),
        100,
        100,
    )
}
fn rows(batch: &GraphInsertQueryBatch) -> Vec<Vec<GraphValue>> {
    batch
        .returning()
        .value
        .iter()
        .map(|row| row.values().to_vec())
        .collect()
}
fn identity(request: GraphInsertRequest) -> Result<ElementId, ()> {
    Ok(match request {
        GraphInsertRequest::Vertex { row, vertex } => {
            ElementId::Vertex(VId(100 + row as u128 * 16 + vertex as u128))
        }
        GraphInsertRequest::Edge { row, edge } => {
            ElementId::Edge(EId(1_000 + row as u128 * 16 + edge as u128))
        }
    })
}
fn prepare(statement: &str, catalog: impl GraphSymbolResolver) -> PreparedGraphInsertQuery {
    PreparedGraphInsertQueryText::prepare(statement, R, catalog)
        .unwrap_or_else(|error| panic!("{statement}: {error:?}"))
        .bind_parameters(&GqlParameters::new())
        .unwrap_or_else(|error| panic!("{statement}: {error:?}"))
}
fn create(
    query: &PreparedGraphInsertQuery,
    policy: GraphInsertPolicy,
) -> Result<GraphInsertQueryBatch, GqlQueryError<GraphInsertQueryError<(), ()>, ()>> {
    query.execute_governed(
        policy,
        |_, _| -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<(), ()>> {
            panic!("standalone/UNWIND creation must not scan a graph for maps")
        },
        identity,
        || Ok(()),
    )
}

#[test]
fn created_vertex_edge_empty_and_selective_maps_share_one_native_occurrence() {
    let body = "(a {p:3,q:NULL})-[r:R {p:7}]->(a),(b) RETURN properties(a) AS a,\
              properties(r) AS r,properties(b) AS b,keys(a) AS names,\
              a{.*,p:a.p+1} AS projected,r{.p,a} AS selected";
    let create_query = prepare(&format!("CREATE {body}"), Catalog(Some("p")));
    let insert_query = prepare(&format!("INSERT {body}"), Catalog(Some("p")));
    assert_eq!(
        create_query.canonical_bytes(),
        insert_query.canonical_bytes()
    );
    let batch = create(&create_query, policy()).unwrap();
    assert_eq!(
        rows(&batch),
        vec![vec![
            map(vec![("p", int(3))]),
            map(vec![("p", int(7))]),
            map(vec![]),
            list(vec![text("p")]),
            map(vec![("p", int(4))]),
            map(vec![("p", int(7)), ("a", GraphValue::Vertex(VId(100)))]),
        ]]
    );
    assert_eq!(batch.insertion().stats().created_vertices, 2);
    assert_eq!(batch.insertion().stats().created_edges, 1);
    // Selective projections only need the explicitly resolved property IDs.
    let selective = prepare("CREATE (n {p:8}) RETURN n{.p,q:n.p+1} AS m", symbols);
    assert_eq!(
        rows(&create(&selective, policy()).unwrap()),
        vec![vec![map(vec![("p", int(8)), ("q", int(9))])]]
    );
}

#[test]
fn unwind_maps_keep_duplicate_occurrences_and_empty_native_aggregate_groups() {
    let query = prepare(
        "UNWIND [1,1,2,NULL] AS x CREATE (n {p:x}) \
        RETURN n.p AS p,count(*) AS copies,collect(properties(n)) AS maps ORDER BY p NULLS LAST",
        Catalog(Some("p")),
    );
    let batch = create(&query, policy()).unwrap();
    assert_eq!(
        rows(&batch),
        vec![
            vec![int(1), int(2), list(vec![map(vec![("p", int(1))]); 2])],
            vec![int(2), int(1), list(vec![map(vec![("p", int(2))])])],
            vec![null(), int(1), list(vec![map(vec![])])],
        ]
    );
    assert_eq!(batch.insertion().stats().created_vertices, 4);
    let empty = prepare(
        "UNWIND [] AS x CREATE (n {p:x}) RETURN collect(n{.*}) AS maps,count(*) AS copies",
        Catalog(Some("p")),
    );
    let empty = create(&empty, policy()).unwrap();
    assert_eq!(rows(&empty), vec![vec![list(vec![]), int(0)]]);
    assert_eq!(empty.insertion().stats().created_vertices, 0);
}

#[test]
fn imported_rows_and_matched_maps_use_the_original_source_and_offsets() {
    let vertices = BTreeMap::from([
        (VId(1), map(vec![("p", int(10))])),
        (VId(2), map(vec![("p", int(20))])),
    ]);
    let edge_map = map(vec![("p", int(5))]);
    // The scalar property source stores exactly the values the maps hold.
    let stored: BTreeMap<VId, CanonicalPropertyValue> = vertices
        .iter()
        .map(|(vid, value)| {
            let scalar = value.as_map().unwrap().1[0].as_scalar().unwrap();
            (*vid, CanonicalPropertyValue::from(scalar.clone()))
        })
        .collect();
    let edge_stored =
        CanonicalPropertyValue::from(edge_map.as_map().unwrap().1[0].as_scalar().unwrap().clone());
    let query = prepare(
        "UNWIND [2,3] AS bump MATCH (n)-[old:R]->(m) \
        CREATE (c {p:n.p+bump})-[fresh:R {p:old.p}]->(n) RETURN properties(n) AS original,\
        properties(old) AS old,properties(c) AS created,properties(fresh) AS fresh ORDER BY created",
        Catalog(Some("p")),
    );
    let calls = Cell::new(0);
    let batch = query
        .execute_governed(
            policy(),
            |pattern, budget| {
                calls.set(calls.get() + 1);
                pattern.plan().execute_governed_with_element_maps(
                    3,
                    vertices.keys().copied(),
                    [(EId(11), VId(1), R, VId(2))],
                    |_, _| Ok::<_, ()>(true),
                    |vid, key| Ok((key == P).then(|| stored.get(&vid).unwrap())),
                    |_, key| Ok((key == P).then_some(&edge_stored)),
                    |_| Ok(None),
                    |_| Ok(None),
                    |vid| Ok(vertices.get(&vid)),
                    |_| Ok(Some(&edge_map)),
                    budget,
                    || Ok::<_, ()>(()),
                )
            },
            identity,
            || Ok(()),
        )
        .unwrap();
    assert_eq!(calls.get(), 1);
    assert_eq!(
        rows(&batch),
        [12, 13]
            .into_iter()
            .map(|p| vec![
                map(vec![("p", int(10))]),
                map(vec![("p", int(5))]),
                map(vec![("p", int(p))]),
                map(vec![("p", int(5))]),
            ])
            .collect::<Vec<_>>()
    );
    assert_eq!(batch.insertion().stats().created_vertices, 2);
    assert_eq!(batch.insertion().stats().created_edges, 2);
}

#[test]
fn map_names_admit_before_execution_and_native_identity_binds_only_relevant_catalogs() {
    for prefix in ["", "UNWIND [] AS x "] {
        let statement = format!("{prefix}CREATE (n {{p:1}}) RETURN properties(n) AS m LIMIT 0");
        for (catalog, unmapped) in [(Catalog(None), true), (Catalog(Some("q")), false)] {
            let error = PreparedGraphInsertQueryText::prepare(&statement, R, catalog).unwrap_err();
            assert!(
                matches!(error.kind,
                GraphInsertTextErrorKind::ReturnBuild(GraphInsertQueryBuildError::UnmappedProperty { .. }) if unmapped)
                    || matches!(error.kind,GraphInsertTextErrorKind::ReturnBuild(GraphInsertQueryBuildError::PropertyMapCatalog) if !unmapped)
            );
        }
    }
    let insertion = PreparedGraphInsert::prepare_standalone(
        R,
        vec![GraphInsertVertex {
            labels: vec![],
            properties: vec![(
                P,
                GraphMutationValue::Literal(
                    GqlScalarParameter::new(CanonicalScalar::Int(1)).unwrap(),
                ),
            )],
        }],
        vec![],
    )
    .unwrap();
    let projection = vec![GraphSetProjection::new("m", GraphSetValue::Column(0))];
    assert!(matches!(
        PreparedGraphInsertQuery::prepare(
            insertion.clone(),
            vec![GraphInsertBinding::VertexProperties(0)],
            projection.clone(),
            GraphSetQuantifier::All
        ),
        Err(GraphInsertQueryBuildError::UnmappedProperty { binding: 0 })
    ));
    let bind = |name| {
        PreparedGraphInsertQuery::prepare_with_property_catalog(
            insertion.clone(),
            vec![GraphInsertBinding::VertexProperties(0)],
            projection.clone(),
            GraphSetQuantifier::All,
            Catalog(Some(name)).reverse_catalog().unwrap(),
        )
        .unwrap()
    };
    let ordinary = bind("p");
    let renamed = bind("renamed");
    assert_ne!(ordinary.canonical_bytes(), renamed.canonical_bytes());
    assert_eq!(
        rows(&create(&renamed, policy()).unwrap()),
        vec![vec![map(vec![("renamed", int(1))])]]
    );
    let statement = "CREATE (n {p:1}) RETURN n.p AS p";
    assert_eq!(
        prepare(statement, Catalog(Some("p"))).canonical_bytes(),
        prepare(statement, Catalog(Some("renamed"))).canonical_bytes()
    );
}

#[test]
fn complete_maps_share_budgets_and_cancellation_precedes_identity_allocation() {
    let statement = format!(
        "CREATE (n {{p:'{}'}}) RETURN properties(n) AS m",
        "payload".repeat(1_000)
    );
    let query = prepare(&statement, Catalog(Some("p")));
    let events = Cell::new(0);
    let before_first_id = Cell::new(None);
    let complete = query
        .execute_governed(
            policy(),
            |_, _| -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<(), usize>> {
                panic!("no graph read")
            },
            |request| {
                before_first_id.set(Some(events.get()));
                identity(request)
            },
            || {
                events.set(events.get() + 1);
                Ok(())
            },
        )
        .unwrap();
    let stop = before_first_id.get().unwrap();
    let stopped = Cell::new(0);
    let allocated = Cell::new(0);
    assert!(matches!(
        query.execute_governed(
            policy(),
            |_, _| -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<(), usize>> {
                panic!("no graph read")
            },
            |request| {
                allocated.set(allocated.get() + 1);
                identity(request)
            },
            || {
                stopped.set(stopped.get() + 1);
                if stopped.get() == stop {
                    Err(stop)
                } else {
                    Ok(())
                }
            }
        ),
        Err(GqlQueryError::Interrupted(_))
    ));
    assert_eq!(allocated.get(), 0);
    let stats = complete.returning().evaluator;
    let mut exact = policy();
    exact.query.evaluator = GlaExecutionLimits::new(stats.work_units, stats.scratch_entries);
    assert_eq!(rows(&create(&query, exact).unwrap()), rows(&complete));
    for (work, scratch) in [
        (stats.work_units - 1, stats.scratch_entries),
        (stats.work_units, stats.scratch_entries - 1),
    ] {
        let mut bounded = policy();
        bounded.query.evaluator = GlaExecutionLimits::new(work, scratch);
        assert!(matches!(
            create(&query, bounded),
            Err(GqlQueryError::Evaluator(_))
        ));
    }
    let mut quota = policy();
    quota.max_vertices = 0;
    assert!(matches!(
        create(&query, quota),
        Err(GqlQueryError::Source(GraphInsertQueryError::Insertion(
            GraphInsertError::Limit { .. }
        )))
    ));
    let paged = prepare(&(statement + " LIMIT 0"), Catalog(Some("p")));
    let empty = create(&paged, policy()).unwrap();
    assert!(empty.returning().value.is_empty());
    assert_eq!(empty.insertion().stats().created_vertices, 1);
}

/// openCypher Graph3: labels() of a vertex the CREATE makes is its CREATE
/// labels, without a graph read, named through the catalog in label-identity
/// order like every labels() read. A label the catalog cannot name refuses at
/// preparation, and the label names are part of the definition's identity.
#[test]
fn created_vertex_labels_are_the_create_labels_in_identity_order() {
    use fgdb_delta_types::LabelId;
    #[derive(Clone, Copy)]
    struct Labels(&'static str);
    impl GraphSymbolResolver for Labels {
        fn resolve_symbol(&mut self, kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
            match (kind, name) {
                (GraphSymbolKind::Label, "Foo") => Some(GraphSymbol::Label(LabelId(1))),
                (GraphSymbolKind::Label, "Bar") => Some(GraphSymbol::Label(LabelId(2))),
                (GraphSymbolKind::Label, "Dog") => Some(GraphSymbol::Label(LabelId(3))),
                _ => symbols(kind, name),
            }
        }
        fn reverse_catalog(&self) -> Option<ReverseSymbolCatalog> {
            let mut result = ReverseSymbolCatalog::new();
            result.insert_label(LabelId(1), "Foo");
            if !self.0.is_empty() {
                result.insert_label(LabelId(2), self.0);
            }
            result.insert_label(LabelId(3), "Dog");
            Some(result)
        }
    }
    let names = |names: &[&str]| list(names.iter().map(|name| text(name)).collect());
    for (statement, expected) in [
        (
            // Bar is written first; identity order still names Foo first.
            "CREATE (node:Bar:Foo {p: 1}) RETURN labels(node) AS l, node.p AS p",
            vec![vec![names(&["Foo", "Bar"]), int(1)]],
        ),
        ("CREATE (node) RETURN labels(node)", vec![vec![names(&[])]]),
        (
            "CREATE (node:Foo) RETURN CASE WHEN true THEN labels(node) ELSE labels(NULL) END AS l",
            vec![vec![names(&["Foo"])]],
        ),
        (
            "CREATE (n:Foo)-[:R]->(:Dog) RETURN labels(n) AS l",
            vec![vec![names(&["Foo"])]],
        ),
        (
            "UNWIND [1, 2] AS x CREATE (n:Dog {p: x}) RETURN labels(n) AS l, n.p AS p",
            vec![vec![names(&["Dog"]), int(1)], vec![names(&["Dog"]), int(2)]],
        ),
    ] {
        let batch = create(&prepare(statement, Labels("Bar")), policy())
            .unwrap_or_else(|error| panic!("{statement}: {error:?}"));
        assert_eq!(rows(&batch), expected, "{statement}");
    }
    let statement = "CREATE (node:Bar:Foo) RETURN labels(node) AS l";
    let error = PreparedGraphInsertQueryText::prepare(statement, R, Labels("")).unwrap_err();
    assert!(
        matches!(
            error.kind,
            GraphInsertTextErrorKind::ReturnBuild(GraphInsertQueryBuildError::UnmappedLabel {
                binding: 0
            })
        ),
        "{error:?}"
    );
    assert_ne!(
        prepare(statement, Labels("Bar")).canonical_bytes(),
        prepare(statement, Labels("Baz")).canonical_bytes()
    );
}

/// openCypher: a property map may read a vertex an earlier pattern of the
/// same CREATE made (`CREATE (a {id: 0}), ({num: a.id})`, the fixture of TCK
/// WithSkipLimit1 [1]): that vertex's value for the key, or NULL when its map
/// does not set it. A vertex made later is not yet in scope.
#[test]
fn a_create_map_reads_a_sibling_vertex_made_before_it() {
    let batch = create(
        &prepare(
            "CREATE (a {p: 7}), (b {p: a.p, q: a.q})-[r:R {p: a.p}]->(a) \
             RETURN b.p AS p, b.q AS q, r.p AS weight",
            Catalog(Some("p")),
        ),
        policy(),
    )
    .unwrap();
    assert_eq!(rows(&batch), vec![vec![int(7), null(), int(7)]]);
    assert!(
        PreparedGraphInsertQueryText::prepare(
            "CREATE (b {p: a.p}), (a {p: 7}) RETURN b",
            R,
            Catalog(Some("p")),
        )
        .is_err()
    );
}

/// openCypher Graph7 [2], [3]: a created element's `n[key]` subscripts its
/// property map, so the key may be computed or a parameter, and an absent
/// key reads NULL.
#[test]
fn created_element_subscripts_read_its_property_map() {
    let batch = create(
        &prepare(
            "CREATE (n {p:5})-[r:R {q:6}]->(m) RETURN n['p'] AS p, n['q'] AS absent, \
             r['' + 'q'] AS weight, m['p'] AS empty",
            Catalog(Some("p")),
        ),
        policy(),
    )
    .unwrap();
    assert_eq!(rows(&batch), vec![vec![int(5), null(), int(6), null()]]);
    let query = PreparedGraphInsertQueryText::prepare_with_parameter_types(
        "CREATE (n {p:5}) RETURN n[$key] AS value",
        R,
        &[(
            "key",
            fgdb_gql::GqlParameterType::Scalar(fgdb_types::CanonicalScalarKind::Text),
        )],
        Catalog(Some("p")),
    )
    .unwrap()
    .bind_parameters(&GqlParameters::new().with_text("key", "p").unwrap())
    .unwrap();
    assert_eq!(rows(&create(&query, policy()).unwrap()), vec![vec![int(5)]]);
}
