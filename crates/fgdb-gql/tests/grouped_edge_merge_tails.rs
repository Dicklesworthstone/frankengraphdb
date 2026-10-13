//! fgdb-cpny9: grouped plain relationship MERGE carries only direct identity
//! keys. Native final-state bindings feed native grouping, never another MATCH.

use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GraphValue, GraphValueRow};
use fgdb_gql::{
    GlaExecutionStats, GqlExecutionStats, GqlParameters, GqlQueryError, GqlQueryExecution,
    GqlQueryPolicy, GraphEdgeMergeBinding, GraphSetExecutionError, GraphSymbol, GraphSymbolKind,
    PreparedGraphEdgeMergeQuery, PreparedGraphEdgeMergeQueryText,
};
use fgdb_types::{CanonicalScalar, EId, VId};
use std::cell::Cell;

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);
const FIRST: EId = EId((1_u128 << 100) + 7);
const SECOND: EId = EId((2_u128 << 100) + 7);
const PREFIX: &str = "MATCH (a),(b) MERGE (a)-[r:R]->(b)";

type Output = GqlQueryExecution<GraphValueRow>;
type Failure = GqlQueryError<GraphSetExecutionError<()>, ()>;

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Relation, "S") => Some(GraphSymbol::Relation(S)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
        _ => None,
    }
}

fn prepare(tail: &str) -> PreparedGraphEdgeMergeQuery {
    bind(&format!("{PREFIX} {tail}"))
}

fn bind(text: &str) -> PreparedGraphEdgeMergeQuery {
    PreparedGraphEdgeMergeQueryText::prepare(text, R, symbols)
        .expect(text)
        .bind_parameters(&GqlParameters::new())
        .expect(text)
}

fn int(value: i64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(value))
}

fn null() -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Null)
}

fn policy(rows: u64) -> GqlQueryPolicy {
    GqlQueryPolicy::new(1_000, rows, 1_000_000, 100_000)
}

fn edge_row(query: &PreparedGraphEdgeMergeQuery, edge: EId, p: i64, q: i64) -> Vec<GraphValue> {
    query
        .bindings()
        .iter()
        .map(|binding| match *binding {
            GraphEdgeMergeBinding::Edge => GraphValue::Edge(edge),
            GraphEdgeMergeBinding::Property(key) if key == P => int(p),
            GraphEdgeMergeBinding::Property(key) if key == Q => int(q),
            _ => panic!("this fixture has only the merged relationship and its fields"),
        })
        .collect()
}

fn finish(
    query: &PreparedGraphEdgeMergeQuery,
    rows: Vec<Vec<GraphValue>>,
    limit: u64,
) -> Result<Output, Failure> {
    query.finish_governed(
        rows,
        GqlExecutionStats::default(),
        GlaExecutionStats::default(),
        policy(limit),
        || Ok(()),
    )
}

fn values(output: Output) -> Vec<Vec<GraphValue>> {
    output
        .value
        .into_iter()
        .map(|row| row.values().to_vec())
        .collect()
}

#[test]
fn grouped_full_width_identities_do_not_collapse_equal_property_values() {
    let query = prepare(
        "WITH count(*) AS n, r AS edge WHERE edge.p > 0 \
         RETURN edge AS identity, n, edge.p AS p ORDER BY identity",
    );
    assert_eq!(query.columns(), &["identity", "n", "p"]);
    let rows = vec![
        edge_row(&query, SECOND, 7, 0),
        edge_row(&query, FIRST, 7, 0),
        edge_row(&query, FIRST, 7, 0),
    ];
    let result = finish(&query, rows, 2).unwrap();
    assert_eq!(result.rows.result_rows, 2);
    assert_eq!(
        values(result),
        vec![
            vec![GraphValue::Edge(FIRST), int(2), int(7)],
            vec![GraphValue::Edge(SECOND), int(1), int(7)],
        ]
    );
}

#[test]
fn hidden_keys_remap_all_summaries_but_not_their_argument_slots() {
    let query = prepare(
        "WITH sum(r.p) AS total, r AS edge, count(*) AS n, \
         count(DISTINCT r.p) AS distinct_values \
         WHERE edge.q > 0 \
         RETURN total, n, distinct_values, edge.p AS p, edge.q AS q ORDER BY p",
    );
    // p already feeds SUM and COUNT DISTINCT; q needs a new raw input. Both
    // become hidden keys before the summaries, whose output slots must move.
    assert_eq!(
        query
            .bindings()
            .iter()
            .filter(|b| { **b == GraphEdgeMergeBinding::Property(P) })
            .count(),
        1
    );
    let rows = vec![
        edge_row(&query, FIRST, 4, 9),
        edge_row(&query, FIRST, 4, 9),
        edge_row(&query, SECOND, 5, 10),
    ];
    assert_eq!(
        values(finish(&query, rows, 2).unwrap()),
        vec![
            vec![int(8), int(2), int(1), int(4), int(9)],
            vec![int(5), int(1), int(1), int(5), int(10)],
        ]
    );
}

