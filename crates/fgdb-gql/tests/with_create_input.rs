//! WITH/UNWIND is the CREATE input relation, not an output page or a loop of
//! separately compiled writes. Expected bags and effects are hand-enumerated.

use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GraphValue, GraphValueRow, PreparedGraphPattern};
use fgdb_gql::insertion::{GraphInsertPolicy, GraphInsertRequest};
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryExecution, GqlQueryPolicy, GraphInsertQueryBatch,
    GraphInsertQueryError, GraphSymbol, GraphSymbolKind, PreparedGraphInsertQuery,
    PreparedGraphInsertQueryText, PreparedGraphInsertText, PreparedGraphWriteScript,
};
use fgdb_types::{CanonicalScalar, EId, VId};
use std::cell::Cell;

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);
const ID_BASE: u128 = 1_u128 << 100;

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Label, "Item") => Some(GraphSymbol::Label(LabelId(1))),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
        _ => None,
    }
}

fn policy() -> GraphInsertPolicy {
    GraphInsertPolicy::new(
        GqlQueryPolicy::new(1_000, 100, 1_000_000, 100_000),
        100,
        100,
    )
}

fn prepare(text: &str) -> PreparedGraphInsertQuery {
    PreparedGraphInsertQueryText::prepare(text, R, symbols)
        .expect(text)
        .bind_parameters(&GqlParameters::new())
        .expect(text)
}

fn no_graph(
    _: &PreparedGraphPattern<GraphValueRow>,
    _: GqlQueryPolicy,
) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<(), ()>> {
    panic!("a source-free WITH must not invent a graph scan")
}

fn execute(
    query: &PreparedGraphInsertQuery,
    policy: GraphInsertPolicy,
    requests: &mut Vec<GraphInsertRequest>,
    checkpoint: impl FnMut() -> Result<(), ()>,
) -> Result<GraphInsertQueryBatch, GqlQueryError<GraphInsertQueryError<(), ()>, ()>> {
    query.execute_governed(
        policy,
        no_graph,
        |request| {
            requests.push(request);
            let id = ID_BASE + requests.len() as u128;
            Ok(match request {
                GraphInsertRequest::Vertex { .. } => ElementId::Vertex(VId(id)),
                GraphInsertRequest::Edge { .. } => ElementId::Edge(EId(id)),
            })
        },
        checkpoint,
    )
}

fn ints(values: &[&[i64]]) -> Vec<GraphValueRow> {
    values
        .iter()
        .map(|row| {
            GraphValueRow::from_owned_values(
                row.iter()
                    .map(|&value| GraphValue::Scalar(CanonicalScalar::Int(value)))
                    .collect(),
            )
        })
        .collect()
}

#[test]
fn computed_with_bindings_rebind_once_and_feed_the_creation_and_return() {
    let text = "WITH $base + 1 AS v WITH v * 2 AS value \
                CREATE (n:Item {p: value}) RETURN n.p AS p, value AS imported";
    let calls = Cell::new(0);
    let prepared = PreparedGraphInsertQueryText::prepare(text, R, |kind, name: &str| {
        calls.set(calls.get() + 1);
        symbols(kind, name)
    })
    .unwrap();
    assert_eq!(prepared.parameter_schema().len(), 1);
    assert_eq!(prepared.parameter_schema()[0].name, "base");
    let resolved = calls.get();
    assert!(resolved > 0);
    let mut previous = None;
    for (base, expected) in [(3, 8), (5, 12), (3, 8)] {
        let arguments = GqlParameters::new().with_int64("base", base).unwrap();
        let bound = prepared.bind_parameters(&arguments).unwrap();
        let repeated = prepared.bind_parameters(&arguments).unwrap();
        assert_eq!(bound.canonical_bytes(), repeated.canonical_bytes());
        if let Some(previous) = previous {
            assert_ne!(bound.canonical_bytes(), previous);
        }
        previous = Some(bound.canonical_bytes());
        let mut requests = Vec::new();
        let output = execute(&bound, policy(), &mut requests, || Ok(())).unwrap();
        assert_eq!(output.returning().value, ints(&[&[expected, expected]]));
        assert_eq!(output.insertion().stats().created_vertices, 1);
        assert_eq!(
            requests,
            vec![GraphInsertRequest::Vertex { row: 0, vertex: 0 }]
        );
    }
    assert_eq!(
        calls.get(),
        resolved,
        "binding and execution do not resolve again"
    );
    assert!(prepared.bind_parameters(&GqlParameters::new()).is_err());
}

