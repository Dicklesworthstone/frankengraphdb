//! Aggregate write RETURN consumes the statement's frozen occurrence bag.
//! Expectations enumerate those occurrences, independently of the reducer.
//! The source callbacks below execute native GLA, never a second matcher.

use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GraphValue, GraphValueRow};
use fgdb_gql::insertion::{GraphInsertPolicy, GraphInsertRequest};
use fgdb_gql::{
    GqlBudgetDimension, GqlParameters, GqlQueryError, GqlQueryExecution, GqlQueryPolicy,
    GraphInsertQueryBatch, GraphInsertQueryError, GraphMutationPolicy, GraphMutationQueryBatch,
    GraphMutationQueryError, GraphSetColumnType, GraphSymbol, GraphSymbolKind,
    PreparedGraphInsertQuery, PreparedGraphInsertQueryText, PreparedGraphMutationQueryText,
    PreparedGraphVertexUpsertQueryText,
};
use fgdb_types::{CanonicalF64, CanonicalScalar, EId, VId};
use std::cell::Cell;
use std::collections::BTreeMap;

const R: RelationId = RelationId(1);
const ITEM: LabelId = LabelId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);
type CreateResult =
    Result<GraphInsertQueryBatch, GqlQueryError<GraphInsertQueryError<(), ()>, usize>>;
type Props = BTreeMap<(VId, PropertyKeyId), CanonicalScalar>;

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Label, "Item") => Some(GraphSymbol::Label(ITEM)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
        _ => None,
    }
}

fn policy(rows: u64) -> GraphInsertPolicy {
    GraphInsertPolicy::new(GqlQueryPolicy::new(0, rows, 2_000_000, 1_000_000), 100, 100)
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

fn prepare(text: &str) -> PreparedGraphInsertQuery {
    PreparedGraphInsertQueryText::prepare(text, R, symbols)
        .expect(text)
        .bind_parameters(&GqlParameters::new())
        .expect(text)
}

fn create(
    query: &PreparedGraphInsertQuery,
    policy: GraphInsertPolicy,
    checkpoint: impl FnMut() -> Result<(), usize>,
) -> CreateResult {
    query.execute_governed(
        policy,
        |_, _| -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<(), usize>> {
            panic!("source-free CREATE grouping must not invent a graph scan")
        },
        identity,
        checkpoint,
    )
}

fn int(value: i64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(value))
}

fn null() -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Null)
}

fn list(values: Vec<GraphValue>) -> GraphValue {
    GraphValue::List(values.into_boxed_slice())
}

fn rows(execution: &GqlQueryExecution<GraphValueRow>) -> Vec<Vec<GraphValue>> {
    execution
        .value
        .iter()
        .map(|row| row.values().to_vec())
        .collect()
}

#[test]
fn create_groups_actual_frozen_identities_and_null_keys() {
    let query = prepare(
        "UNWIND [1,1,2,NULL] AS x CREATE (n:Item {p:x}) \
         RETURN n.p AS key,count(*) AS occurrences,count(n.p) AS nonnull, \
         sum(n.p) AS total,collect(n) AS nodes,collect(DISTINCT n.p) AS values \
         ORDER BY key NULLS LAST",
    );
    assert_eq!(
        query.column_types(),
        &[
            GraphSetColumnType::Scalar,
            GraphSetColumnType::Scalar,
            GraphSetColumnType::Scalar,
            GraphSetColumnType::Scalar,
            GraphSetColumnType::List,
            GraphSetColumnType::List,
        ],
    );
    let batch = create(&query, policy(3), || Ok(())).unwrap();
    assert_eq!(
        rows(batch.returning()),
        vec![
            vec![
                int(1),
                int(2),
                int(2),
                int(2),
                list(vec![
                    GraphValue::Vertex(VId(100)),
                    GraphValue::Vertex(VId(116))
                ]),
                list(vec![int(1)]),
            ],
            vec![
                int(2),
                int(1),
                int(1),
                int(2),
                list(vec![GraphValue::Vertex(VId(132))]),
                list(vec![int(2)]),
            ],
            vec![
                null(),
                int(1),
                int(0),
                null(),
                list(vec![GraphValue::Vertex(VId(148))]),
                list(vec![]),
            ],
        ],
    );
    assert_eq!(batch.insertion().stats().created_vertices, 4);
    assert_eq!(batch.returning().rows.result_rows, 3);
    assert_eq!(batch.returning().rows.snapshot_records, 0);
}