#[test]
fn repeated_identity_aliases_reuse_existing_keys_and_hide_private_columns() {
    let query = prepare(
        "WITH r.p AS known, count(*) AS n, r AS __fg_boundary_0, r AS again \
         WHERE __fg_boundary_0.p > 0 AND again.q > 0 RETURN *",
    );
    assert_eq!(query.columns(), &["known", "n", "__fg_boundary_0", "again"]);
    assert_eq!(query.bindings().len(), 3);
    let row = edge_row(&query, FIRST, 4, 9);
    assert_eq!(
        values(finish(&query, vec![row.clone(), row], 1).unwrap()),
        vec![vec![
            int(4),
            int(2),
            GraphValue::Edge(FIRST),
            GraphValue::Edge(FIRST)
        ],]
    );
}

#[test]
fn actual_undirected_endpoint_and_match_vertex_keep_their_distinct_owners() {
    let query = bind(
        "MATCH (a),(b) MERGE (a)-[r:R]-(b) \
         WITH count(*) AS n, startNode(r) AS source, a AS matched, r AS edge \
         WHERE edge.p > 0 RETURN edge.p AS ep, source.p AS sp, matched.p AS mp, n",
    );
    assert_eq!(
        query.bindings(),
        &[
            GraphEdgeMergeBinding::Endpoint { start: true },
            GraphEdgeMergeBinding::Input(0),
            GraphEdgeMergeBinding::Edge,
            GraphEdgeMergeBinding::Property(P),
            GraphEdgeMergeBinding::EndpointProperty {
                start: true,
                key: P
            },
            GraphEdgeMergeBinding::Input(2),
        ]
    );
    let row = vec![
        GraphValue::Vertex(VId(2)), // Actual edge source, not MATCH a.
        GraphValue::Vertex(VId(1)),
        GraphValue::Edge(FIRST),
        int(7),
        int(20),
        int(10),
    ];
    assert_eq!(
        values(finish(&query, vec![row.clone(), row], 1).unwrap()),
        vec![vec![int(7), int(20), int(10), int(2)],]
    );
}

#[test]
fn grouped_match_relationship_uses_its_own_frozen_selection_field() {
    let query = bind(
        "MATCH (a)-[original:S]->(b) MERGE (a)-[r:R]->(b) \
         WITH original AS old, count(*) AS n RETURN old.p AS p, n",
    );
    assert_eq!(
        query.bindings(),
        &[
            GraphEdgeMergeBinding::Input(2),
            GraphEdgeMergeBinding::Input(3),
        ]
    );
    let row = vec![GraphValue::Edge(FIRST), int(12)];
    assert_eq!(
        values(finish(&query, vec![row.clone(), row], 1).unwrap()),
        vec![vec![int(12), int(2)]]
    );
}

#[test]
fn head_page_precedes_property_unwind_and_final_page_follows_expansion() {
    let query = prepare(
        "WITH r AS edge, count(*) AS n ORDER BY n DESC LIMIT 1 \
         UNWIND [edge.p, edge.p + 1] AS x RETURN x AS value, n ORDER BY value DESC",
    )
    .with_page(1, Some(1));
    let rows = vec![
        edge_row(&query, FIRST, 4, 0),
        edge_row(&query, FIRST, 4, 0),
        edge_row(&query, SECOND, 50, 0),
    ];
    assert_eq!(
        values(finish(&query, rows, 1).unwrap()),
        vec![vec![int(4), int(2)]]
    );
}

#[test]
fn null_keys_empty_groups_and_filtered_groups_do_not_spend_final_row_quota() {
    let query = prepare("WITH r AS edge, count(*) AS n WHERE edge.p IS NULL RETURN n");
    assert_eq!(
        query.bindings(),
        &[
            GraphEdgeMergeBinding::Edge,
            GraphEdgeMergeBinding::Property(P),
        ]
    );
    let row = vec![GraphValue::Edge(FIRST), null()];
    assert_eq!(
        values(finish(&query, vec![row.clone(), row], 1).unwrap()),
        vec![vec![int(2)]]
    );
    assert!(finish(&query, vec![], 0).unwrap().value.is_empty());
    let query = prepare("WITH r AS edge, count(*) AS n WHERE edge.p < 0 RETURN n");
    let rows = vec![
        edge_row(&query, FIRST, 4, 0),
        edge_row(&query, SECOND, 5, 0),
    ];
    assert!(finish(&query, rows, 0).unwrap().value.is_empty());
}

