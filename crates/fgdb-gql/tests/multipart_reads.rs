//! Native multi-part reads execute real GLA leaves through one governed source.

use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GraphValue, GraphValueRow};
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryExecution, GqlQueryPolicy,
    GraphSetExecutionError, GraphSymbol, GraphSymbolKind, PreparedGraphSet,
    PreparedGraphSetText,
};
use fgdb_types::{CanonicalScalar, VId};
use std::cell::RefCell;
use std::collections::BTreeSet;

const P: PropertyKeyId = PropertyKeyId(1);
const R: RelationId = RelationId(1);
const EDGES: [(VId, RelationId, VId); 5] = [
    (VId(1), R, VId(2)),
    (VId(1), R, VId(2)),
    (VId(1), R, VId(3)),
    (VId(2), R, VId(3)),
    (VId(3), R, VId(4)),
];
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        _ => None,
    }
}
fn wide() -> GqlQueryPolicy {
    GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX)
}
fn prepare(text: &str) -> PreparedGraphSet {
    PreparedGraphSetText::prepare(text, symbols).unwrap()
        .bind_parameters(&GqlParameters::new()).unwrap()
}
fn run(
    query: &PreparedGraphSet,
    policy: GqlQueryPolicy,
    checkpoint: &mut impl FnMut() -> Result<(), usize>,
) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<GraphSetExecutionError<&'static str>, usize>> {
    let values = [1, 2, 2, 3].map(CanonicalScalar::Int);
    let checkpoint = RefCell::new(checkpoint);
    query.execute_governed(
        policy,
        |pattern, remaining| pattern.plan().execute_governed_with_properties(
            (values.len() + EDGES.len()) as u64,
            (1..=4).map(VId),
            EDGES,
            |vid, tests| Ok::<_, &'static str>(tests.iter().all(|test| {
                test.matches(&[], &[(P, values[vid.0 as usize - 1].clone())])
            })),
            |vid, key| Ok((key == P).then(|| &values[vid.0 as usize - 1])),
            remaining,
            || (checkpoint.borrow_mut())(),
        ),
        || (checkpoint.borrow_mut())(),
    )
}
fn execute(text: &str) -> Vec<GraphValueRow> {
    run(&prepare(text), wide(), &mut || Ok(())).unwrap().value
}
fn vertices(values: &[u128]) -> GraphValueRow {
    GraphValueRow::from_owned_values(values.iter().map(|id| GraphValue::Vertex(VId(*id))).collect())
}
fn scalar_vertex(value: i64, vid: u128) -> GraphValueRow {
    GraphValueRow::from_owned_values(vec![
        GraphValue::Scalar(CanonicalScalar::Int(value)), GraphValue::Vertex(VId(vid)),
    ])
}

#[test]
fn required_continuation_preserves_parallel_edges_and_applies_input_page_first() {
    // This exact form used to be on with_pipelines' refusal list. Independent
    // edge enumeration determines its positive result, including parallel edges.
    let query = prepare("MATCH (n) WITH n MATCH (n)-[:R]->(m) RETURN n,m ORDER BY n,m");
    let result = run(&query, wide(), &mut || Ok(())).unwrap();
    let expected: Vec<_> = EDGES.iter().map(|(a, _, b)| vertices(&[a.0, b.0])).collect();
    assert_eq!(result.value, expected);
    assert_eq!(result.rows.snapshot_records, 18);
    assert_eq!(query.operand_count(), 2);
    assert_eq!(
        execute("MATCH (n) WITH n ORDER BY n LIMIT 1 MATCH (n)-[:R]->(m) RETURN m ORDER BY m"),
        vec![vertices(&[2]), vertices(&[2]), vertices(&[3])],
    );
    assert_eq!(
        execute("MATCH (n) WITH n ORDER BY n LIMIT 1 WHERE n IS NULL MATCH (n)-[:R]->(m) RETURN m"),
        Vec::<GraphValueRow>::new(),
    );
}