#[test]
fn all_native_scalar_and_collection_aggregates_keep_occurrence_and_distinct_semantics() {
    let query = prepare(
        "UNWIND [1,3,3,NULL] AS x CREATE (n {p:x}) \
         RETURN count(*) AS rows,count(n.p) AS nonnull,count(DISTINCT n.p) AS unique, \
         sum(n.p) AS total,sum(DISTINCT n.p) AS unique_total,min(n.p) AS lo,max(n.p) AS hi, \
         collect(n.p) AS values,collect(DISTINCT n.p) AS unique_values",
    );
    let batch = create(&query, policy(1), || Ok(())).unwrap();
    assert_eq!(
        rows(batch.returning()),
        vec![vec![
            int(4),
            int(3),
            int(2),
            int(7),
            int(4),
            int(1),
            int(3),
            list(vec![int(1), int(3), int(3)]),
            list(vec![int(1), int(3)]),
        ]],
    );
    assert_eq!(batch.insertion().stats().created_vertices, 4);

    // Exact native binary64 accumulation keeps the small term, even when an
    // ordinary left-to-right floating sum would lose it.
    let floating = create(
        &prepare(
            "UNWIND [1.0,9007199254740992.0,-9007199254740992.0] AS x \
             CREATE (n {p:x}) RETURN sum(n.p) AS total,avg(n.p) AS mean",
        ),
        policy(1),
        || Ok(()),
    )
    .unwrap();
    assert_eq!(
        rows(floating.returning()),
        vec![vec![
            GraphValue::Scalar(CanonicalScalar::Float(CanonicalF64::new(1.0))),
            GraphValue::Scalar(CanonicalScalar::Float(CanonicalF64::new(1.0 / 3.0))),
        ]],
    );
}

#[test]
fn empty_keyless_and_grouped_sources_keep_native_empty_group_semantics() {
    let global = create(
        &prepare(
            "UNWIND [] AS x CREATE (n {p:x}) \
             RETURN count(*) AS rows,count(n.p) AS nonnull,sum(n.p) AS total, \
             min(n.p) AS lo,max(n.p) AS hi,collect(n) AS nodes,avg(n.p) AS mean",
        ),
        policy(1),
        || Ok(()),
    )
    .unwrap();
    assert_eq!(global.insertion().stats().created_vertices, 0);
    assert_eq!(
        rows(global.returning()),
        vec![vec![
            int(0),
            int(0),
            null(),
            null(),
            null(),
            list(vec![]),
            null()
        ]],
    );
    let grouped = create(
        &prepare("UNWIND [] AS x CREATE (n {p:x}) RETURN n.p AS p,count(*) AS rows"),
        policy(0),
        || Ok(()),
    )
    .unwrap();
    assert!(grouped.returning().value.is_empty());
    assert_eq!(grouped.insertion().stats().created_vertices, 0);
}

fn mutate(
    text: &str,
    vertices: &[VId],
    edges: &[(VId, RelationId, VId)],
    properties: &Props,
) -> Result<GraphMutationQueryBatch, GqlQueryError<GraphMutationQueryError<()>, ()>> {
    let query = PreparedGraphMutationQueryText::prepare(text, R, symbols)
        .expect(text)
        .bind_parameters(&GqlParameters::new())
        .expect(text);
    query.execute_governed(
        GraphMutationPolicy::new(GqlQueryPolicy::new(1_000, 1, 2_000_000, 1_000_000), 100),
        |selection, budget| {
            selection.plan().execute_governed_with_properties(
                (vertices.len() + edges.len()) as u64,
                vertices.iter().copied(),
                edges.iter().copied(),
                |vertex, predicates| {
                    Ok::<_, ()>(predicates.iter().all(|predicate| {
                        predicate.matches_borrowed(
                            [],
                            properties.iter().filter_map(|(&(owner, key), value)| {
                                (owner == vertex).then_some((key, value))
                            }),
                        )
                    }))
                },
                |vertex, key| Ok(properties.get(&(vertex, key))),
                budget,
                || Ok::<_, ()>(()),
            )
        },
        || Ok(()),
    )
}