#[test]
fn grouped_tail_rebinds_without_catalog_calls_or_identity_changes() {
    let calls = Cell::new(0);
    let template = PreparedGraphEdgeMergeQueryText::prepare(
        &format!(
            "{PREFIX} WITH r AS edge, count(*) AS n \
                  WHERE edge.p > $minimum RETURN edge.p + $delta AS value, n"
        ),
        R,
        |kind: GraphSymbolKind, name: &str| {
            calls.set(calls.get() + 1);
            symbols(kind, name)
        },
    )
    .unwrap();
    let resolved = calls.get();
    let arguments = |minimum| {
        GqlParameters::new()
            .with_int64("minimum", minimum)
            .unwrap()
            .with_int64("delta", 10)
            .unwrap()
    };
    let shown = template.bind_parameters(&arguments(0)).unwrap();
    let hidden = template.bind_parameters(&arguments(9)).unwrap();
    assert_eq!(calls.get(), resolved);
    assert_ne!(shown.canonical_bytes(), hidden.canonical_bytes());
    assert_eq!(
        values(finish(&shown, vec![edge_row(&shown, FIRST, 4, 0)], 1).unwrap()),
        vec![vec![int(14), int(1)]]
    );
    assert!(
        finish(&hidden, vec![edge_row(&hidden, FIRST, 4, 0)], 0)
            .unwrap()
            .value
            .is_empty()
    );
    assert!(template.bind_parameters(&GqlParameters::new()).is_err());
}

#[test]
fn representatives_dropped_bindings_and_later_scopes_do_not_gain_field_reads() {
    for tail in [
        "WITH min(r) AS edge RETURN edge.p",
        "WITH max(r) AS edge RETURN edge.p",
        "WITH count(*) AS n RETURN r.p",
        "WITH r AS edge, count(*) AS n WITH n RETURN edge.p",
        "WITH r AS edge, count(*) AS n UNWIND [1] AS x RETURN edge.p",
        "WITH r AS edge, count(*) AS n WHERE edge.p > 0 RETURN __fg_boundary_0",
    ] {
        let calls = Cell::new(0);
        let result = PreparedGraphEdgeMergeQueryText::prepare(
            &format!("{PREFIX} {tail}"),
            R,
            |kind: GraphSymbolKind, name: &str| {
                calls.set(calls.get() + 1);
                symbols(kind, name)
            },
        );
        assert!(result.is_err(), "{tail}");
        assert_eq!(calls.get(), 0, "{tail}");
    }
}

#[test]
fn late_field_expression_failure_is_not_hidden_by_limit_zero() {
    let query = prepare("WITH r AS edge, count(*) AS n RETURN 12 / edge.p AS value LIMIT 0");
    let rows = vec![
        edge_row(&query, FIRST, 4, 0),
        edge_row(&query, SECOND, 0, 0),
    ];
    assert!(matches!(
        finish(&query, rows, 0),
        Err(GqlQueryError::Source(_))
    ));
}

#[test]
fn cumulative_work_scratch_and_every_checkpoint_cover_the_grouped_tail() {
    let query = prepare(
        "WITH r AS edge, count(*) AS n \
         UNWIND [edge.p, edge.p + 1] AS x RETURN x, n ORDER BY x",
    );
    let rows = vec![edge_row(&query, FIRST, 4, 0), edge_row(&query, FIRST, 4, 0)];
    let prefix = GlaExecutionStats {
        work_units: 17,
        scratch_entries: 11,
    };
    let mut checkpoints = 0;
    let output = query
        .finish_governed::<(), ()>(
            rows.clone(),
            GqlExecutionStats::default(),
            prefix,
            policy(2),
            || {
                checkpoints += 1;
                Ok(())
            },
        )
        .unwrap();
    assert_eq!(output.value.len(), 2);
    assert_eq!(output.value[0].values(), &[int(4), int(2)]);
    let used = output.evaluator;
    assert!(used.work_units > prefix.work_units);
    assert!(used.scratch_entries > prefix.scratch_entries);
    assert!(checkpoints > 1);
    for (work, scratch, pass) in [
        (used.work_units, used.scratch_entries, true),
        (used.work_units - 1, used.scratch_entries, false),
        (used.work_units, used.scratch_entries - 1, false),
    ] {
        let result = query.finish_governed::<(), ()>(
            rows.clone(),
            GqlExecutionStats::default(),
            prefix,
            GqlQueryPolicy::new(1_000, 2, work, scratch),
            || Ok(()),
        );
        if pass {
            assert_eq!(result.unwrap().value, output.value);
        } else {
            assert!(matches!(result, Err(GqlQueryError::Evaluator(_))));
        }
    }
    for fail_at in 0..checkpoints {
        let mut reached = 0;
        let result = query.finish_governed::<(), usize>(
            rows.clone(),
            GqlExecutionStats::default(),
            prefix,
            policy(2),
            || {
                let current = reached;
                reached += 1;
                if current == fail_at {
                    Err(current)
                } else {
                    Ok(())
                }
            },
        );
        assert!(matches!(result, Err(GqlQueryError::Interrupted(at)) if at == fail_at));
    }
}