#[test]
fn renamed_identity_and_carried_scalar_survive_three_graph_parts() {
    let text = "MATCH (n) WITH n AS root,n.p AS score WHERE score=1 \
        MATCH (root)-[:R]->(m) WITH m AS root,score+1 AS score \
        MATCH (root)-[:R]->(destination) RETURN score,destination ORDER BY destination";
    let mut expected = Vec::new();
    for &(source, _, middle) in &EDGES {
        if source != VId(1) { continue; }
        for &(next, _, destination) in &EDGES {
            if middle == next { expected.push(scalar_vertex(2, destination.0)); }
        }
    }
    expected.sort();
    assert_eq!(execute(text), expected);
    assert_eq!(
        execute("MATCH (n) WITH n AS owner MATCH (owner) RETURN owner.p AS p ORDER BY p"),
        [1, 2, 2, 3].map(|n| GraphValueRow::from_owned_values(vec![
            GraphValue::Scalar(CanonicalScalar::Int(n)),
        ])).to_vec(),
    );
}

#[test]
fn repeated_property_values_join_as_bags_and_null_unwind_keys_never_match() {
    let expected = vec![scalar_vertex(2, 2), scalar_vertex(2, 2), scalar_vertex(2, 3), scalar_vertex(2, 3)];
    assert_eq!(execute(
        "MATCH (n) WITH n.p AS wanted WHERE wanted=2 MATCH (m {p:wanted}) \
         RETURN wanted,m ORDER BY m"
    ), expected);
    assert_eq!(execute(
        "UNWIND [2,2,NULL] AS wanted WITH wanted MATCH (m {p:wanted}) \
         RETURN wanted,m ORDER BY m"
    ), expected);
    assert_eq!(execute(
        "MATCH (n) WITH DISTINCT n.p AS wanted WHERE wanted=2 MATCH (m {p:wanted}) \
         RETURN wanted,m ORDER BY m"
    ), vec![scalar_vertex(2, 2), scalar_vertex(2, 3)]);
}

#[test]
fn null_imports_from_optional_match_are_not_rebound_to_arbitrary_vertices() {
    assert_eq!(execute(
        "MATCH (n) OPTIONAL MATCH (n)-[:R]->(m) WITH m \
         MATCH (m)-[:R]->(destination) RETURN destination ORDER BY destination"
    ), vec![vertices(&[3]), vertices(&[3]), vertices(&[4]), vertices(&[4])]);
}

#[test]
fn one_parameter_and_catalog_contract_spans_all_parts_and_rebinding() {
    let text = "\u{2003}MATCH (n) WHERE n.p=$start WITH n LIMIT $top \
        MATCH (n)-[:R]->(m) WHERE m.p >= $min RETURN m.p+$add AS score";
    let mut calls = BTreeSet::new();
    let template = PreparedGraphSetText::prepare(text, |kind, name| {
        assert!(calls.insert((kind, name.to_owned())), "duplicate catalog callback");
        symbols(kind, name)
    }).unwrap();
    assert_eq!(calls.len(), 2);
    assert_eq!(template.parameter_schema().len(), 4);
    let before = template.canonical_template_bytes();
    let args = GqlParameters::new().with_int64("start", 1).unwrap()
        .with_uint64("top", 1).unwrap().with_int64("min", 2).unwrap();
    let missing = template.bind_parameters(&args).unwrap_err();
    assert_eq!(missing.offset, text.find("$add").unwrap());
    let a = template.bind_parameters(&args.clone().with_int64("add", 3).unwrap()).unwrap();
    let b = template.bind_parameters(&args.with_int64("add", 4).unwrap()).unwrap();
    assert_ne!(a.canonical_bytes(), b.canonical_bytes());
    assert_eq!(template.canonical_template_bytes(), before);
    let result = run(&a, wide(), &mut || Ok(())).unwrap();
    assert_eq!(result.value.len(), 3);
    assert!(result.value.iter().all(|row| row.get(0).unwrap().as_scalar() == Some(&CanonicalScalar::Int(5))));
    assert!(!format!("{template:?}").contains("score"));
}