#[test]
fn mutation_groups_post_statement_values_and_duplicate_matches_without_rematching() {
    let properties = Props::from([
        ((VId(1), P), CanonicalScalar::Int(5)),
        ((VId(2), P), CanonicalScalar::Int(7)),
        ((VId(3), P), CanonicalScalar::Int(11)),
    ]);
    let edges = [
        (VId(1), R, VId(2)),
        (VId(1), R, VId(2)),
        (VId(1), R, VId(3)),
    ];
    let batch = mutate(
        "MATCH (a)-[:R]->(b) SET a.p=a.p+1,b.q=0 \
         RETURN a.p AS p,count(*) AS matches,count(DISTINCT b) AS neighbors, \
         sum(b.p) AS total,collect(b.q) AS assigned",
        &[VId(1), VId(2), VId(3)],
        &edges,
        &properties,
    )
    .unwrap();
    assert_eq!(
        rows(batch.returning()),
        vec![vec![
            int(6),
            int(3),
            int(2),
            int(25),
            list(vec![int(0), int(0), int(0)])
        ]],
    );
    assert_eq!(batch.mutation().stats().effects, 3);
    assert_eq!(batch.mutation().stats().selection.result_rows, 3);
    assert_eq!(batch.returning().rows.result_rows, 1);
    assert_eq!(batch.returning().rows.snapshot_records, 6);

    let empty = mutate(
        "MATCH (n {p:99}) SET n.q=1 RETURN count(*) AS rows,sum(n.p) AS total",
        &[VId(1), VId(2), VId(3)],
        &edges,
        &properties,
    )
    .unwrap();
    assert_eq!(rows(empty.returning()), vec![vec![int(0), null()]]);
    assert_eq!(empty.mutation().stats().effects, 0);
}

#[test]
fn ordering_paging_and_zero_final_allowance_never_limit_effects() {
    let query = prepare(
        "UNWIND [3,1,2,1] AS x CREATE (n {p:x}) \
         RETURN n.p AS p,count(*) AS rows ORDER BY p DESC SKIP 1 LIMIT 1",
    );
    let batch = create(&query, policy(1), || Ok(())).unwrap();
    assert_eq!(rows(batch.returning()), vec![vec![int(2), int(1)]]);
    assert_eq!(batch.insertion().stats().created_vertices, 4);

    let hidden = prepare(
        "UNWIND [3,1,2,1] AS x CREATE (n {p:x}) \
         RETURN count(*) AS rows,sum(n.p) AS total LIMIT 0",
    );
    let batch = create(&hidden, policy(0), || Ok(())).unwrap();
    assert!(batch.returning().value.is_empty());
    assert_eq!(batch.insertion().stats().created_vertices, 4);

    let error = create(
        &prepare("UNWIND [1,2,3] AS x CREATE (n {p:x}) RETURN count(*) AS rows"),
        policy(0),
        || Ok(()),
    )
    .unwrap_err();
    assert!(matches!(
        error,
        GqlQueryError::Rows(error) if error.dimension == GqlBudgetDimension::ResultRows
    ));
}

#[test]
fn late_argument_and_output_conversion_errors_survive_limit_zero() {
    for statement in [
        "UNWIND [1,2] AS x CREATE (n {p:x}) RETURN sum(10/(2-n.p)) AS broken LIMIT 0",
        "UNWIND [1,'bad'] AS x CREATE (n {p:x}) RETURN sum(n.p) AS broken LIMIT 0",
        "UNWIND [9223372036854775807,1] AS x CREATE (n {p:x}) RETURN sum(n.p) AS wide LIMIT 0",
        "UNWIND [1,3] AS x CREATE (n {p:x}) RETURN avg(n.p) AS exact LIMIT 0",
    ] {
        let error = create(&prepare(statement), policy(0), || Ok(())).unwrap_err();
        assert!(
            matches!(
                error,
                GqlQueryError::Source(GraphInsertQueryError::Returning(_))
            ),
            "{statement}: {error:?}"
        );
    }
}

#[test]
fn grouping_continues_the_complete_write_work_and_scratch_allowances() {
    let query = prepare(
        "UNWIND [1,2,2,3] AS x CREATE (n {p:x}) \
         RETURN count(*) AS rows,sum(n.p) AS total,collect(n) AS nodes",
    );
    let baseline = create(&query, policy(1), || Ok(())).unwrap();
    let stats = baseline.returning().evaluator;
    let exact = GraphInsertPolicy::new(
        GqlQueryPolicy::new(0, 1, stats.work_units, stats.scratch_entries),
        100,
        100,
    );
    let repeated = create(&query, exact, || Ok(())).unwrap();
    assert_eq!(repeated.returning(), baseline.returning());
    assert_eq!(repeated.insertion(), baseline.insertion());
    for policy in [
        GraphInsertPolicy::new(
            GqlQueryPolicy::new(0, 1, stats.work_units - 1, stats.scratch_entries),
            100,
            100,
        ),
        GraphInsertPolicy::new(
            GqlQueryPolicy::new(0, 1, stats.work_units, stats.scratch_entries - 1),
            100,
            100,
        ),
    ] {
        assert!(matches!(
            create(&query, policy, || Ok(())),
            Err(GqlQueryError::Evaluator(_))
        ));
    }
}

