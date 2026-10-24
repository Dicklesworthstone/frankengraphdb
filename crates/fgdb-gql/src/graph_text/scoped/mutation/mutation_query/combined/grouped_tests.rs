//! Actual prepared write projections over final-state occurrence rows.

use crate::algebra::{GraphValue, GraphValueRow};
use crate::{
    GlaExecutionStats, GqlExecutionStats, GqlParameters, GqlQueryError, GqlQueryExecution,
    GqlQueryPolicy, GraphEdgeReturnBinding, GraphSymbol, GraphSymbolKind,
    PreparedGraphEdgeUpsertQuery, PreparedGraphEdgeUpsertQueryText,
};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_types::{CanonicalScalar, EId};

const A: EId = EId((1_u128 << 100) + 1);
const B: EId = EId((1_u128 << 100) + 2);
type Record = (EId, Option<i64>, Option<i64>);
const RECORDS: [Record; 3] = [
    (A, Some(4), Some(9)),
    (B, Some(7), Some(3)),
    (A, Some(4), Some(9)),
];

fn int(value: i64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(value))
}

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(RelationId(1))),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(PropertyKeyId(2))),
        _ => None,
    }
}

fn template(tail: &str) -> PreparedGraphEdgeUpsertQueryText {
    let text = format!("MATCH (a),(b) MERGE (a)-[r:R]->(b) ON MATCH SET r.p=r.p {tail}");
    PreparedGraphEdgeUpsertQueryText::prepare(&text, RelationId(1), symbols).unwrap()
}

fn prepare(tail: &str) -> PreparedGraphEdgeUpsertQuery {
    template(tail)
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}

fn inputs(query: &PreparedGraphEdgeUpsertQuery, records: &[Record]) -> Vec<Vec<GraphValue>> {
    records
        .iter()
        .map(|&(edge, p, q)| {
            query
                .bindings()
                .iter()
                .map(|binding| match binding {
                    GraphEdgeReturnBinding::Edge => GraphValue::Edge(edge),
                    GraphEdgeReturnBinding::Property(key) => {
                        let value = match key.0 {
                            1 => p,
                            2 => q,
                            _ => panic!("unexpected property"),
                        };
                        value.map_or(GraphValue::Scalar(CanonicalScalar::Null), int)
                    }
                })
                .collect()
        })
        .collect()
}

fn policy(rows: u64) -> GqlQueryPolicy {
    GqlQueryPolicy::new(1_000, rows, 1_000_000, 1_000_000)
}

fn execute(
    query: &PreparedGraphEdgeUpsertQuery,
    records: &[Record],
) -> GqlQueryExecution<GraphValueRow> {
    query
        .project_rows_governed::<(), ()>(
            inputs(query, records),
            GqlExecutionStats::default(),
            GlaExecutionStats::default(),
            policy(100),
            || Ok(()),
        )
        .unwrap()
}

fn values(output: GqlQueryExecution<GraphValueRow>) -> Vec<Vec<GraphValue>> {
    output
        .value
        .into_iter()
        .map(|row| row.values().to_vec())
        .collect()
}

#[test]
fn compound_grouping_remaps_repeated_summaries_and_multiple_final_fields() {
    let query = prepare(
        "WITH sum(r.p)+count(*) AS score, r AS edge, r AS twin, size(collect(r.p)) AS n \
         RETURN edge.p AS p, twin.q AS q, score, n ORDER BY p",
    );
    assert_eq!(query.columns(), &["p", "q", "score", "n"]);
    assert_eq!(query.bindings().len(), 3);
    assert_eq!(
        values(execute(&query, &RECORDS)),
        vec![
            vec![int(4), int(9), int(10), int(2)],
            vec![int(7), int(3), int(8), int(1)],
        ]
    );
}

#[test]
fn composite_group_outputs_keep_map_fields_and_lazy_cases_after_hoisting() {
    let query = prepare(
        "WITH r AS edge, {n:count(*), xs:collect(r.p)} AS summary, \
         CASE WHEN count(*) > 1 THEN sum(r.p) ELSE 100/count(*) END AS score \
         RETURN edge.p AS p, summary.n AS n, size(summary.xs) AS size, score ORDER BY p",
    );
    assert_eq!(
        values(execute(&query, &RECORDS)),
        vec![
            vec![int(4), int(2), int(2), int(8)],
            vec![int(7), int(1), int(1), int(100)],
        ]
    );
}

#[test]
fn existing_keys_are_reused_and_star_never_exposes_hidden_group_fields() {
    let query = prepare(
        "WITH r AS edge, r.p AS p, r.p+count(*) AS score \
         WHERE edge.q > 0 RETURN * ORDER BY p",
    );
    assert_eq!(query.columns(), &["edge", "p", "score"]);
    assert_eq!(query.bindings().len(), 3);
    assert_eq!(
        values(execute(&query, &RECORDS)),
        vec![
            vec![GraphValue::Edge(A), int(4), int(6)],
            vec![GraphValue::Edge(B), int(7), int(8)],
        ]
    );
}