#[test]
fn all_sources_and_fallible_projections_execute_even_after_empty_input_or_limit_zero() {
    for text in [
        "MATCH (n) WITH n LIMIT 0 MATCH (n) RETURN n",
        "MATCH (n) WITH n MATCH (n) RETURN n LIMIT 0",
    ] {
        let query = prepare(text);
        let mut calls = 0;
        let result = query.execute_governed(wide(), |_, _| {
            calls += 1;
            if calls == 1 {
                Ok(GqlQueryExecution {
                    value: Vec::new(), rows: fgdb_gql::GqlExecutionStats {
                        snapshot_records: 0, result_rows: 0,
                    },
                    evaluator: fgdb_gql::GlaExecutionStats::default(),
                })
            } else { Err(GqlQueryError::Source("second-source-error")) }
        }, || Ok::<_, usize>(()));
        assert!(matches!(result, Err(GqlQueryError::Source(GraphSetExecutionError::Source("second-source-error")))));
        assert_eq!(calls, 2);
    }
    let late = prepare("MATCH (n) WITH n LIMIT 1 MATCH (n)-[:R]->(m) RETURN 1/(m.p-m.p) AS bad LIMIT 0");
    assert!(matches!(run(&late, wide(), &mut || Ok(())),
        Err(GqlQueryError::Source(GraphSetExecutionError::Projection { .. }))));
}

#[test]
fn cumulative_budgets_and_every_cancellation_checkpoint_cover_the_whole_join() {
    let query = prepare("MATCH (n) WITH n LIMIT 1 MATCH (n)-[:R]->(m) RETURN m");
    let mut calls = 0;
    let measured = run(&query, wide(), &mut || { calls += 1; Ok(()) }).unwrap();
    let caps = [measured.rows.snapshot_records, measured.rows.result_rows,
        measured.evaluator.work_units, measured.evaluator.scratch_entries];
    assert_eq!(run(&query, GqlQueryPolicy::new(caps[0], caps[1], caps[2], caps[3]), &mut || Ok(())).unwrap(), measured);
    for dimension in 0..4 {
        let mut limits = caps;
        assert!(limits[dimension] > 0);
        limits[dimension] -= 1;
        assert!(run(&query, GqlQueryPolicy::new(limits[0], limits[1], limits[2], limits[3]), &mut || Ok(())).is_err());
    }
    for stop in 1..=calls {
        let mut seen = 0;
        let result = run(&query, wide(), &mut || {
            seen += 1;
            if seen == stop { Err(stop) } else { Ok(()) }
        });
        assert!(matches!(result, Err(GqlQueryError::Interrupted(value)) if value == stop));
        assert_eq!(seen, stop);
    }
}

#[test]
fn illegal_imports_scopes_and_statement_wide_depth_refuse_before_catalog() {
    for text in [
        "MATCH (n) WITH n.p AS n MATCH (n)-[:R]->(m) RETURN m",
        "MATCH (n) WITH n AS kept MATCH (m) RETURN n",
        "MATCH (n) WITH n AS kept MATCH (m) RETURN kept.p AS value",
        "MATCH (n) WITH n AS kept MATCH (m) OPTIONAL MATCH (kept)-[:R]->(x) RETURN x",
        "MATCH (n) WITH n AS kept MATCH (m) WHERE EXISTS { MATCH (kept)-[:R]->(x) } RETURN m",
        "MATCH (n) WITH n MATCH (n) RETURN n+1 AS bad",
    ] {
        let mut calls = 0;
        assert!(PreparedGraphSetText::prepare(text, |kind, name| {
            calls += 1; symbols(kind, name)
        }).is_err(), "{text}");
        assert_eq!(calls, 0, "{text}");
    }
    let too_deep = format!("MATCH (n){} RETURN n", " WITH n MATCH (n)".repeat(fgdb_gql::MAX_GRAPH_SET_DEPTH));
    let mut calls = 0;
    assert!(PreparedGraphSetText::prepare(&too_deep, |kind, name| {
        calls += 1; symbols(kind, name)
    }).is_err());
    assert_eq!(calls, 0);
}

#[test]
fn set_arms_and_literal_routing_keep_their_original_boundaries() {
    assert_eq!(execute(
        "(MATCH (n) WITH n ORDER BY n LIMIT 1 MATCH (n)-[:R]->(m) RETURN m LIMIT 1) \
         UNION ALL (MATCH (n) WITH n WHERE n.p=3 MATCH (n) RETURN n AS m) ORDER BY m"
    ), vec![vertices(&[2]), vertices(&[4])]);
    let literal = "RETURN 'WITH n MATCH (m)' AS text";
    let template = PreparedGraphSetText::prepare(literal, |_, _| None).unwrap();
    assert_eq!(template.columns(), &["text"]);
    assert_eq!(execute("MATCH (n) WITH n MATCH (n) RETURN *").len(), 4);
}
