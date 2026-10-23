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