#[test]
fn input_filters_distinct_and_windows_select_exact_creation_occurrences() {
    for (text, expected) in [
        (
            "WITH [3, 1, 3, 2] AS data UNWIND data AS x WITH x WHERE x > 1 \
             ORDER BY x DESC SKIP 1 LIMIT 2 CREATE (n {p: x}) RETURN n.p AS p ORDER BY p",
            ints(&[&[2], &[3]]),
        ),
        (
            "WITH [3, 1, 3, 2] AS data UNWIND data AS x WITH DISTINCT x WHERE x > 1 \
             ORDER BY x DESC SKIP 1 LIMIT 2 CREATE (n {p: x}) RETURN n.p AS p ORDER BY p",
            ints(&[&[2]]),
        ),
        (
            "UNWIND [3, 1, 2] AS x WITH x ORDER BY x LIMIT 1 WHERE x > 1 \
             CREATE (n {p: x}) RETURN n.p AS p",
            Vec::new(),
        ),
        (
            "WITH [] AS data UNWIND data AS x CREATE (n {p: x}) RETURN n.p AS p",
            Vec::new(),
        ),
    ] {
        let mut requests = Vec::new();
        let output = execute(&prepare(text), policy(), &mut requests, || Ok(())).unwrap();
        assert_eq!(
            output.insertion().stats().created_vertices,
            expected.len() as u64,
            "{text}"
        );
        assert_eq!(requests.len(), expected.len(), "{text}");
        assert_eq!(output.returning().value, expected, "{text}");
    }
}

#[test]
fn grouped_with_feeds_groups_not_members_and_empty_global_aggregation_is_one_row() {
    let query = prepare(
        "WITH [1, 2, 2, 3] AS data UNWIND data AS x \
         WITH x AS key, count(*) AS occurrences CREATE (n {p: key, q: occurrences}) \
         RETURN n.p AS p, n.q AS q ORDER BY p",
    );
    let mut requests = Vec::new();
    let output = execute(&query, policy(), &mut requests, || Ok(())).unwrap();
    assert_eq!(output.returning().value, ints(&[&[1, 1], &[2, 2], &[3, 1]]));
    assert_eq!(output.insertion().stats().created_vertices, 3);
    let query = prepare(
        "WITH [] AS data UNWIND data AS x WITH count(*) AS total \
         CREATE (n {p: total}) RETURN n.p AS p",
    );
    let output = execute(&query, policy(), &mut Vec::new(), || Ok(())).unwrap();
    assert_eq!(output.returning().value, ints(&[&[0]]));
    assert_eq!(output.insertion().stats().created_vertices, 1);
}

#[test]
fn equal_input_rows_create_distinct_full_width_identities_and_edges() {
    let query = prepare(
        "WITH [2, 2] AS data UNWIND data AS x \
         CREATE (a {p: x})-[r:R {p: x + 1}]->(b {p: x + 2}) RETURN a, b, r",
    );
    let mut requests = Vec::new();
    let output = execute(&query, policy(), &mut requests, || Ok(())).unwrap();
    assert_eq!(output.insertion().stats().created_vertices, 4);
    assert_eq!(output.insertion().stats().created_edges, 2);
    assert_eq!(
        output.returning().value,
        vec![
            GraphValueRow::from_owned_values(vec![
                GraphValue::Vertex(VId(ID_BASE + 1)),
                GraphValue::Vertex(VId(ID_BASE + 2)),
                GraphValue::Edge(EId(ID_BASE + 3)),
            ]),
            GraphValueRow::from_owned_values(vec![
                GraphValue::Vertex(VId(ID_BASE + 4)),
                GraphValue::Vertex(VId(ID_BASE + 5)),
                GraphValue::Edge(EId(ID_BASE + 6)),
            ]),
        ]
    );
    assert_eq!(requests.len(), 6);
}

#[test]
fn only_the_final_result_consumes_the_result_row_allowance() {
    let prefix = "WITH [1, 2, 3] AS data UNWIND data AS x CREATE (n {p: x})";
    let mut budget = policy();
    budget.query = GqlQueryPolicy::new(1_000, 1, 1_000_000, 100_000);
    let output = execute(
        &prepare(&format!("{prefix} RETURN count(*) AS count")),
        budget,
        &mut Vec::new(),
        || Ok(()),
    )
    .unwrap();
    assert_eq!(output.insertion().stats().created_vertices, 3);
    assert_eq!(output.returning().value, ints(&[&[3]]));
    budget.query = GqlQueryPolicy::new(1_000, 0, 1_000_000, 100_000);
    let output = execute(
        &prepare(&format!("{prefix} RETURN n.p AS p LIMIT 0")),
        budget,
        &mut Vec::new(),
        || Ok(()),
    )
    .unwrap();
    assert_eq!(output.insertion().stats().created_vertices, 3);
    assert!(output.returning().value.is_empty());
}