#[test]
fn cancelled_grouping_retries_from_frozen_definition_without_partial_output() {
    let query =
        prepare("UNWIND [1,2,2,3] AS x CREATE (n {p:x}) RETURN n.p AS p,collect(n) AS nodes");
    let visits = Cell::new(0_usize);
    let expected = create(&query, policy(3), || {
        visits.set(visits.get() + 1);
        Ok(())
    })
    .unwrap();
    for stop in [0, visits.get() / 2, visits.get() - 1] {
        let observed = Cell::new(0_usize);
        let result = create(&query, policy(3), || {
            let at = observed.get();
            observed.set(at + 1);
            if at == stop { Err(stop) } else { Ok(()) }
        });
        assert!(matches!(result, Err(GqlQueryError::Interrupted(at)) if at == stop));
        let retried = create(&query, policy(3), || Ok(())).unwrap();
        assert_eq!(retried.returning(), expected.returning());
        assert_eq!(retried.insertion(), expected.insertion());
    }
}

#[test]
fn malformed_aggregate_forms_refuse_before_catalog_access_for_every_write_family() {
    for text in [
        "CREATE (n:Item {p:1}) RETURN sum(n)",
        "CREATE (n:Item {p:1}) RETURN count(DISTINCT *)",
        "CREATE (n:Item {p:1}) RETURN sum(n.p)+1",
        "CREATE (n:Item {p:1}) RETURN count(*) AS same,max(n.p) AS same",
    ] {
        let calls = Cell::new(0);
        assert!(
            PreparedGraphInsertQueryText::prepare(text, R, |kind, name| {
                calls.set(calls.get() + 1);
                symbols(kind, name)
            })
            .is_err(),
            "{text}"
        );
        assert_eq!(calls.get(), 0, "{text}");
    }
    let calls = Cell::new(0);
    assert!(
        PreparedGraphMutationQueryText::prepare(
            "MATCH (n:Item) SET n.p=2 RETURN sum(n)",
            R,
            |kind, name| {
                calls.set(calls.get() + 1);
                symbols(kind, name)
            },
        )
        .is_err()
    );
    assert_eq!(calls.get(), 0);
    assert!(
        PreparedGraphVertexUpsertQueryText::prepare(
            "MERGE (n:Item {p:1}) RETURN count(DISTINCT *)",
            R,
            |kind, name| {
                calls.set(calls.get() + 1);
                symbols(kind, name)
            },
        )
        .is_err()
    );
    assert_eq!(calls.get(), 0);
}

#[test]
fn grouping_definition_binds_parameters_once_and_separates_aggregate_semantics() {
    let template = PreparedGraphInsertQueryText::prepare(
        "CREATE (n {p:$p}) RETURN sum(n.p+$step) AS total LIMIT $limit",
        R,
        symbols,
    )
    .unwrap();
    let arguments = GqlParameters::new()
        .with_int64("p", 7)
        .unwrap()
        .with_int64("step", 3)
        .unwrap()
        .with_uint64("limit", 1)
        .unwrap();
    let query = template.bind_parameters(&arguments).unwrap();
    assert_eq!(
        rows(create(&query, policy(1), || Ok(())).unwrap().returning()),
        vec![vec![int(10)]]
    );
    assert_eq!(
        query.canonical_bytes(),
        template
            .bind_parameters(&arguments)
            .unwrap()
            .canonical_bytes()
    );

    let sum = prepare("UNWIND [1,1] AS x CREATE (n {p:x}) RETURN sum(n.p) AS answer");
    let distinct = prepare("UNWIND [1,1] AS x CREATE (n {p:x}) RETURN sum(DISTINCT n.p) AS answer");
    let count = prepare("UNWIND [1,1] AS x CREATE (n {p:x}) RETURN count(n.p) AS answer");
    assert_ne!(sum.canonical_bytes(), distinct.canonical_bytes());
    assert_ne!(sum.canonical_bytes(), count.canonical_bytes());
    let renamed = prepare("UNWIND [1,1] AS x CREATE (n {p:x}) RETURN sum(n.p) AS renamed");
    assert_eq!(sum.canonical_bytes(), renamed.canonical_bytes());
}