#[test]
fn compound_group_page_and_property_unwind_keep_their_written_order() {
    let query = prepare(
        "WITH r AS edge, sum(r.p)+count(*) AS score ORDER BY score DESC LIMIT 1 \
         UNWIND [edge.p,edge.q,score] AS x RETURN sum(x)+count(*) AS total",
    );
    assert_eq!(values(execute(&query, &RECORDS)), vec![vec![int(26)]]);
}

#[test]
fn compound_group_property_rebinding_and_nulls_preserve_input_bags() {
    let template = template(
        "WITH r AS edge, sum(r.p)+count(*)+$extra AS score \
         WHERE edge.q >= $minimum RETURN edge.p AS p, score ORDER BY p",
    );
    let arguments = |minimum| {
        GqlParameters::new()
            .with_int64("extra", 5)
            .unwrap()
            .with_int64("minimum", minimum)
            .unwrap()
    };
    let both = template.bind_parameters(&arguments(3)).unwrap();
    let one = template.bind_parameters(&arguments(9)).unwrap();
    assert_ne!(both.canonical_bytes(), one.canonical_bytes());
    assert_eq!(
        values(execute(&both, &RECORDS)),
        vec![vec![int(4), int(15)], vec![int(7), int(13)]]
    );
    assert_eq!(values(execute(&one, &RECORDS)), vec![vec![int(4), int(15)]]);
    assert!(template.bind_parameters(&GqlParameters::new()).is_err());

    let nullable = prepare("WITH r AS edge, count(*)+1 AS n RETURN edge.p AS p, n");
    assert_eq!(
        values(execute(&nullable, &[(A, None, None)])),
        vec![vec![GraphValue::Scalar(CanonicalScalar::Null), int(2)]]
    );
    assert!(values(execute(&nullable, &[])).is_empty());
}

#[test]
fn hoisted_compound_group_errors_remain_visible_below_limit_zero() {
    let query = prepare("WITH r AS edge, 12/(sum(r.p)-8) AS bad RETURN edge.q AS q, bad LIMIT 0");
    let result = query.project_rows_governed::<(), ()>(
        inputs(&query, &RECORDS),
        GqlExecutionStats::default(),
        GlaExecutionStats::default(),
        policy(0),
        || Ok(()),
    );
    assert!(matches!(result, Err(GqlQueryError::Source(_))));
}

#[test]
fn compound_hidden_groups_keep_cumulative_limits_and_every_checkpoint() {
    let query = prepare(
        "WITH r AS edge, sum(r.p)+count(*) AS score \
         UNWIND [edge.p,edge.q,score] AS x RETURN sum(x)+count(*) AS total",
    );
    let prefix = GlaExecutionStats {
        work_units: 17,
        scratch_entries: 11,
    };
    let mut checkpoints = 0;
    let output = query
        .project_rows_governed::<(), usize>(
            inputs(&query, &RECORDS),
            GqlExecutionStats::default(),
            prefix,
            policy(1),
            || {
                checkpoints += 1;
                Ok(())
            },
        )
        .unwrap();
    assert_eq!(output.value[0].values(), &[int(47)]);
    let work = output.evaluator.work_units;
    let scratch = output.evaluator.scratch_entries;
    assert!(work > prefix.work_units && scratch > prefix.scratch_entries);
    let exact = query
        .project_rows_governed::<(), ()>(
            inputs(&query, &RECORDS),
            GqlExecutionStats::default(),
            prefix,
            GqlQueryPolicy::new(1_000, 1, work, scratch),
            || Ok(()),
        )
        .unwrap();
    assert_eq!(output.value, exact.value);
    for (work, scratch) in [(work - 1, scratch), (work, scratch - 1)] {
        let result = query.project_rows_governed::<(), ()>(
            inputs(&query, &RECORDS),
            GqlExecutionStats::default(),
            prefix,
            GqlQueryPolicy::new(1_000, 1, work, scratch),
            || Ok(()),
        );
        assert!(matches!(result, Err(GqlQueryError::Evaluator(_))));
    }
    for fail_at in 0..checkpoints {
        let mut position = 0;
        let result = query.project_rows_governed::<(), usize>(
            inputs(&query, &RECORDS),
            GqlExecutionStats::default(),
            prefix,
            policy(1),
            || {
                let at = position;
                position += 1;
                if at == fail_at { Err(at) } else { Ok(()) }
            },
        );
        assert!(matches!(result, Err(GqlQueryError::Interrupted(at)) if at == fail_at));
    }
}