#[test]
fn input_failure_precedes_all_allocation_even_under_a_zero_output_page() {
    let query = prepare(
        "WITH [1, 0] AS data UNWIND data AS x WITH 10 / x AS value \
         CREATE (n {p: value}) RETURN n.p AS p LIMIT 0",
    );
    let mut requests = Vec::new();
    assert!(execute(&query, policy(), &mut requests, || Ok(())).is_err());
    assert!(requests.is_empty());
}

#[test]
fn prefix_scope_and_depth_refuse_before_any_catalog_callback() {
    for text in [
        "WITH missing AS value CREATE (n:Item {p: value}) RETURN n",
        "WITH 1 AS old WITH 2 AS kept CREATE (n:Item {p: old}) RETURN n",
        "WITH 1 AS n CREATE (n:Item) RETURN n",
        "WITH 1 AS x, 2 AS x CREATE (n:Item {p: x}) RETURN n",
        "WITH [] AS data UNWIND data AS x CREATE (n:Item {p: missing}) RETURN n",
        "WITH 1 AS x CREATE (n:Item {p: CASE WHEN true THEN x ELSE missing END}) RETURN n",
        "WHERE true CREATE (n:Item) RETURN n",
    ] {
        assert!(
            PreparedGraphInsertQueryText::prepare(text, R, |_, _: &str| {
                panic!("invalid scope reached catalog: {text}")
            })
            .is_err(),
            "{text}"
        );
    }
    let text = format!(
        "WITH 1 AS x {} CREATE (n:Item {{p:x}}) RETURN n",
        "WITH x AS x ".repeat(fgdb_gql::MAX_GRAPH_SET_DEPTH + 1),
    );
    assert!(
        PreparedGraphInsertQueryText::prepare(&text, R, |_, _: &str| {
            panic!("excessive prefix depth reached catalog")
        })
        .is_err()
    );
}

#[test]
fn cancellation_and_work_scratch_limits_cover_the_whole_prefix_and_write() {
    let query = prepare(
        "WITH [3, 1, 2, 2] AS data UNWIND data AS x WITH DISTINCT x WHERE x > 1 \
         CREATE (n {p:x}) RETURN n.p AS p ORDER BY p",
    );
    let mut checkpoints = 0;
    let baseline = execute(&query, policy(), &mut Vec::new(), || {
        checkpoints += 1;
        Ok(())
    })
    .unwrap();
    assert_eq!(baseline.returning().value, ints(&[&[2], &[3]]));
    let work = baseline.returning().evaluator.work_units;
    let scratch = baseline.returning().evaluator.scratch_entries;
    assert!(work > 0 && scratch > 0 && checkpoints > 0);
    let exact = GraphInsertPolicy::new(GqlQueryPolicy::new(1_000, 100, work, scratch), 100, 100);
    assert!(execute(&query, exact, &mut Vec::new(), || Ok(())).is_ok());
    for (work, scratch) in [(work - 1, scratch), (work, scratch - 1)] {
        let budget =
            GraphInsertPolicy::new(GqlQueryPolicy::new(1_000, 100, work, scratch), 100, 100);
        assert!(execute(&query, budget, &mut Vec::new(), || Ok(())).is_err());
    }
    for stop in 1..=checkpoints {
        let mut reached = 0;
        let error = execute(&query, policy(), &mut Vec::new(), || {
            reached += 1;
            if reached == stop { Err(()) } else { Ok(()) }
        })
        .unwrap_err();
        assert!(
            matches!(error, GqlQueryError::Interrupted(())),
            "checkpoint {stop}: {error:?}"
        );
    }
}

#[test]
fn no_return_script_preflight_uses_the_same_imported_scope() {
    let script = "WITH $value AS x CREATE (n:Item {p:x}); \
                  WITH $value + 1 AS y INSERT (m:Item {p:y})";
    assert!(!PreparedGraphInsertQueryText::has_return_clause(script).unwrap());
    let prepared = PreparedGraphWriteScript::prepare(script, R, symbols).unwrap();
    let bound = prepared
        .bind_parameters(&GqlParameters::new().with_int64("value", 7).unwrap())
        .unwrap();
    assert_eq!(bound.statements().len(), 2);
    assert!(PreparedGraphInsertText::prepare("WITH 42 AS x CREATE (n {p:x})", R, symbols).is_ok());
    assert!(
        PreparedGraphWriteScript::prepare(
            "CREATE (n:Item); WITH missing AS x CREATE (m {p:x})",
            R,
            |_, _| panic!("whole-script preflight reached the first statement's catalog"),
        )
        .is_err()
    );
    assert!(PreparedGraphWriteScript::prepare("WITH 1 AS x MERGE (n {p:x})", R, symbols).is_err());
}
