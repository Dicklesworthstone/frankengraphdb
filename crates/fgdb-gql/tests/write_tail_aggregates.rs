//! Native post-write row stages: no graph source or second MATCH is supplied
//! to projection. The host-owned final relationship cells feed the real
//! projection, grouping, sorting and pagination operators.

use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GraphValue, GraphValueRow};
use fgdb_gql::{
    GlaExecutionStats, GqlExecutionStats, GqlParameters, GqlQueryError, GqlQueryExecution,
    GqlQueryPolicy, GraphEdgeReturnBinding, GraphSetExecutionError, GraphSymbol, GraphSymbolKind,
    PreparedGraphEdgeUpsertQuery, PreparedGraphEdgeUpsertQueryText,
};
use fgdb_types::{CanonicalScalar, EId};

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const EDGE: EId = EId((1_u128 << 100) + 7);
const PREFIX: &str = "MATCH (a),(b) MERGE (a)-[r:R]->(b) \
    ON CREATE SET r.p=2 ON MATCH SET r.p=r.p+1 SET r.p=r.p*2";

type Output = GqlQueryExecution<GraphValueRow>;
type Failure = GqlQueryError<GraphSetExecutionError<()>, ()>;

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    }
}

fn int(value: i64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(value))
}

fn prepare(tail: &str) -> PreparedGraphEdgeUpsertQuery {
    PreparedGraphEdgeUpsertQueryText::prepare(&format!("{PREFIX} {tail}"), R, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}

fn policy(rows: u64) -> GqlQueryPolicy {
    GqlQueryPolicy::new(1_000, rows, 1_000_000, 100_000)
}

fn input(query: &PreparedGraphEdgeUpsertQuery, value: i64) -> Vec<GraphValue> {
    query
        .bindings()
        .iter()
        .map(|binding| match binding {
            GraphEdgeReturnBinding::Edge => GraphValue::Edge(EDGE),
            GraphEdgeReturnBinding::Property(key) => {
                assert_eq!(*key, P);
                int(value)
            }
        })
        .collect()
}

fn finish(
    query: &PreparedGraphEdgeUpsertQuery,
    value: Option<i64>,
    rows: u64,
) -> Result<Output, Failure> {
    query.project_governed(
        value.map(|value| input(query, value)),
        GqlExecutionStats::default(),
        GlaExecutionStats::default(),
        policy(rows),
        || Ok(()),
    )
}

fn values(output: &Output) -> Vec<Vec<GraphValue>> {
    output
        .value
        .iter()
        .map(|row| row.values().to_vec())
        .collect()
}

#[test]
fn terminal_aggregate_tail_groups_before_its_order_and_page() {
    let query = prepare(
        "WITH r.p AS p UNWIND [p,p+1,p+1] AS x \
         RETURN x AS key, count(*) AS n ORDER BY key DESC SKIP 1 LIMIT 1",
    );
    let output = finish(&query, Some(4), 1).unwrap();
    assert_eq!(query.columns(), &["key", "n"]);
    assert_eq!(values(&output), vec![vec![int(4), int(1)]]);
    assert_eq!(output.rows.result_rows, 1);
}

#[test]
fn terminal_aggregate_tail_names_unaliased_and_combined_items() {
    let query = prepare(
        "WITH r.p AS p UNWIND [p,p+1] AS x \
         RETURN count(*), sum(x)+count(*)",
    );
    assert_eq!(query.columns(), &["count(*)", "sum(x)+count(*)"]);
    let output = finish(&query, Some(4), 1).unwrap();
    assert_eq!(values(&output), vec![vec![int(2), int(11)]]);
}

#[test]
fn terminal_aggregate_tail_reads_the_carried_edges_final_properties() {
    let query = prepare("WITH r AS edge RETURN count(edge.p) AS n, sum(edge.p) AS total");
    assert_eq!(
        query.bindings(),
        &[
            GraphEdgeReturnBinding::Edge,
            GraphEdgeReturnBinding::Property(P)
        ]
    );
    let output = finish(&query, Some(4), 1).unwrap();
    assert_eq!(values(&output), vec![vec![int(1), int(4)]]);
}

#[test]
fn terminal_aggregate_tail_distinguishes_empty_grouped_and_global_input() {
    let global = prepare("WITH r.p AS p WHERE p < 0 RETURN count(*) AS n");
    let output = finish(&global, Some(4), 1).unwrap();
    assert_eq!(values(&output), vec![vec![int(0)]]);
    assert_eq!(output.rows.result_rows, 1);
    assert!(finish(&global, Some(4), 0).is_err());
    assert_eq!(
        values(&finish(&global, None, 1).unwrap()),
        vec![vec![int(0)]]
    );

    let grouped = prepare("WITH r.p AS p WHERE p < 0 RETURN p, count(*) AS n");
    let output = finish(&grouped, Some(4), 0).unwrap();
    assert!(output.value.is_empty());
    assert_eq!(output.rows.result_rows, 0);
    assert!(finish(&grouped, None, 0).unwrap().value.is_empty());
}

#[test]
fn terminal_aggregate_tail_limit_zero_does_not_hide_bad_input() {
    let query = prepare("WITH r.p AS p UNWIND [p,0] AS x RETURN sum(12/x) AS n LIMIT 0");
    assert!(matches!(
        finish(&query, Some(4), 0),
        Err(GqlQueryError::Source(_))
    ));
}

#[test]
fn terminal_aggregate_tail_keeps_cumulative_limits_and_cancellation() {
    for tail in [
        "WITH r.p AS p UNWIND [p,p,p+1] AS x RETURN count(*) AS n, sum(x) AS total",
        "WITH r AS edge UNWIND [edge.p,edge.p,edge.p+1] AS x \
         RETURN count(*) AS n, sum(x) AS total",
    ] {
        let query = prepare(tail);
        let prefix = GlaExecutionStats {
            work_units: 17,
            scratch_entries: 11,
        };
        let mut checkpoints = 0_usize;
        let output = query
            .project_governed::<(), usize>(
                Some(input(&query, 4)),
                GqlExecutionStats::default(),
                prefix,
                policy(1),
                || {
                    checkpoints += 1;
                    Ok(())
                },
            )
            .unwrap();
        assert_eq!(values(&output), vec![vec![int(3), int(13)]]);
        let work = output.evaluator.work_units;
        let scratch = output.evaluator.scratch_entries;
        assert!(work > prefix.work_units);
        assert!(scratch > prefix.scratch_entries);
        let exact = query
            .project_governed::<(), ()>(
                Some(input(&query, 4)),
                GqlExecutionStats::default(),
                prefix,
                GqlQueryPolicy::new(1_000, 1, work, scratch),
                || Ok(()),
            )
            .unwrap();
        assert_eq!(output.value, exact.value);
        for (work_limit, scratch_limit) in [(work - 1, scratch), (work, scratch - 1)] {
            assert!(matches!(
                query.project_governed::<(), ()>(
                    Some(input(&query, 4)),
                    GqlExecutionStats::default(),
                    prefix,
                    GqlQueryPolicy::new(1_000, 1, work_limit, scratch_limit),
                    || Ok(()),
                ),
                Err(GqlQueryError::Evaluator(_))
            ));
        }
        assert!(checkpoints > 1);
        for fail_at in 0..checkpoints {
            let mut at = 0;
            let result = query.project_governed::<(), usize>(
                Some(input(&query, 4)),
                GqlExecutionStats::default(),
                prefix,
                policy(1),
                || {
                    let current = at;
                    at += 1;
                    if current == fail_at {
                        Err(fail_at)
                    } else {
                        Ok(())
                    }
                },
            );
            assert!(matches!(result, Err(GqlQueryError::Interrupted(index)) if index == fail_at));
        }
    }
}

#[test]
fn text_predicate_with_does_not_end_the_write_property_scope() {
    for predicate in [
        "'alphabet' STARTS WITH 'alpha'",
        "'alphabet' ENDS WITH 'bet'",
    ] {
        let query = prepare(&format!(
            "WITH r AS edge WHERE {predicate} RETURN edge.p AS p"
        ));
        let output = finish(&query, Some(14), 1).unwrap();
        assert_eq!(query.columns(), &["p"]);
        assert_eq!(values(&output), vec![vec![int(14)]]);

        let star = prepare(&format!(
            "WITH r AS edge WHERE {predicate} AND edge.p > 0 RETURN *"
        ));
        assert_eq!(star.columns(), &["edge"]);
        assert_eq!(
            values(&finish(&star, Some(14), 1).unwrap()),
            vec![vec![GraphValue::Edge(EDGE)]]
        );
    }
}

#[test]
fn a_real_with_still_ends_the_write_property_scope() {
    let text = format!(
        "{PREFIX} WITH r AS edge WHERE 'alphabet' STARTS WITH 'alpha' \
         WITH 0 AS edge RETURN edge.p AS p"
    );
    let result = PreparedGraphEdgeUpsertQueryText::prepare(&text, R, symbols)
        .and_then(|template| template.bind_parameters(&GqlParameters::new()));
    assert!(result.is_err());
}

#[test]
fn first_unwind_expression_reads_final_properties_through_the_carried_alias() {
    let query = prepare("WITH r AS edge UNWIND [edge.p,edge.p+1] AS x RETURN x ORDER BY x");
    assert_eq!(
        query.bindings(),
        &[
            GraphEdgeReturnBinding::Edge,
            GraphEdgeReturnBinding::Property(P)
        ]
    );
    assert_eq!(
        values(&finish(&query, Some(4), 2).unwrap()),
        vec![vec![int(4)], vec![int(5)]]
    );
    assert!(finish(&query, Some(4), 1).is_err());

    let nested = prepare(
        "WITH r AS edge \
         UNWIND [v IN [edge.p,edge.p+1] WHERE v > 4 | v*2] AS x RETURN x",
    );
    assert_eq!(
        values(&finish(&nested, Some(4), 1).unwrap()),
        vec![vec![int(10)]]
    );
}

#[test]
fn filtered_unwind_properties_and_parameters_share_one_prepared_statement() {
    let text = format!(
        "{PREFIX} WITH r AS edge \
         WHERE 'alphabet' STARTS WITH 'alpha' AND edge.p >= $minimum \
         UNWIND [edge.p,edge.p+$extra] AS x RETURN x ORDER BY x"
    );
    let template = PreparedGraphEdgeUpsertQueryText::prepare(&text, R, symbols).unwrap();
    assert_eq!(template.parameter_schema().len(), 2);
    let arguments = |minimum| {
        GqlParameters::new()
            .with_int64("minimum", minimum)
            .unwrap()
            .with_int64("extra", 3)
            .unwrap()
    };
    let admitted = template.bind_parameters(&arguments(4)).unwrap();
    let filtered = template.bind_parameters(&arguments(5)).unwrap();
    assert_eq!(
        values(&finish(&admitted, Some(4), 2).unwrap()),
        vec![vec![int(4)], vec![int(7)]]
    );
    assert!(finish(&filtered, Some(4), 0).unwrap().value.is_empty());
    assert_ne!(admitted.canonical_bytes(), filtered.canonical_bytes());
    assert!(template.bind_parameters(&GqlParameters::new()).is_err());
}

#[test]
fn unwind_star_never_exposes_hidden_cells_or_shadows_a_reserved_looking_alias() {
    for alias in ["edge", "__fg_boundary_0"] {
        let query = prepare(&format!(
            "WITH r AS {alias} UNWIND [{alias}.p] AS x RETURN *"
        ));
        assert_eq!(query.columns(), &[alias, "x"]);
        assert_eq!(
            values(&finish(&query, Some(4), 1).unwrap()),
            vec![vec![GraphValue::Edge(EDGE), int(4)]]
        );
        assert!(finish(&query, None, 0).unwrap().value.is_empty());
    }
}

#[test]
fn unwind_keeps_null_properties_distinct_from_absent_input_rows() {
    let query = prepare(
        "WITH r AS edge UNWIND [edge.p,7] AS x \
         RETURN count(x) AS present, count(*) AS occurrences",
    );
    let row = query
        .bindings()
        .iter()
        .map(|binding| match binding {
            GraphEdgeReturnBinding::Edge => GraphValue::Edge(EDGE),
            GraphEdgeReturnBinding::Property(key) => {
                assert_eq!(*key, P);
                GraphValue::Scalar(CanonicalScalar::Null)
            }
        })
        .collect();
    let output = query
        .project_governed::<(), ()>(
            Some(row),
            GqlExecutionStats::default(),
            GlaExecutionStats::default(),
            policy(1),
            || Ok(()),
        )
        .unwrap();
    assert_eq!(values(&output), vec![vec![int(1), int(2)]]);
    assert_eq!(
        values(&finish(&query, None, 1).unwrap()),
        vec![vec![int(0), int(0)]]
    );
}

#[test]
fn unwind_preserves_occurrences_unless_the_with_head_is_distinct() {
    for (quantifier, count, total) in [("", 4, 16), ("DISTINCT ", 2, 8)] {
        let query = prepare(&format!(
            "WITH {quantifier}r AS edge UNWIND [edge.p,edge.p] AS x \
             RETURN count(*) AS n, sum(x) AS total"
        ));
        let row = input(&query, 4);
        let output = query
            .project_rows_governed::<(), ()>(
                vec![row.clone(), row],
                GqlExecutionStats::default(),
                GlaExecutionStats::default(),
                policy(1),
                || Ok(()),
            )
            .unwrap();
        assert_eq!(values(&output), vec![vec![int(count), int(total)]]);
        assert_eq!(output.rows.result_rows, 1);
    }
}

#[test]
fn unwind_property_reads_stop_at_its_as_and_do_not_revive_dropped_bindings() {
    for tail in [
        "WITH r AS edge UNWIND [edge.p] AS x RETURN edge.p AS p",
        "WITH r AS edge UNWIND [1] AS x RETURN edge.p AS p",
        "WITH r AS edge UNWIND [1] AS x UNWIND [edge.p] AS y RETURN y",
        "WITH 0 AS edge UNWIND [edge.p] AS x RETURN x",
        "WITH r AS edge WITH edge UNWIND [edge.p] AS x RETURN x",
    ] {
        let text = format!("{PREFIX} {tail}");
        let result = PreparedGraphEdgeUpsertQueryText::prepare(&text, R, symbols)
            .and_then(|template| template.bind_parameters(&GqlParameters::new()));
        assert!(result.is_err(), "{tail}");
    }
    // A caller that needs a later scalar read can explicitly retain it.
    let query = prepare("WITH r.p AS p UNWIND [p] AS x UNWIND [p+x] AS y RETURN p, y");
    assert_eq!(
        values(&finish(&query, Some(4), 1).unwrap()),
        vec![vec![int(4), int(8)]]
    );
}

#[test]
fn unwind_final_field_errors_are_not_suppressed_by_limit_zero() {
    for tail in [
        "WITH r AS edge UNWIND [12/(edge.p-4)] AS x RETURN x LIMIT 0",
        "WITH r AS edge UNWIND [edge.p,0] AS x RETURN sum(12/x) AS n LIMIT 0",
    ] {
        let query = prepare(tail);
        assert!(matches!(
            finish(&query, Some(4), 0),
            Err(GqlQueryError::Source(_))
        ));
    }
}
