//! MERGE maps use the existing native final-row projection and grouping owners.
//! Actual staged branch values, authority and rollback are exercised in fgdb.

use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GraphValue, GraphValueRow};
use fgdb_gql::{
    GlaExecutionStats, GqlExecutionStats, GqlParameters, GqlQueryError, GqlQueryExecution,
    GqlQueryPolicy, GraphSetExecutionError, GraphSetProjection, GraphSetQuantifier, GraphSetValue,
    GraphSymbol, GraphSymbolKind, GraphSymbolResolver, GraphVertexReturnBinding,
    GraphVertexUpsertQueryBuildError, GraphVertexUpsertTextErrorKind,
    PreparedGraphVertexUpsertQuery, PreparedGraphVertexUpsertQueryText, ReverseSymbolCatalog,
};
use fgdb_types::{CanonicalScalar, VId};
use std::cell::Cell;

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);
const NOTE: PropertyKeyId = PropertyKeyId(3);

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "Item") => Some(GraphSymbol::Label(LabelId(1))),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
        (GraphSymbolKind::Property, "edge-note") => Some(GraphSymbol::Property(NOTE)),
        _ => None,
    }
}

#[derive(Clone, Copy)]
struct Catalog(&'static str);
impl GraphSymbolResolver for Catalog {
    fn resolve_symbol(&mut self, kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        symbols(kind, name)
    }
    fn reverse_catalog(&self) -> Option<ReverseSymbolCatalog> {
        let mut catalog = ReverseSymbolCatalog::new();
        catalog.insert_property(P, self.0);
        catalog.insert_property(Q, "q");
        catalog.insert_property(NOTE, "edge-note");
        Some(catalog)
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
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100, 100, 1_000_000, 1_000_000)
}
fn prepare(statement: &str, catalog: impl GraphSymbolResolver) -> PreparedGraphVertexUpsertQuery {
    PreparedGraphVertexUpsertQueryText::prepare(statement, R, catalog)
        .unwrap_or_else(|error| panic!("{statement}: {error:?}"))
        .bind_parameters(&GqlParameters::new())
        .unwrap_or_else(|error| panic!("{statement}: {error:?}"))
}
fn project(
    query: &PreparedGraphVertexUpsertQuery,
    input: Vec<GraphValue>,
) -> GqlQueryExecution<GraphValueRow> {
    query
        .project_governed::<(), ()>(
            input,
            GqlExecutionStats::default(),
            GlaExecutionStats::default(),
            policy(),
            || Ok(()),
        )
        .unwrap()
}
fn rows(execution: &GqlQueryExecution<GraphValueRow>) -> Vec<Vec<GraphValue>> {
    execution
        .value
        .iter()
        .map(|row| row.values().to_vec())
        .collect()
}

#[test]
fn complete_keys_wildcard_and_selective_maps_share_one_final_binding() {
    let template = PreparedGraphVertexUpsertQueryText::prepare(
        "MERGE (n:Item {p:$key}) ON MATCH SET n.q=8 ON CREATE SET n.q=$value \
         SET n.q=n.q+1 RETURN properties(n) AS full,keys(n) AS names,\
         n{.*,q:n.q+1,anchor:n} AS overlay,n{.p,`edge-note`:n.q+2} AS chosen",
        R,
        Catalog("p"),
    )
    .unwrap();
    let parameters = GqlParameters::new()
        .with_int64("key", 7)
        .unwrap()
        .with_int64("value", 9)
        .unwrap();
    let query = template.bind_parameters(&parameters).unwrap();
    assert_eq!(
        query.bindings(),
        [
            GraphVertexReturnBinding::Properties,
            GraphVertexReturnBinding::Property(Q),
            GraphVertexReturnBinding::Vertex,
            GraphVertexReturnBinding::Property(P),
        ]
    );
    let full = map(vec![("edge-note", null()), ("p", int(7)), ("q", int(10))]);
    let execution = project(
        &query,
        vec![full.clone(), int(10), GraphValue::Vertex(VId(42)), int(7)],
    );
    assert_eq!(
        rows(&execution),
        vec![vec![
            full,
            list(vec![text("edge-note"), text("p"), text("q")]),
            map(vec![
                ("anchor", GraphValue::Vertex(VId(42))),
                ("edge-note", null()),
                ("p", int(7)),
                ("q", int(11))
            ]),
            map(vec![("edge-note", int(12)), ("p", int(7))]),
        ]]
    );
    assert_eq!(
        template
            .bind_parameters(&parameters)
            .unwrap()
            .canonical_bytes(),
        query.canonical_bytes()
    );

    // Selectors need ordinary symbol resolution only; absent scalar fields are
    // explicit NULL entries, while a selective projection never enumerates.
    let selective = prepare(
        "MERGE (n:Item {p:7}) RETURN n{.p,.`edge-note`} AS m",
        symbols,
    );
    assert!(selective.property_catalog().is_none());
    assert_eq!(
        selective.bindings(),
        [
            GraphVertexReturnBinding::Property(P),
            GraphVertexReturnBinding::Property(NOTE),
            GraphVertexReturnBinding::Vertex,
        ]
    );
    assert_eq!(
        rows(&project(
            &selective,
            vec![int(7), null(), GraphValue::Vertex(VId(42))]
        )),
        vec![vec![map(vec![("edge-note", null()), ("p", int(7))])]]
    );
}

#[test]
fn map_grouping_and_collect_use_native_aggregate_semantics() {
    let query = prepare(
        "MERGE (n:Item {p:7}) RETURN properties(n) AS key,count(*) AS hits,\
         collect(DISTINCT n{.*,p:n.p+1}) AS maps ORDER BY key",
        Catalog("p"),
    );
    assert_eq!(
        query.bindings(),
        [
            GraphVertexReturnBinding::Properties,
            GraphVertexReturnBinding::Property(P)
        ]
    );
    let full = map(vec![("p", int(7)), ("q", null())]);
    let execution = project(&query, vec![full.clone(), int(7)]);
    assert_eq!(
        rows(&execution),
        vec![vec![
            full,
            int(1),
            list(vec![map(vec![("p", int(8)), ("q", null())])])
        ]]
    );
}

#[test]
fn only_complete_map_identity_depends_on_reverse_property_names() {
    let statement = "MERGE (n:Item {p:7}) RETURN properties(n) AS m";
    let plain = prepare(statement, Catalog("p"));
    let renamed = prepare(statement, Catalog("renamed"));
    assert_ne!(plain.canonical_bytes(), renamed.canonical_bytes());
    assert_eq!(
        renamed
            .property_catalog()
            .unwrap()
            .properties
            .get(&P)
            .unwrap(),
        "renamed"
    );
    let named = map(vec![("renamed", int(7))]);
    assert_eq!(
        rows(&project(&renamed, vec![named.clone()])),
        vec![vec![named]]
    );
    let native = PreparedGraphVertexUpsertQuery::prepare_with_property_catalog(
        plain.upsert().clone(),
        vec![GraphVertexReturnBinding::Properties],
        vec![GraphSetProjection::new("m", GraphSetValue::Column(0))],
        GraphSetQuantifier::All,
        Catalog("p").reverse_catalog().unwrap(),
    )
    .unwrap();
    assert_eq!(native.canonical_bytes(), plain.canonical_bytes());

    for statement in [
        "MERGE (n:Item {p:7}) RETURN n.p",
        "MERGE (n:Item {p:7}) RETURN n{.p}",
    ] {
        let first = prepare(statement, Catalog("p"));
        let renamed = prepare(statement, Catalog("renamed"));
        assert!(first.property_catalog().is_none());
        assert_eq!(first.canonical_bytes(), renamed.canonical_bytes());
        assert_eq!(
            first.canonical_bytes(),
            prepare(statement, symbols).canonical_bytes()
        );
    }
    let duplicate = PreparedGraphVertexUpsertQueryText::prepare(statement, R, Catalog("q"))
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap_err();
    assert!(matches!(
        duplicate.kind,
        GraphVertexUpsertTextErrorKind::ReturnBuild(
            GraphVertexUpsertQueryBuildError::PropertyMapCatalog
        )
    ));
}

#[test]
fn malformed_complete_map_inputs_refuse_before_grouping_or_limit_zero() {
    let query = prepare(
        "MERGE (n:Item {p:7}) RETURN collect(properties(n)) AS maps LIMIT 0",
        Catalog("p"),
    );
    let unused = PreparedGraphVertexUpsertQuery::prepare_with_property_catalog(
        query.upsert().clone(),
        vec![GraphVertexReturnBinding::Properties],
        vec![GraphSetProjection::new("one", GraphSetValue::Value(int(1)))],
        GraphSetQuantifier::All,
        Catalog("p").reverse_catalog().unwrap(),
    )
    .unwrap()
    .with_page(0, Some(0));
    for query in [&query, &unused] {
        for value in [
            int(1),
            null(),
            list(vec![]),
            GraphValue::Map {
                keys: vec!["p".into()].into_boxed_slice(),
                values: Box::new([]),
            },
            GraphValue::Map {
                keys: vec!["q".into(), "p".into()].into_boxed_slice(),
                values: vec![int(1), int(2)].into_boxed_slice(),
            },
            GraphValue::Map {
                keys: vec!["p".into(), "p".into()].into_boxed_slice(),
                values: vec![int(1), int(2)].into_boxed_slice(),
            },
            // A stored List is a property value (fgdb-k3nub); a graph
            // element, alone or inside a list, never is.
            map(vec![("p", GraphValue::Vertex(VId(1)))]),
            map(vec![("p", list(vec![GraphValue::Vertex(VId(1))]))]),
        ] {
            assert!(matches!(
                query.project_governed::<(), ()>(
                    vec![value],
                    GqlExecutionStats::default(),
                    GlaExecutionStats::default(),
                    policy(),
                    || Ok(())
                ),
                Err(GqlQueryError::Source(
                    GraphSetExecutionError::InputSchema { .. }
                ))
            ));
        }
        assert!(project(query, vec![map(vec![])]).value.is_empty());
        assert!(matches!(
            query.project_governed::<(), ()>(
                vec![],
                GqlExecutionStats::default(),
                GlaExecutionStats::default(),
                policy(),
                || Ok(())
            ),
            Err(GqlQueryError::Source(
                GraphSetExecutionError::InputSchema { .. }
            ))
        ));
    }
}

#[test]
fn final_map_validation_and_projection_share_prefix_allowances_and_cancellation() {
    let query = prepare(
        "MERGE (n:Item {p:7}) RETURN properties(n) AS m",
        Catalog("p"),
    );
    let input = map(vec![("p", int(7)), ("q", text(&"payload".repeat(64)))]);
    let selection = GqlExecutionStats {
        snapshot_records: 3,
        result_rows: 1,
    };
    let prefix = GlaExecutionStats {
        work_units: 19,
        scratch_entries: 7,
    };
    let checkpoints = Cell::new(0usize);
    let result = query
        .project_governed::<(), usize>(vec![input.clone()], selection, prefix, policy(), || {
            checkpoints.set(checkpoints.get() + 1);
            Ok(())
        })
        .unwrap();
    assert_eq!(result.rows.snapshot_records, 3);
    assert_eq!(result.rows.result_rows, 1);
    let exact = GqlQueryPolicy::new(
        3,
        1,
        result.evaluator.work_units,
        result.evaluator.scratch_entries,
    );
    let execute = |policy| {
        query.project_governed::<(), ()>(vec![input.clone()], selection, prefix, policy, || Ok(()))
    };
    assert_eq!(execute(exact).unwrap(), result);
    let mut less_work = exact;
    less_work.evaluator.max_work_units -= 1;
    assert!(matches!(
        execute(less_work),
        Err(GqlQueryError::Evaluator(_))
    ));
    let mut less_scratch = exact;
    less_scratch.evaluator.max_scratch_entries -= 1;
    assert!(matches!(
        execute(less_scratch),
        Err(GqlQueryError::Evaluator(_))
    ));
    for stop in 0..checkpoints.get() {
        let mut at = 0;
        let failed = query.project_governed::<(), usize>(
            vec![input.clone()],
            selection,
            prefix,
            policy(),
            || {
                let current = at;
                at += 1;
                if current == stop { Err(stop) } else { Ok(()) }
            },
        );
        assert!(matches!(failed, Err(GqlQueryError::Interrupted(index)) if index == stop));
    }

    let values = prepare(
        "MERGE (n:Item {p:7}) RETURN properties(NULL) AS m,keys(NULL) AS k,properties({p:3}) AS literal,n{} AS empty",
        symbols,
    );
    assert_eq!(values.bindings(), [GraphVertexReturnBinding::Vertex]);
    assert_eq!(
        rows(&project(&values, vec![GraphValue::Vertex(VId(42))])),
        vec![vec![null(), null(), map(vec![("p", int(3))]), map(vec![])]]
    );
}

// fgdb-iufb7: labels() of the MERGE vertex is one host-read list cell, shared
// by every mention. The definition binds the reverse label names (and not the
// property names RETURN never reads), and the final projection refuses a host
// cell that is not a list of text.
#[test]
fn merge_labels_bind_reverse_label_names_and_refuse_malformed_host_cells() {
    #[derive(Clone, Copy)]
    struct Labels(&'static str, &'static str);
    impl GraphSymbolResolver for Labels {
        fn resolve_symbol(&mut self, kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
            symbols(kind, name)
        }
        fn reverse_catalog(&self) -> Option<ReverseSymbolCatalog> {
            let mut catalog = ReverseSymbolCatalog::new();
            catalog.insert_label(LabelId(1), self.0);
            catalog.insert_property(P, self.1);
            catalog.insert_property(Q, "q");
            Some(catalog)
        }
    }
    let statement =
        "MERGE (n:Item {p:7}) ON CREATE SET n.q=1 RETURN labels(n) AS names,labels(n) AS again";
    let query = prepare(statement, Labels("Item", "p"));
    assert_eq!(query.bindings(), [GraphVertexReturnBinding::Labels]);
    assert_eq!(
        query
            .property_catalog()
            .unwrap()
            .labels
            .get(&LabelId(1))
            .unwrap(),
        "Item"
    );
    assert_ne!(
        query.canonical_bytes(),
        prepare(statement, Labels("Renamed", "p")).canonical_bytes()
    );
    // Duplicate property names matter only to a complete property map.
    assert_eq!(
        query.canonical_bytes(),
        prepare(statement, Labels("Item", "q")).canonical_bytes()
    );
    let names = list(vec![text("Item"), text("Other")]);
    assert_eq!(
        rows(&project(&query, vec![names.clone()])),
        vec![vec![names.clone(), names]]
    );
    for malformed in [
        null(),
        text("Item"),
        map(vec![]),
        list(vec![int(1)]),
        list(vec![list(vec![])]),
    ] {
        assert!(matches!(
            query.project_governed::<(), ()>(
                vec![malformed],
                GqlExecutionStats::default(),
                GlaExecutionStats::default(),
                policy(),
                || Ok(())
            ),
            Err(GqlQueryError::Source(
                GraphSetExecutionError::InputSchema { .. }
            ))
        ));
    }
}
